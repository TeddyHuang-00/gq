use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use nom::{
    Parser,
    bytes::complete::{tag, take_till1, take_until},
    character::complete::{line_ending, multispace0, space0, space1, u32},
    combinator::{all_consuming, map},
    multi::separated_list0,
    sequence::{delimited, preceded, separated_pair},
};

/// Number of compute apps on one card.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AppCount {
    /// Apps of every owner.
    pub any: usize,
    /// Apps not owned by the current user.
    pub foreign: usize,
}

/// One `GPU-uuid, pid` row, without its line ending.
fn compute_app_row(input: &str) -> nom::IResult<&str, (String, u32)> {
    map(
        separated_pair(
            take_till1(|ch: char| ch == ','),
            tag(","),
            preceded(space0::<&str, nom::error::Error<&str>>, u32),
        ),
        |(uuid, pid)| (uuid.trim().to_owned(), pid),
    )
    .parse(input)
}

/// Parses `--query-compute-apps=gpu_uuid,pid` output into `(UUID, pid)` rows.
///
/// Empty output means no compute app is running and is not an error.
pub(super) fn parse_compute_apps(output: &str) -> Result<Vec<(String, u32)>> {
    let (_, rows) = all_consuming(delimited(
        multispace0,
        separated_list0(delimited(space0, line_ending, multispace0), compute_app_row),
        multispace0,
    ))
    .parse_complete(output)
    .map_err(|err| anyhow::anyhow!("{err}"))?;
    Ok(rows)
}

/// Counts compute apps per card. An app whose owner cannot be read counts as
/// foreign, so a card never looks available just because ownership is unknown.
pub(super) fn count_apps(
    rows: &[(String, u32)],
    uuid_to_index: &BTreeMap<String, usize>,
    current_uid: u32,
    uid_of: impl Fn(u32) -> Option<u32>,
) -> BTreeMap<usize, AppCount> {
    let mut counts: BTreeMap<usize, AppCount> = BTreeMap::new();
    // Snapshot-scoped only: pids are reused across rounds and /proc visibility
    // can change, so negative results are cached just for this call.
    let mut owners: HashMap<u32, Option<u32>> = HashMap::new();
    for (uuid, pid) in rows {
        let Some(&index) = uuid_to_index.get(uuid) else {
            log::debug!("ignoring compute app pid {pid} on unknown GPU {uuid}");
            continue;
        };
        let count = counts.entry(index).or_default();
        count.any += 1;
        if *owners.entry(*pid).or_insert_with(|| uid_of(*pid)) != Some(current_uid) {
            count.foreign += 1;
        }
    }
    counts
}

/// Real UID of `pid` from `/proc/<pid>/status`; `None` when the process is
/// gone or the file is unreadable (for example under `hidepid`).
pub(super) fn proc_uid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    parse_status_uid(&status)
}

/// Real UID from the first field of a `/proc/<pid>/status` `Uid:` line.
fn parse_status_uid(status: &str) -> Option<u32> {
    let (_, uid) = preceded(
        take_until::<&str, &str, nom::error::Error<&str>>("Uid:"),
        preceded(tag("Uid:"), preceded(space1, u32)),
    )
    .parse(status)
    .ok()?;
    Some(uid)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use rustix::process::getuid;

    use super::*;

    #[test]
    fn compute_apps_parser_accepts_empty_and_multiple_rows_per_gpu() {
        assert_eq!(parse_compute_apps("").ok(), Some(Vec::new()));
        assert_eq!(parse_compute_apps("\n").ok(), Some(Vec::new()));
        assert_eq!(parse_compute_apps("  \n \n").ok(), Some(Vec::new()));

        let rows = parse_compute_apps("GPU-a, 100\nGPU-a, 200\nGPU-b, 300\n")
            .unwrap_or_else(|error| panic!("valid app output failed: {error}"));
        assert_eq!(
            rows,
            vec![
                ("GPU-a".to_owned(), 100),
                ("GPU-a".to_owned(), 200),
                ("GPU-b".to_owned(), 300),
            ]
        );
        // No trailing newline, blank lines, and padded fields are all valid.
        assert_eq!(
            parse_compute_apps(" GPU-a , 100 \n\nGPU-b,200").ok(),
            Some(vec![("GPU-a".to_owned(), 100), ("GPU-b".to_owned(), 200),])
        );
        assert!(parse_compute_apps("GPU-a\n").is_err());
        assert!(parse_compute_apps("GPU-a, not-a-pid\n").is_err());
        assert!(parse_compute_apps("GPU-a, 100\ntrailing garbage\n").is_err());
    }

    #[test]
    fn app_counts_distinguish_own_foreign_and_unknown_owners() {
        let uuids = BTreeMap::from([("GPU-a".to_owned(), 0), ("GPU-b".to_owned(), 1)]);
        let rows = vec![
            ("GPU-a".to_owned(), 10),
            ("GPU-a".to_owned(), 11),
            ("GPU-a".to_owned(), 12),
            ("GPU-b".to_owned(), 13),
            ("GPU-gone".to_owned(), 14),
        ];
        // 10 is the current user; 11 belongs to someone else; 12 and 13 cannot
        // be looked up and must count as foreign; 14 is on an absent card.
        let counts = count_apps(&rows, &uuids, 1000, |pid| match pid {
            10 => Some(1000),
            11 => Some(1001),
            _ => None,
        });

        assert_eq!(
            counts.get(&0).copied().unwrap_or_default(),
            AppCount { any: 3, foreign: 2 }
        );
        assert_eq!(
            counts.get(&1).copied().unwrap_or_default(),
            AppCount { any: 1, foreign: 1 }
        );
        assert_eq!(counts.len(), 2);
    }

    #[test]
    fn app_counts_resolve_each_pid_once_per_snapshot() {
        let uuids = BTreeMap::from([("GPU-a".to_owned(), 0), ("GPU-b".to_owned(), 1)]);
        // pid 10 spans both cards; pid 11 belongs to someone else. A
        // multi-GPU process must not repeat the same /proc read.
        let rows = vec![
            ("GPU-a".to_owned(), 10),
            ("GPU-b".to_owned(), 10),
            ("GPU-b".to_owned(), 11),
        ];
        let calls = Cell::new(0);
        let counts = count_apps(&rows, &uuids, 1000, |pid| {
            calls.set(calls.get() + 1);
            match pid {
                10 => Some(1000),
                _ => Some(1001),
            }
        });

        assert_eq!(calls.get(), 2);
        assert_eq!(
            counts.get(&0).copied().unwrap_or_default(),
            AppCount { any: 1, foreign: 0 }
        );
        assert_eq!(
            counts.get(&1).copied().unwrap_or_default(),
            AppCount { any: 2, foreign: 1 }
        );
    }

    #[test]
    fn status_uid_parser_reads_the_real_uid_field() {
        assert_eq!(
            parse_status_uid("Name:\tbash\nUid:\t1000\t1001\t1002\t1003\n"),
            Some(1000)
        );
        assert_eq!(
            parse_status_uid("Uid:\t1000\t1001\t1002\t1003\n"),
            Some(1000)
        );
        assert_eq!(parse_status_uid("Name:\tbash\n"), None);
        assert_eq!(parse_status_uid("Uid:\tnot-a-uid\t1001\n"), None);
    }

    #[test]
    fn proc_uid_reads_the_real_uid_of_a_live_process() {
        assert_eq!(proc_uid(std::process::id()), Some(getuid().as_raw()));
        assert_eq!(proc_uid(u32::MAX), None);
    }
}

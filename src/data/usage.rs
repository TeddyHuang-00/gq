use std::{
    collections::BTreeMap,
    fmt,
    sync::Once,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use nom::{
    Parser,
    bytes::complete::{tag, take_till},
    character::complete::{line_ending, u8, u32, usize},
    combinator::{all_consuming, map, opt},
    multi::separated_list1,
    sequence::{separated_pair, terminated},
};
use rustix::process::getuid;

use super::{
    apps::{AppCount, count_apps, parse_compute_apps, proc_uid},
    probe::{ensure_before_deadline, smi, smi_allow_empty},
};

/// Instantaneous usage statistics
#[derive(Clone, Copy, Debug)]
pub struct UsageInfo {
    pub memory_total_mib: f64,
    pub memory_free_mib: f64,
    pub memory_used_mib: f64,
    pub utilization_percent: f64,
    /// Compute apps seen in one snapshot during the probe round.
    pub apps: AppCount,
}

pub struct AllUsage {
    cards: BTreeMap<usize, UsageInfo>,
    /// Card UUID to index, used to attribute compute apps to cards.
    uuids: BTreeMap<String, usize>,
}

/// Card sets differed between probes; the caller may retry the round.
#[derive(Debug)]
struct InconsistentCardSet {
    expected: Vec<usize>,
    found: Vec<usize>,
}

impl fmt::Display for InconsistentCardSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Inconsistent card sets across probes: expected {:?}, found {:?}",
            self.expected, self.found
        )
    }
}

impl std::error::Error for InconsistentCardSet {}

/// The UUID column of a usage row: everything up to the next comma.
fn uuid_field(input: &str) -> nom::IResult<&str, &str> {
    take_till(|ch: char| ch == ',').parse(input)
}

impl AllUsage {
    /// Parses the command output from query.
    fn parse_command_output<S>(input: S) -> Result<Self>
    where
        S: AsRef<str>,
    {
        let input = input.as_ref();
        let (_, stats) = all_consuming(terminated(
            separated_list1(
                line_ending::<&str, nom::error::Error<&str>>,
                separated_pair(
                    usize,
                    tag(", "),
                    map(
                        (
                            terminated(uuid_field, tag(", ")),
                            terminated(u32, tag(" MiB, ")),
                            terminated(u32, tag(" MiB, ")),
                            terminated(u32, tag(" MiB, ")),
                            terminated(u8, tag(" %")),
                        ),
                        |(uuid, mem_tot, mem_free, mem_used, util)| {
                            (
                                uuid.to_owned(),
                                UsageInfo {
                                    memory_total_mib: f64::from(mem_tot),
                                    memory_free_mib: f64::from(mem_free),
                                    memory_used_mib: f64::from(mem_used),
                                    utilization_percent: f64::from(util),
                                    apps: AppCount::default(),
                                },
                            )
                        },
                    ),
                ),
            ),
            opt(line_ending),
        ))
        .parse_complete(input)
        .map_err(|err| anyhow::anyhow!("{err}"))?;
        let mut cards = BTreeMap::new();
        let mut uuids = BTreeMap::new();
        for (index, (uuid, info)) in stats {
            if cards.insert(index, info).is_some() {
                anyhow::bail!("duplicate GPU index {index} in nvidia-smi output");
            }
            uuids.insert(uuid.trim().to_owned(), index);
        }
        Ok(Self { cards, uuids })
    }

    /// Aggregates parsed probe results. Kept separate from command execution so
    /// callers and tests can supply captured `nvidia-smi` output.
    fn from_probe_outputs<I, S>(outputs: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let probes = outputs
            .into_iter()
            .map(Self::parse_command_output)
            .collect::<Result<Vec<_>>>()?;
        let Some(first) = probes.first() else {
            anyhow::bail!("no probe outputs");
        };
        let expected = first.cards.keys().copied().collect::<Vec<_>>();
        for probe in &probes[1..] {
            let found = probe.cards.keys().copied().collect::<Vec<_>>();
            if found != expected {
                return Err(anyhow::Error::new(InconsistentCardSet { expected, found }));
            }
        }
        let probe_count = f64::from(u32::try_from(probes.len())?);
        let mut aggregate = BTreeMap::new();
        for (&index, first_stats) in &first.cards {
            let mut stats = *first_stats;
            for probe in &probes[1..] {
                let next = probe
                    .cards
                    .get(&index)
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("GPU{index} missing from probe"))?;
                if (next.memory_total_mib - stats.memory_total_mib).abs() > f64::EPSILON {
                    anyhow::bail!(
                        "GPU{index} total memory changed between probes: {} MiB then {} MiB",
                        stats.memory_total_mib,
                        next.memory_total_mib
                    );
                }
                stats.memory_used_mib = stats.memory_used_mib.max(next.memory_used_mib);
                stats.memory_free_mib = stats.memory_free_mib.min(next.memory_free_mib);
                stats.utilization_percent += next.utilization_percent;
            }
            stats.utilization_percent /= probe_count;
            aggregate.insert(index, stats);
        }
        Ok(Self {
            cards: aggregate,
            uuids: first.uuids.clone(),
        })
    }

    /// Samples `probe_count` outputs one second apart.
    fn sample(probe_count: u16, deadline: Option<Instant>) -> Result<Vec<String>> {
        let mut outputs = Vec::with_capacity(usize::from(probe_count));
        for idx in 0..probe_count {
            if idx > 0 {
                ensure_before_deadline(deadline)?;
                // Cap the inter-probe sleep so a short --timeout is not
                // overshot by a full second before the next check.
                let sleep = deadline.map_or(Duration::from_secs(1), |deadline| {
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_secs(1))
                });
                if sleep.is_zero() {
                    anyhow::bail!("the probe deadline passed before the next probe could start");
                }
                std::thread::sleep(sleep);
            }
            ensure_before_deadline(deadline)?;
            outputs.push(smi(
                &[
                    "--query-gpu=index,uuid,memory.total,memory.free,memory.used,utilization.gpu",
                    "--format=noheader",
                ],
                deadline,
            )?);
        }
        Ok(outputs)
    }

    /// Runs `nvidia-smi` to get usage info and aggregate over time.
    pub fn query(probe_count: u16) -> Result<Self> {
        Self::query_with_deadline(probe_count, None)
    }

    /// Like [`Self::query`], but stops before `deadline` passes.
    pub fn query_with_deadline(probe_count: u16, deadline: Option<Instant>) -> Result<Self> {
        static PROBE_COUNT_WARNING: Once = Once::new();
        if probe_count > 60 {
            PROBE_COUNT_WARNING.call_once(|| {
                log::warn!(
                    "`--probe-count {probe_count}` takes about {} seconds; \
                    10 or fewer probes is recommended",
                    probe_count.saturating_sub(1)
                );
            });
        }
        let mut usage = match Self::from_probe_outputs(Self::sample(probe_count, deadline)?) {
            Ok(usage) => usage,
            Err(error) if error.is::<InconsistentCardSet>() => {
                log::warn!("discarding probe round ({error}); retrying");
                // A discarded round does not consume a wait attempt; one retry
                // is a bound against an infinite loop, not an extra attempt.
                Self::from_probe_outputs(Self::sample(probe_count, deadline)?)?
            }
            Err(error) => return Err(error),
        };
        let rows = parse_compute_apps(
            &smi_allow_empty(
                &["--query-compute-apps=gpu_uuid,pid", "--format=noheader"],
                deadline,
            )
            .context("cannot read compute apps from nvidia-smi")?,
        )
        .context("cannot parse compute apps from nvidia-smi")?;
        usage.attach_apps(&rows, getuid().as_raw(), proc_uid);
        Ok(usage)
    }

    /// Attaches one app snapshot to the already aggregated round.
    fn attach_apps(
        &mut self,
        rows: &[(String, u32)],
        current_uid: u32,
        uid_of: impl Fn(u32) -> Option<u32>,
    ) {
        let counts = count_apps(rows, &self.uuids, current_uid, uid_of);
        for (&index, stats) in &mut self.cards {
            stats.apps = counts.get(&index).copied().unwrap_or_default();
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (usize, UsageInfo)> + '_ {
        self.cards.iter().map(|(&index, &usage)| (index, usage))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USAGE_EXAMPLE: &str =
        "0, GPU-00000000-0000-0000-0000-000000000000, 49140 MiB, 48536 MiB, 5 MiB, 0 %
1, GPU-00000001-0000-0000-0000-000000000000, 49140 MiB, 43612 MiB, 4928 MiB, 53 %
2, GPU-00000002-0000-0000-0000-000000000000, 49140 MiB, 43612 MiB, 4928 MiB, 72 %
3, GPU-00000003-0000-0000-0000-000000000000, 49140 MiB, 44621 MiB, 3920 MiB, 96 %
4, GPU-00000004-0000-0000-0000-000000000000, 49140 MiB, 48536 MiB, 5 MiB, 0 %
5, GPU-00000005-0000-0000-0000-000000000000, 49140 MiB, 42548 MiB, 5992 MiB, 61 %
6, GPU-00000006-0000-0000-0000-000000000000, 49140 MiB, 42644 MiB, 5896 MiB, 44 %
7, GPU-00000007-0000-0000-0000-000000000000, 49140 MiB, 46844 MiB, 1696 MiB, 37 %
";

    #[test]
    fn usage_parser_and_probe_averaging_accept_example_command_output() {
        let usage_result = AllUsage::from_probe_outputs([USAGE_EXAMPLE, USAGE_EXAMPLE]);
        assert!(usage_result.is_ok());
        let usage =
            usage_result.unwrap_or_else(|error| panic!("valid usage output failed: {error}"));

        assert!(usage.cards.contains_key(&0));
        let gpu0 = usage
            .cards
            .get(&0)
            .unwrap_or_else(|| panic!("GPU 0 missing"));
        assert_eq!(gpu0.memory_total_mib, 49_140.0);
        assert_eq!(gpu0.memory_free_mib, 48_536.0);
        assert_eq!(gpu0.memory_used_mib, 5.0);
        assert_eq!(gpu0.utilization_percent, 0.0);
        assert_eq!(gpu0.apps, AppCount::default());
        assert!(usage.cards.contains_key(&1));
        let gpu1 = usage
            .cards
            .get(&1)
            .unwrap_or_else(|| panic!("GPU 1 missing"));
        assert_eq!(gpu1.memory_free_mib, 43_612.0);
        assert_eq!(gpu1.utilization_percent, 53.0);
    }

    #[test]
    fn usage_aggregates_used_max_free_min_and_utilization_mean() {
        let first = "0, GPU-a, 1000 MiB, 900 MiB, 100 MiB, 10 %\n";
        let second = "0, GPU-a, 1000 MiB, 800 MiB, 200 MiB, 30 %\n";
        let usage_result = AllUsage::from_probe_outputs([first, second]);
        assert!(usage_result.is_ok());
        let usage =
            usage_result.unwrap_or_else(|error| panic!("valid usage output failed: {error}"));

        assert!(usage.cards.contains_key(&0));
        let gpu0 = usage
            .cards
            .get(&0)
            .unwrap_or_else(|| panic!("GPU 0 missing"));
        assert_eq!(gpu0.memory_total_mib, 1000.0);
        assert_eq!(gpu0.memory_used_mib, 200.0);
        assert_eq!(gpu0.memory_free_mib, 800.0);
        assert_eq!(gpu0.utilization_percent, 20.0);
    }

    #[test]
    fn usage_rejects_probes_with_different_cards_or_memory_totals() {
        let one_card = "0, GPU-a, 1000 MiB, 900 MiB, 100 MiB, 10 %\n";
        let two_cards = "0, GPU-a, 1000 MiB, 900 MiB, 100 MiB, 10 %\n\
                         1, GPU-b, 1000 MiB, 900 MiB, 100 MiB, 10 %\n";
        let changed_total = "0, GPU-a, 2000 MiB, 900 MiB, 100 MiB, 10 %\n";

        let mismatch_result = AllUsage::from_probe_outputs([one_card, two_cards]);
        assert!(mismatch_result.is_err());
        let mismatch = mismatch_result
            .err()
            .unwrap_or_else(|| panic!("expected card-set error"));
        assert!(mismatch.is::<InconsistentCardSet>());
        let changed_result = AllUsage::from_probe_outputs([one_card, changed_total]);
        assert!(changed_result.is_err());
        let changed = changed_result
            .err()
            .unwrap_or_else(|| panic!("expected changed-total error"));
        assert!(!changed.is::<InconsistentCardSet>());
    }

    #[test]
    fn usage_rejects_duplicate_gpu_index_in_one_output() {
        let duplicate = "0, GPU-a, 1000 MiB, 900 MiB, 100 MiB, 10 %\n\
                         0, GPU-b, 1000 MiB, 900 MiB, 100 MiB, 10 %\n";

        let result = AllUsage::parse_command_output(duplicate);
        assert!(result.is_err());
        let error = result
            .err()
            .unwrap_or_else(|| panic!("expected duplicate-index error"));
        assert!(error.to_string().contains("duplicate GPU index 0"));
    }

    #[test]
    fn usage_rejects_invalid_or_empty_probe_output() {
        assert!(AllUsage::parse_command_output("0, invalid").is_err());
        assert!(AllUsage::from_probe_outputs(std::iter::empty::<&str>()).is_err());
    }

    #[test]
    fn usage_maps_card_uuids_to_indices() {
        let usage = AllUsage::parse_command_output(
            "0, GPU-a, 1000 MiB, 900 MiB, 100 MiB, 10 %\n\
             2, GPU-b, 1000 MiB, 900 MiB, 100 MiB, 10 %\n",
        )
        .unwrap_or_else(|error| panic!("valid usage output failed: {error}"));

        assert_eq!(usage.uuids.get("GPU-a"), Some(&0));
        assert_eq!(usage.uuids.get("GPU-b"), Some(&2));
        assert_eq!(usage.uuids.len(), 2);
    }

    #[test]
    fn usage_attaches_one_app_snapshot_per_card() {
        let mut usage = AllUsage::parse_command_output(
            "0, GPU-a, 1000 MiB, 900 MiB, 100 MiB, 10 %\n\
             1, GPU-b, 1000 MiB, 900 MiB, 100 MiB, 10 %\n",
        )
        .unwrap_or_else(|error| panic!("valid usage output failed: {error}"));
        let rows = vec![("GPU-a".to_owned(), 10), ("GPU-a".to_owned(), 11)];

        usage.attach_apps(&rows, 1000, |_| Some(1000));

        let gpu0 = usage
            .cards
            .get(&0)
            .unwrap_or_else(|| panic!("GPU 0 missing"));
        assert_eq!(gpu0.apps, AppCount { any: 2, foreign: 0 });
        let gpu1 = usage
            .cards
            .get(&1)
            .unwrap_or_else(|| panic!("GPU 1 missing"));
        assert_eq!(gpu1.apps, AppCount::default());
        assert!(usage.uuids.contains_key("GPU-a"));
    }
}

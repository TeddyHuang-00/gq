use std::{
    collections::BTreeMap,
    fmt,
    process::{Command, Stdio},
    sync::Once,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use ndarray::prelude::*;
use nom::{
    Parser,
    branch::alt,
    bytes::complete::{tag, take, take_till, take_till1, take_until},
    character::complete::{line_ending, multispace0, space0, space1, u8, u32, usize},
    combinator::{all_consuming, map, opt, value},
    multi::{separated_list0, separated_list1},
    sequence::{delimited, preceded, separated_pair, terminated},
};
use rustix::process::getuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkType {
    /// Link to self.
    X,
    /// `PCIe` + SMP interconnect between NUMA nodes.
    System,
    /// `PCIe` + `PCIe` Host Bridges within a NUMA node.
    SameNode,
    /// `PCIe` + `PCIe` Host Bridge (typically CPU)
    HostBridge,
    /// Multiple `PCIe` bridges (no `PCIe` Host Bridge)
    MultipleBridges,
    /// At most a single `PCIe` Bridge
    SingleBridge,
    /// Bounded set of # `NVLinks`
    NVLink(u8),
}

/// Coarse link quality used for reporting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Quality {
    High,
    Medium,
    Low,
}

impl fmt::Display for Quality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::High => "High",
            Self::Medium => "Medium",
            Self::Low => "Low",
        })
    }
}

// Built-in priors; `LinkProfile` in `score.rs` overrides them for
// topology-aware selection.
impl LinkType {
    /// One-direction bandwidth estimate, in GB/s.
    pub const fn bandwidth_gbps(self) -> f64 {
        match self {
            Self::X => 0.0,
            Self::System => 10.0,
            Self::SameNode => 20.0,
            Self::HostBridge => 22.0,
            Self::MultipleBridges | Self::SingleBridge => 25.0,
            Self::NVLink(x) => x as f64 * 25.0,
        }
    }

    /// End-to-end peer-to-peer latency estimate, in microseconds.
    pub const fn latency_us(self) -> f64 {
        match self {
            Self::X => 10.0,
            Self::System => 6.0,
            Self::SameNode => 3.0,
            Self::HostBridge => 2.2,
            Self::MultipleBridges => 1.8,
            Self::SingleBridge => 1.5,
            Self::NVLink(_) => 1.0,
        }
    }

    /// An estimate of link quality given the weights of bandwidth and latency
    /// scores, higher is better.
    pub const fn score(self, weight_bandwidth: f64, weight_latency: f64) -> f64 {
        weight_bandwidth * self.bandwidth_gbps() / 100.0 + weight_latency / self.latency_us()
    }

    /// Built-in pair score used by quality labels and as the default for
    /// topology ranking.
    pub const fn default_score(self) -> f64 {
        self.score(1.0, 1.0)
    }

    /// Classifies the link by its bandwidth/latency score, excluding the
    /// self-link.
    pub const fn quality(self) -> Option<Quality> {
        // SYS scores 0.267 and NODE scores 0.533, so 0.4 separates them.
        const MEDIUM_SCORE: f64 = 0.4;
        // SingleBridge/PIX scores 0.917, the lowest NVLink score is 1.0 (NV0),
        // so 1.0 separates them.
        const HIGH_SCORE: f64 = 1.0;
        if matches!(self, Self::X) {
            return None;
        }
        let score = self.default_score();
        if score >= HIGH_SCORE {
            Some(Quality::High)
        } else if score >= MEDIUM_SCORE {
            Some(Quality::Medium)
        } else {
            Some(Quality::Low)
        }
    }
}

/// Network topology
pub struct Topology {
    connections: Array2<LinkType>,
}

impl Topology {
    /// Builds a topology from a square row matrix. Test-only.
    #[cfg(test)]
    pub(crate) fn from_rows(rows: Vec<Vec<LinkType>>) -> Self {
        let dimensions = rows.len();
        assert!(
            rows.iter().all(|row| row.len() == dimensions),
            "topology test matrix must be square"
        );
        let mut connections = Array2::from_elem((dimensions, dimensions), LinkType::X);
        for (row_index, row) in rows.into_iter().enumerate() {
            for (column_index, link) in row.into_iter().enumerate() {
                connections[(row_index, column_index)] = link;
            }
        }
        Self { connections }
    }

    /// Parses the command output from query.
    fn parse_command_output<S>(input: S) -> Result<Self>
    where
        S: AsRef<str>,
    {
        let input = input.as_ref();
        let (input, gpus) = delimited(
            (
                space0::<&str, nom::error::Error<&str>>,
                opt((
                    tag("\x1b["),
                    take_till(|ch: char| ('@'..='~').contains(&ch)),
                    take(1usize),
                )),
            ),
            separated_list1(alt((space1, tag("\t"))), preceded(tag("GPU"), usize)),
            (take_till(|ch| ch == '\n'), line_ending),
        )
        .parse_complete(input)
        .map_err(|err| anyhow::anyhow!("{err}"))?;
        let (_, connections) = separated_list1(
            line_ending::<&str, nom::error::Error<&str>>,
            terminated(
                separated_pair(
                    preceded((space0, tag("GPU")), usize),
                    space1,
                    separated_list1(
                        space1,
                        alt((
                            value(LinkType::X, tag("X")),
                            value(LinkType::System, tag("SYS")),
                            value(LinkType::SameNode, tag("NODE")),
                            value(LinkType::HostBridge, tag("PHB")),
                            value(LinkType::MultipleBridges, tag("PXB")),
                            value(LinkType::SingleBridge, tag("PIX")),
                            map(preceded(tag("NV"), u8), LinkType::NVLink),
                        )),
                    ),
                ),
                take_till(|ch| ch == '\n'),
            ),
        )
        .parse_complete(input)
        .map_err(|err| anyhow::anyhow!("{err}"))?;
        let expected = (0..gpus.len()).collect::<Vec<_>>();
        if gpus != expected {
            anyhow::bail!("Mismatched GPU indices: Expect header {expected:?}, got {gpus:?}.")
        }
        let row_indices = connections
            .iter()
            .map(|(index, _)| *index)
            .collect::<Vec<_>>();
        if row_indices != expected {
            anyhow::bail!("Mismatched GPU indices: Expect rows {expected:?}, got {row_indices:?}.")
        }
        for (idx, (_, row)) in connections.iter().enumerate() {
            if row.len() != gpus.len() {
                anyhow::bail!(
                    "Mismatched shape: Expect {} columns at row {idx}, got {}",
                    gpus.len(),
                    row.len()
                )
            }
        }
        let mut matrix = Array2::from_elem((gpus.len(), gpus.len()), LinkType::X);
        for (row_index, row) in &connections {
            for (&column_index, &link) in gpus.iter().zip(row) {
                matrix[(*row_index, column_index)] = link;
            }
        }
        Ok(Self {
            connections: matrix,
        })
    }

    /// Runs `nvidia-smi` and returns the extracted topology.
    pub fn query() -> Result<Self> {
        Self::query_with_deadline(None)
    }

    /// Like [`Self::query`], but the probe is bounded by `deadline`.
    pub fn query_with_deadline(deadline: Option<Instant>) -> Result<Self> {
        let topology = Self::parse_command_output(smi(&["topo", "-m"], deadline)?)?;
        log::debug!("topology: {} GPUs", topology.dimensions());
        Ok(topology)
    }

    pub fn dimensions(&self) -> usize {
        self.connections.nrows()
    }

    pub fn connection(&self, row: usize, column: usize) -> LinkType {
        self.connections[(row, column)]
    }
}

/// Number of compute apps on one card.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AppCount {
    /// Apps of every owner.
    pub any: usize,
    /// Apps not owned by the current user.
    pub foreign: usize,
}

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
fn parse_compute_apps(output: &str) -> Result<Vec<(String, u32)>> {
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
fn count_apps(
    rows: &[(String, u32)],
    uuid_to_index: &BTreeMap<String, usize>,
    current_uid: u32,
    uid_of: impl Fn(u32) -> Option<u32>,
) -> BTreeMap<usize, AppCount> {
    let mut counts: BTreeMap<usize, AppCount> = BTreeMap::new();
    for (uuid, pid) in rows {
        let Some(&index) = uuid_to_index.get(uuid) else {
            log::debug!("ignoring compute app pid {pid} on unknown GPU {uuid}");
            continue;
        };
        let count = counts.entry(index).or_default();
        count.any += 1;
        if uid_of(*pid) != Some(current_uid) {
            count.foreign += 1;
        }
    }
    counts
}

/// Real UID of `pid` from `/proc/<pid>/status`; `None` when the process is
/// gone or the file is unreadable (for example under `hidepid`).
fn proc_uid(pid: u32) -> Option<u32> {
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

/// Longest a single `nvidia-smi` probe may run.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// How often a running probe checks for exit and the deadline.
const PROBE_POLL: Duration = Duration::from_millis(20);

/// Fails once `deadline` has passed, before starting or waiting for a probe.
fn ensure_before_deadline(deadline: Option<Instant>) -> Result<()> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        anyhow::bail!("the probe deadline passed before the probe finished");
    }
    Ok(())
}

/// Runs `nvidia-smi` (or `GQ_NVIDIA_SMI` when set) with the given arguments.
///
/// The child is killed after [`PROBE_TIMEOUT`] or when `deadline` passes,
/// whichever comes first. Signals are handled between probes, never during
/// one. Empty stdout is an error.
fn smi(args: &[&str], deadline: Option<Instant>) -> Result<String> {
    smi_with(args, deadline, false)
}

/// Like [`smi`], but empty stdout is a valid answer, as when no compute app is
/// running.
fn smi_allow_empty(args: &[&str], deadline: Option<Instant>) -> Result<String> {
    smi_with(args, deadline, true)
}

fn smi_with(args: &[&str], deadline: Option<Instant>, allow_empty: bool) -> Result<String> {
    let program = std::env::var("GQ_NVIDIA_SMI").unwrap_or_else(|_| "nvidia-smi".to_owned());
    if program.is_empty() {
        anyhow::bail!("GQ_NVIDIA_SMI is set but empty");
    }
    log::debug!("running `{program}` {args:?}");
    let limit = deadline.map_or(PROBE_TIMEOUT, |deadline| {
        deadline
            .saturating_duration_since(Instant::now())
            .min(PROBE_TIMEOUT)
    });
    if limit.is_zero() {
        anyhow::bail!("`{program}` not started: the probe deadline has already passed");
    }
    let started = Instant::now();
    let mut child = Command::new(&program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run `{program}`"))?;
    loop {
        let finished = child
            .try_wait()
            .with_context(|| format!("cannot wait for `{program}`"))?;
        if finished.is_some() {
            break;
        }
        if started.elapsed() >= limit {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("`{program}` did not finish within {limit:?}; killed it");
        }
        std::thread::sleep(PROBE_POLL);
    }
    let output = child
        .wait_with_output()
        .with_context(|| format!("cannot read output of `{program}`"))?;
    if !output.status.success() {
        anyhow::bail!(
            "`{program}` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout =
        String::from_utf8(output.stdout).context("nvidia-smi output is not valid UTF-8")?;
    if !allow_empty && stdout.trim().is_empty() {
        anyhow::bail!("`{program}` returned no output");
    }
    log::trace!("`{program}` {args:?} output:\n{stdout}");
    Ok(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOPOLOGY_EXAMPLE: &str = "    GPU0    GPU1    GPU2    GPU3    GPU4    GPU5    GPU6    GPU7    CPU Affinity    NUMA Affinity   GPU NUMA ID
GPU0     X      NV4     NODE    NODE    SYS     SYS     SYS     SYS     0-31,64-95      0               N/A
GPU1    NV4      X      NODE    NODE    SYS     SYS     SYS     SYS     0-31,64-95      0               N/A
GPU2    NODE    NODE     X      NV4     SYS     SYS     SYS     SYS     0-31,64-95      0               N/A
GPU3    NODE    NODE    NV4      X      SYS     SYS     SYS     SYS     0-31,64-95      0               N/A
GPU4    SYS     SYS     SYS     SYS      X      NV4     NODE    NODE    32-63,96-127    1               N/A
GPU5    SYS     SYS     SYS     SYS     NV4      X      NODE    NODE    32-63,96-127    1               N/A
GPU6    SYS     SYS     SYS     SYS     NODE    NODE     X      NV4     32-63,96-127    1               N/A
GPU7    SYS     SYS     SYS     SYS     NODE    NODE    NV4      X      32-63,96-127    1               N/A

Legend:

X    = Self
SYS  = Connection traversing PCIe as well as the SMP interconnect between NUMA nodes (e.g., QPI/UPI)
NODE = Connection traversing PCIe as well as the interconnect between PCIe Host Bridges within a NUMA node
PHB  = Connection traversing PCIe as well as a PCIe Host Bridge (typically the CPU)
PXB  = Connection traversing multiple PCIe bridges (without traversing the PCIe Host Bridge)
PIX  = Connection traversing at most a single PCIe bridge
NV#  = Connection traversing a bonded set of # NVLinks
";
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
    fn topology_parses_example_command_output() {
        let topology = Topology::parse_command_output(TOPOLOGY_EXAMPLE);

        assert!(topology.is_ok());
        let topology = topology.unwrap_or_else(|error| panic!("valid topology failed: {error}"));

        assert_eq!(topology.connections.shape(), &[8, 8]);
        assert_eq!(topology.connections[(0, 0)], LinkType::X);
        assert_eq!(topology.connections[(0, 1)], LinkType::NVLink(4));
        assert_eq!(topology.connections[(0, 2)], LinkType::SameNode);
        assert_eq!(topology.connections[(0, 4)], LinkType::System);
    }

    #[test]
    fn topology_maps_every_connection_label_to_its_link_type() {
        let input = "GPU0 GPU1 GPU2 GPU3 GPU4 GPU5 GPU6\n\
GPU0 X SYS NODE PHB PXB PIX NV8\n\
GPU1 X SYS NODE PHB PXB PIX NV8\n\
GPU2 X SYS NODE PHB PXB PIX NV8\n\
GPU3 X SYS NODE PHB PXB PIX NV8\n\
GPU4 X SYS NODE PHB PXB PIX NV8\n\
GPU5 X SYS NODE PHB PXB PIX NV8\n\
GPU6 X SYS NODE PHB PXB PIX NV8\n";
        let topology = Topology::parse_command_output(input);

        assert!(topology.is_ok());
        let topology = topology.unwrap_or_else(|error| panic!("valid topology failed: {error}"));

        assert_eq!(topology.connections.shape(), &[7, 7]);
        assert_eq!(
            topology.connections.row(0).to_vec(),
            vec![
                LinkType::X,
                LinkType::System,
                LinkType::SameNode,
                LinkType::HostBridge,
                LinkType::MultipleBridges,
                LinkType::SingleBridge,
                LinkType::NVLink(8),
            ]
        );
    }

    #[test]
    fn topology_parser_discards_ansi_sequences_in_header() {
        let input = TOPOLOGY_EXAMPLE
            .replacen("    GPU0    GPU1", "\x1b[4mGPU0\tGPU1", 1)
            .replacen("GPU NUMA ID\n", "GPU NUMA ID\x1b[0m\n", 1);
        let topology = Topology::parse_command_output(input);

        assert!(topology.is_ok());
        let topology = topology.unwrap_or_else(|error| panic!("valid topology failed: {error}"));

        assert_eq!(topology.connections.shape(), &[8, 8]);
        assert_eq!(topology.connections[(0, 1)], LinkType::NVLink(4));
    }

    #[test]
    fn topology_rejects_mismatched_rows_and_columns() {
        let wrong_rows = "GPU0 GPU1\nGPU0 X NV4\n";
        let wrong_columns = "GPU0 GPU1\nGPU0 X\nGPU1 NV4 X\n";

        assert!(Topology::parse_command_output(wrong_rows).is_err());
        assert!(Topology::parse_command_output(wrong_columns).is_err());
    }

    #[test]
    fn topology_rejects_gapped_or_reordered_card_indices() {
        let header_gap = "GPU0 GPU2\nGPU0 X NV4\nGPU2 NV4 X\n";
        let row_gap = "GPU0 GPU1 GPU2\nGPU0 X SYS SYS\nGPU2 SYS SYS X\n";
        let swapped = "GPU0 GPU1\nGPU1 NV4 X\nGPU0 X NV4\n";

        assert!(Topology::parse_command_output(header_gap).is_err());
        assert!(Topology::parse_command_output(row_gap).is_err());
        assert!(Topology::parse_command_output(swapped).is_err());
    }

    #[test]
    fn link_quality_buckets_match_previous_classification() {
        assert_eq!(LinkType::X.quality(), None);
        assert_eq!(LinkType::System.quality(), Some(Quality::Low));
        for link in [
            LinkType::SameNode,
            LinkType::HostBridge,
            LinkType::MultipleBridges,
        ] {
            assert_eq!(link.quality(), Some(Quality::Medium));
        }
        assert_eq!(LinkType::SingleBridge.quality(), Some(Quality::Medium));
        assert_eq!(LinkType::NVLink(0).quality(), Some(Quality::High));
        assert_eq!(LinkType::NVLink(1).quality(), Some(Quality::High));
        assert_eq!(LinkType::NVLink(8).quality(), Some(Quality::High));
    }

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

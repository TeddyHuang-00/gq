use std::{
    collections::BTreeMap,
    fmt,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use ndarray::prelude::*;
use nom::{
    Parser,
    branch::alt,
    bytes::complete::{tag, take, take_till},
    character::complete::{line_ending, space0, space1, u8, u32, usize},
    combinator::{all_consuming, map, opt, value},
    multi::separated_list1,
    sequence::{delimited, preceded, separated_pair, terminated},
};

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

// TODO: Add support for using user-provided measurements instead of prior
// heuristics.
/// Weight of the bandwidth term in [`LinkType::score`].
const BANDWIDTH_WEIGHT: f64 = 1.0;
/// Weight of the latency term in [`LinkType::score`].
const LATENCY_WEIGHT: f64 = 1.0;

impl LinkType {
    /// An estimate of relative bandwidth, higher the better
    pub const fn relative_bandwidth(self) -> f64 {
        match self {
            Self::X => 0.0,
            Self::System => 0.1,
            Self::SameNode => 0.2,
            Self::HostBridge => 0.22,
            Self::MultipleBridges | Self::SingleBridge => 0.25,
            Self::NVLink(x) => x as f64 * 0.25,
        }
    }

    /// An estimate of relative latency, lower the better
    pub const fn relative_latency(self) -> f64 {
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
        weight_bandwidth * self.relative_bandwidth() + weight_latency / self.relative_latency()
    }

    /// Canonical pair score used for topology ranking, higher is better.
    pub const fn default_score(self) -> f64 {
        self.score(BANDWIDTH_WEIGHT, LATENCY_WEIGHT)
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
        Self::parse_command_output(smi(&["topo", "-m"], deadline)?)
    }

    pub fn dimensions(&self) -> usize {
        self.connections.nrows()
    }

    pub fn connection(&self, row: usize, column: usize) -> LinkType {
        self.connections[(row, column)]
    }
}

/// Instantaneous usage statistics
#[derive(Clone, Copy, Debug)]
pub struct UsageInfo {
    pub memory_total_mib: f64,
    pub memory_free_mib: f64,
    pub memory_used_mib: f64,
    pub utilization_percent: f64,
}

pub struct AllUsage(BTreeMap<usize, UsageInfo>);

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
                            terminated(u32, tag(" MiB, ")),
                            terminated(u32, tag(" MiB, ")),
                            terminated(u32, tag(" MiB, ")),
                            terminated(u8, tag(" %")),
                        ),
                        |(mem_tot, mem_free, mem_used, util)| UsageInfo {
                            memory_total_mib: f64::from(mem_tot),
                            memory_free_mib: f64::from(mem_free),
                            memory_used_mib: f64::from(mem_used),
                            utilization_percent: f64::from(util),
                        },
                    ),
                ),
            ),
            opt(line_ending),
        ))
        .parse_complete(input)
        .map_err(|err| anyhow::anyhow!("{err}"))?;
        let mut usage = BTreeMap::new();
        for (index, info) in stats {
            if usage.insert(index, info).is_some() {
                anyhow::bail!("duplicate GPU index {index} in nvidia-smi output");
            }
        }
        Ok(Self(usage))
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
        let expected = first.0.keys().copied().collect::<Vec<_>>();
        for probe in &probes[1..] {
            let found = probe.0.keys().copied().collect::<Vec<_>>();
            if found != expected {
                return Err(anyhow::Error::new(InconsistentCardSet { expected, found }));
            }
        }
        let probe_count = f64::from(u32::try_from(probes.len())?);
        let mut aggregate = BTreeMap::new();
        for (&index, first_stats) in &first.0 {
            let mut stats = *first_stats;
            for probe in &probes[1..] {
                let next = probe
                    .0
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
        Ok(Self(aggregate))
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
                    "--query-gpu=index,memory.total,memory.free,memory.used,utilization.gpu",
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
        if probe_count > 60 {
            log::warn!(
                "probing {probe_count} times takes about {} seconds; 10 or fewer probes is \
                 recommended",
                probe_count.saturating_sub(1)
            );
        }
        match Self::from_probe_outputs(Self::sample(probe_count, deadline)?) {
            Ok(usage) => Ok(usage),
            Err(error) if error.is::<InconsistentCardSet>() => {
                log::warn!("discarding probe round ({error}); retrying");
                // A discarded round does not consume a wait attempt; one retry
                // is a bound against an infinite loop, not an extra attempt.
                Self::from_probe_outputs(Self::sample(probe_count, deadline)?)
            }
            Err(error) => Err(error),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (usize, UsageInfo)> + '_ {
        self.0.iter().map(|(&index, &usage)| (index, usage))
    }
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
/// one.
fn smi(args: &[&str], deadline: Option<Instant>) -> Result<String> {
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
    if stdout.trim().is_empty() {
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
    const USAGE_EXAMPLE: &str = "0, 49140 MiB, 48536 MiB, 5 MiB, 0 %
1, 49140 MiB, 43612 MiB, 4928 MiB, 53 %
2, 49140 MiB, 43612 MiB, 4928 MiB, 72 %
3, 49140 MiB, 44621 MiB, 3920 MiB, 96 %
4, 49140 MiB, 48536 MiB, 5 MiB, 0 %
5, 49140 MiB, 42548 MiB, 5992 MiB, 61 %
6, 49140 MiB, 42644 MiB, 5896 MiB, 44 %
7, 49140 MiB, 46844 MiB, 1696 MiB, 37 %
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

        assert!(usage.0.contains_key(&0));
        let gpu0 = usage.0.get(&0).unwrap_or_else(|| panic!("GPU 0 missing"));
        assert_eq!(gpu0.memory_total_mib, 49_140.0);
        assert_eq!(gpu0.memory_free_mib, 48_536.0);
        assert_eq!(gpu0.memory_used_mib, 5.0);
        assert_eq!(gpu0.utilization_percent, 0.0);
        assert!(usage.0.contains_key(&1));
        let gpu1 = usage.0.get(&1).unwrap_or_else(|| panic!("GPU 1 missing"));
        assert_eq!(gpu1.memory_free_mib, 43_612.0);
        assert_eq!(gpu1.utilization_percent, 53.0);
    }

    #[test]
    fn usage_aggregates_used_max_free_min_and_utilization_mean() {
        let first = "0, 1000 MiB, 900 MiB, 100 MiB, 10 %\n";
        let second = "0, 1000 MiB, 800 MiB, 200 MiB, 30 %\n";
        let usage_result = AllUsage::from_probe_outputs([first, second]);
        assert!(usage_result.is_ok());
        let usage =
            usage_result.unwrap_or_else(|error| panic!("valid usage output failed: {error}"));

        assert!(usage.0.contains_key(&0));
        let gpu0 = usage.0.get(&0).unwrap_or_else(|| panic!("GPU 0 missing"));
        assert_eq!(gpu0.memory_total_mib, 1000.0);
        assert_eq!(gpu0.memory_used_mib, 200.0);
        assert_eq!(gpu0.memory_free_mib, 800.0);
        assert_eq!(gpu0.utilization_percent, 20.0);
    }

    #[test]
    fn usage_rejects_probes_with_different_cards_or_memory_totals() {
        let one_card = "0, 1000 MiB, 900 MiB, 100 MiB, 10 %\n";
        let two_cards = "0, 1000 MiB, 900 MiB, 100 MiB, 10 %\n\
                         1, 1000 MiB, 900 MiB, 100 MiB, 10 %\n";
        let changed_total = "0, 2000 MiB, 900 MiB, 100 MiB, 10 %\n";

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
        let duplicate = "0, 1000 MiB, 900 MiB, 100 MiB, 10 %\n\
                         0, 1000 MiB, 900 MiB, 100 MiB, 10 %\n";

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
}

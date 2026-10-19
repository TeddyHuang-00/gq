use std::{fmt, time::Instant};

use anyhow::Result;
use ndarray::prelude::*;
use nom::{
    Parser,
    branch::alt,
    bytes::complete::{tag, take, take_till},
    character::complete::{line_ending, space0, space1, u8, usize},
    combinator::{map, opt, value},
    multi::separated_list1,
    sequence::{delimited, preceded, separated_pair, terminated},
};

use super::probe::smi;

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
}

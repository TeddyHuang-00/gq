//! Controls how to select cards for use/query.

use std::{cmp::Ordering, collections::BTreeMap};

use clap::{Args, ValueEnum};
use itertools::Itertools;

use crate::{
    query::{Topology, UsageInfo},
    score::LinkProfile,
};

/// Card ordering strategy when selecting cards.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Strategy {
    /// Lowest index first.
    First,
    /// Lowest memory used first, ties broken by index.
    MinUsed,
    /// Scores each `count`-card combination by the lexicographic pair (minimum
    /// pairwise link score, sum of pairwise link scores).
    ///
    /// The target is the maximum over all candidate combinations; only
    /// available combinations exactly matching the target are eligible.
    /// Selection is strict: there is no fallback.
    TopologyAware,
}

/// Controls how many cards are selected and in what order.
#[derive(Args, Debug)]
pub struct Select {
    /// Number of cards to be selected
    #[arg(
        short = 'k',
        long,
        default_value_t = 1,
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    pub count: u32,
    /// Card ordering strategy
    #[arg(long, value_enum, default_value = "first")]
    pub strategy: Strategy,
}

impl Select {
    /// Selects card indices for one run.
    ///
    /// `candidates` is the whole candidate set with usage; `idle` is the
    /// subset passing the usage predicate. Both are in ascending index order.
    /// The returned indices are ascending in every strategy.
    pub fn select(
        &self,
        candidates: &[(usize, UsageInfo)],
        idle: &[usize],
        topology: Option<&Topology>,
        profile: &LinkProfile,
    ) -> Option<Vec<usize>> {
        let count = usize::try_from(self.count).ok()?;
        if idle.len() < count {
            return None;
        }
        match self.strategy {
            Strategy::First => {
                let selected = idle[..count].to_vec();
                log::debug!("strategy first: selected the lowest-index idle cards {selected:?}");
                Some(selected)
            }
            Strategy::MinUsed => {
                let usage = candidates.iter().copied().collect::<BTreeMap<_, _>>();
                let mut ranked = idle
                    .iter()
                    .filter_map(|&index| usage.get(&index).map(|stats| (index, *stats)))
                    .collect::<Vec<_>>();
                ranked.sort_unstable_by(|left, right| {
                    left.1
                        .memory_used_mib
                        .total_cmp(&right.1.memory_used_mib)
                        .then_with(|| left.0.cmp(&right.0))
                });
                log::debug!(
                    "strategy min-used: ranking idle GPUs by used VRAM: {}",
                    ranked
                        .iter()
                        .map(|(index, stats)| format!(
                            "GPU{index}={:.0} MiB",
                            stats.memory_used_mib
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                let mut selected = ranked
                    .into_iter()
                    .take(count)
                    .map(|(index, _)| index)
                    .collect::<Vec<_>>();
                selected.sort_unstable();
                Some(selected)
            }
            // One card has no pairs to score, so topology-aware degenerates to
            // `first` and needs no matrix.
            Strategy::TopologyAware if count == 1 => {
                log::debug!(
                    "strategy topology-aware: one card has no pairs to score; selecting the first idle GPU{}",
                    idle[0]
                );
                Some(vec![idle[0]])
            }
            Strategy::TopologyAware => {
                let Some(topology) = topology else {
                    log::debug!(
                        "strategy topology-aware: no topology matrix for a {count}-card group"
                    );
                    return None;
                };
                let Some(target) = candidates
                    .iter()
                    .map(|(index, _)| *index)
                    .combinations(count)
                    .map(|combination| combination_score(topology, &combination, profile))
                    .max_by(|left, right| compare_scores(*left, *right))
                else {
                    log::debug!("strategy topology-aware: no candidate group to score");
                    return None;
                };
                log::debug!(
                    "strategy topology-aware: target (minimum pair, sum) = ({:.3}, {:.3})",
                    target.0,
                    target.1
                );
                let selected = idle
                    .iter()
                    .copied()
                    .combinations(count)
                    .find(|combination| {
                        compare_scores(combination_score(topology, combination, profile), target)
                            == Ordering::Equal
                    });
                if let Some(combination) = &selected {
                    log::debug!(
                        "strategy topology-aware: selected {combination:?} matching the target"
                    );
                } else {
                    log::debug!(
                        "strategy topology-aware: no idle group matches the target; not falling back"
                    );
                }
                selected
            }
        }
    }
}

/// Scores a combination by its minimum pairwise link score, then by the sum of
/// its pairwise link scores.
///
/// A combination with no pairs (`count` of 1) scores `(INFINITY, 0.0)`.
fn combination_score(
    topology: &Topology,
    combination: &[usize],
    profile: &LinkProfile,
) -> (f64, f64) {
    let mut scores = combination
        .iter()
        .array_combinations::<2>()
        .map(|[left, right]| profile.score(topology.connection(*left, *right)))
        .collect::<Vec<_>>();
    if scores.is_empty() {
        return (f64::INFINITY, 0.0);
    }
    // Sorting canonicalizes the sum so equal score multisets add identically.
    scores.sort_by(f64::total_cmp);
    (scores[0], scores.iter().sum())
}

/// Lexicographic comparison of `(minimum, sum)` scores.
fn compare_scores(left: (f64, f64), right: (f64, f64)) -> Ordering {
    left.0
        .total_cmp(&right.0)
        .then_with(|| left.1.total_cmp(&right.1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{AppCount, LinkType};

    fn cards(used_mib: &[f64]) -> Vec<(usize, UsageInfo)> {
        used_mib
            .iter()
            .enumerate()
            .map(|(index, &used)| {
                (
                    index,
                    UsageInfo {
                        memory_total_mib: 1024.0,
                        memory_free_mib: 1024.0 - used,
                        memory_used_mib: used,
                        utilization_percent: 0.0,
                        apps: AppCount::default(),
                    },
                )
            })
            .collect()
    }

    fn selector(strategy: Strategy, count: u32) -> Select {
        Select { count, strategy }
    }

    fn topology_from_pairs(dimensions: usize, pairs: &[(usize, usize, LinkType)]) -> Topology {
        let mut rows = vec![vec![LinkType::System; dimensions]; dimensions];
        for (index, row) in rows.iter_mut().enumerate() {
            row[index] = LinkType::X;
        }
        for &(left, right, link) in pairs {
            rows[left][right] = link;
            rows[right][left] = link;
        }
        Topology::from_rows(rows)
    }

    #[test]
    fn picks_the_lowest_idle_indices_first() {
        let candidates = cards(&[0.0; 4]);
        let selected = selector(Strategy::First, 2).select(
            &candidates,
            &[1, 3],
            None,
            &LinkProfile::default(),
        );
        assert_eq!(selected, Some(vec![1, 3]));
    }

    #[test]
    fn min_used_orders_by_memory_then_index_and_returns_ascending() {
        let candidates = cards(&[10.0, 50.0, 5.0, 10.0]);
        let selected = selector(Strategy::MinUsed, 2).select(
            &candidates,
            &[0, 1, 2, 3],
            None,
            &LinkProfile::default(),
        );
        assert_eq!(selected, Some(vec![0, 2]));
    }

    #[test]
    fn topology_aware_picks_the_max_min_pair_without_fallback() {
        let candidates = cards(&[0.0; 4]);
        let topology = topology_from_pairs(4, &[(0, 1, LinkType::NVLink(8))]);
        let selector = selector(Strategy::TopologyAware, 2);

        let selected = selector.select(
            &candidates,
            &[0, 1, 2, 3],
            Some(&topology),
            &LinkProfile::default(),
        );
        assert_eq!(selected, Some(vec![0, 1]));

        // The best pair is busy; the lower-scoring available pair is rejected.
        let selected = selector.select(
            &candidates,
            &[2, 3],
            Some(&topology),
            &LinkProfile::default(),
        );
        assert_eq!(selected, None);
    }

    #[test]
    fn topology_aware_breaks_min_ties_by_sum() {
        let candidates = cards(&[0.0; 4]);
        let topology = topology_from_pairs(
            4,
            &[
                (0, 1, LinkType::NVLink(8)),
                (0, 2, LinkType::HostBridge),
                (1, 2, LinkType::SingleBridge),
                (0, 3, LinkType::HostBridge),
                (1, 3, LinkType::HostBridge),
            ],
        );
        let selector = selector(Strategy::TopologyAware, 3);

        let selected = selector.select(
            &candidates,
            &[0, 1, 2, 3],
            Some(&topology),
            &LinkProfile::default(),
        );
        assert_eq!(selected, Some(vec![0, 1, 2]));

        // Same minimum as the target, smaller sum: not an exact match.
        let selected = selector.select(
            &candidates,
            &[0, 1, 3],
            Some(&topology),
            &LinkProfile::default(),
        );
        assert_eq!(selected, None);
    }

    #[test]
    fn topology_aware_degrades_to_the_first_idle_index_for_one_card() {
        let candidates = cards(&[0.0; 3]);
        let topology = topology_from_pairs(3, &[(0, 1, LinkType::NVLink(8))]);
        let selected = selector(Strategy::TopologyAware, 1).select(
            &candidates,
            &[1, 2],
            Some(&topology),
            &LinkProfile::default(),
        );
        assert_eq!(selected, Some(vec![1]));
        assert_eq!(
            combination_score(&topology, &[0], &LinkProfile::default()),
            (f64::INFINITY, 0.0)
        );
    }

    #[test]
    fn topology_aware_one_card_needs_no_topology() {
        let candidates = cards(&[0.0; 3]);
        let selected = selector(Strategy::TopologyAware, 1).select(
            &candidates,
            &[2],
            None,
            &LinkProfile::default(),
        );
        assert_eq!(selected, Some(vec![2]));
    }

    #[test]
    fn returns_none_when_fewer_idle_cards_than_requested() {
        let candidates = cards(&[0.0; 3]);
        assert_eq!(
            selector(Strategy::First, 2).select(&candidates, &[0], None, &LinkProfile::default()),
            None
        );
        assert_eq!(
            selector(Strategy::TopologyAware, 2).select(
                &candidates,
                &[0, 1],
                None,
                &LinkProfile::default()
            ),
            None
        );
    }

    #[test]
    fn topology_aware_uses_the_link_profile() {
        let candidates = cards(&[0.0; 4]);
        let topology = topology_from_pairs(4, &[(0, 1, LinkType::NVLink(8))]);
        let selector = selector(Strategy::TopologyAware, 2);

        let selected = selector.select(
            &candidates,
            &[0, 1, 2, 3],
            Some(&topology),
            &LinkProfile::default(),
        );
        assert_eq!(selected, Some(vec![0, 1]));

        let profile = LinkProfile::parse("[link.bandwidth]\nsys = 10000\n")
            .unwrap_or_else(|error| panic!("config should parse: {error:#}"));
        let selected = selector.select(&candidates, &[0, 1, 2, 3], Some(&topology), &profile);
        assert_eq!(selected, Some(vec![0, 2]));
    }
}

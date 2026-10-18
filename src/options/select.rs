//! Controls how to filter and pick cards for use/query.

use std::{cmp::Ordering, collections::BTreeMap, ops::Range};

use clap::{Args, ValueEnum};
use itertools::Itertools;

use crate::{
    query::{Topology, UsageInfo},
    units::{
        quantity::{Percent, Vram},
        range,
    },
};

/// Default maximum used VRAM when neither `--max-vram` nor `--min-vram` is
/// given.
const DEFAULT_MAX_VRAM: Vram = Vram::Bytes(100 * 1024 * 1024);

/// Controls which cards are candidates.
#[derive(Args, Debug)]
pub struct Filter {
    /// Only include cards in the specified range.
    #[arg(
        short = 'i',
        long,
        value_parser = range::parse,
        value_delimiter = ',',
        value_name = "A,B-C,..."
    )]
    pub include: Option<Vec<Range<usize>>>,
    /// Exclude cards in the specified range.
    #[arg(
        short = 'x',
        long,
        conflicts_with = "include",
        value_parser = range::parse,
        value_delimiter = ',',
        value_name = "A,B-C,..."
    )]
    pub exclude: Option<Vec<Range<usize>>>,
    /// Only pick cards whose VRAM usage is at most this size or percentage.
    ///
    /// Defaults to 100 MiB when `--max-vram` is not given; mutually exclusive
    /// with `--min-vram`.
    #[arg(
        long,
        conflicts_with = "min_vram",
        value_parser = Vram::parse,
        value_name = "X M/GB/... or Y %"
    )]
    pub max_vram: Option<Vram>,
    /// Only pick cards with at least this much free VRAM.
    #[arg(
        long,
        value_parser = Vram::parse,
        value_name = "X M/GB/... or Y %"
    )]
    pub min_vram: Option<Vram>,
    /// Only use cards whose utilization is at most this percentage.
    #[arg(
        long,
        visible_alias = "max-util",
        value_parser = Percent::parse,
        value_name = "PERCENT"
    )]
    pub max_utilization: Option<Percent>,
    /// Number of probes taken when measuring availability.
    ///
    /// Probes will be taken with 1 second interval.
    #[arg(
        long,
        visible_alias = "probe-cnt",
        value_name = "COUNT",
        default_value_t = 1,
        value_parser = clap::value_parser!(u16).range(1..)
    )]
    pub probe_count: u16,
}

impl Filter {
    /// Whether a GPU index passes the configured include or exclude ranges.
    pub fn accepts_index(&self, index: usize) -> bool {
        self.include.as_ref().map_or_else(
            || {
                self.exclude
                    .as_ref()
                    .is_none_or(|exclude| !exclude.iter().any(|range| range.contains(&index)))
            },
            |include| include.iter().any(|range| range.contains(&index)),
        )
    }

    /// Warns about include/exclude ranges that mention absent cards.
    ///
    /// `present` is the ascending list of cards reported by the probe.
    pub fn warn_absent(&self, present: &[usize]) {
        for (flag, ranges) in [("--include", &self.include), ("--exclude", &self.exclude)] {
            let Some(ranges) = ranges else {
                continue;
            };
            for range in ranges {
                if mentions_absent(range, present) {
                    let end = range.end.saturating_sub(1);
                    let mention = if range.start == end {
                        range.start.to_string()
                    } else {
                        format!("{}-{end}", range.start)
                    };
                    log::warn!(
                        "{flag} {mention} mentions cards not present; present cards: {present:?}"
                    );
                }
            }
        }
    }

    /// Whether measured usage meets the configured availability thresholds.
    ///
    /// Bounds are inclusive.
    pub fn accepts_usage(&self, stats: UsageInfo) -> bool {
        let memory_ok = self.min_vram.map_or_else(
            || {
                stats.memory_used_mib
                    <= threshold_mib(
                        self.max_vram.unwrap_or(DEFAULT_MAX_VRAM),
                        stats.memory_total_mib,
                    )
            },
            |min| stats.memory_free_mib >= threshold_mib(min, stats.memory_total_mib),
        );
        let utilization_ok = self
            .max_utilization
            .is_none_or(|max| stats.utilization_percent <= max.0);
        memory_ok && utilization_ok
    }
}

/// Card ordering strategy when picking cards.
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
    #[value(alias = "aligned")]
    TopologyAware,
}

/// Controls how many cards are picked and in what order.
#[derive(Args, Debug)]
pub struct Pick {
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

impl Pick {
    /// Picks card indices for one run.
    ///
    /// `candidates` is the whole candidate set with usage; `idle` is the
    /// subset passing the usage predicate. Both are in ascending index order.
    /// The returned indices are ascending in every strategy.
    pub fn select(
        &self,
        candidates: &[(usize, UsageInfo)],
        idle: &[usize],
        topology: Option<&Topology>,
    ) -> Option<Vec<usize>> {
        let count = usize::try_from(self.count).ok()?;
        if idle.len() < count {
            return None;
        }
        match self.strategy {
            Strategy::First => Some(idle[..count].to_vec()),
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
            Strategy::TopologyAware if count == 1 => Some(vec![idle[0]]),
            Strategy::TopologyAware => {
                let topology = topology?;
                let target = candidates
                    .iter()
                    .map(|(index, _)| *index)
                    .combinations(count)
                    .map(|combination| combination_score(topology, &combination))
                    .max_by(|left, right| compare_scores(*left, *right))?;
                idle.iter()
                    .copied()
                    .combinations(count)
                    .find(|combination| {
                        compare_scores(combination_score(topology, combination), target)
                            == Ordering::Equal
                    })
            }
        }
    }
}

/// Scores a combination by its minimum pairwise link score, then by the sum of
/// its pairwise link scores.
///
/// A combination with no pairs (`count` of 1) scores `(INFINITY, 0.0)`.
fn combination_score(topology: &Topology, combination: &[usize]) -> (f64, f64) {
    let mut scores = combination
        .iter()
        .array_combinations::<2>()
        .map(|[left, right]| topology.connection(*left, *right).default_score())
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

/// Whether a range mentions any index that is not present.
fn mentions_absent(range: &Range<usize>, present: &[usize]) -> bool {
    if range.len() > present.len() {
        return true;
    }
    range
        .clone()
        .any(|index| present.binary_search(&index).is_err())
}

fn threshold_mib(value: Vram, total_mib: f64) -> f64 {
    match value {
        #[allow(clippy::cast_precision_loss)] // VRAM sizes are within f64's exact integer range.
        Vram::Bytes(bytes) => bytes as f64 / (1024.0 * 1024.0),
        Vram::Percent(percent) => total_mib * percent.0 / 100.0,
    }
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;
    use crate::query::LinkType;

    fn filter() -> Filter {
        Filter {
            include: None,
            exclude: None,
            max_vram: None,
            min_vram: None,
            max_utilization: None,
            probe_count: 1,
        }
    }

    fn stats(total_mib: f64, used_mib: f64, utilization_percent: f64) -> UsageInfo {
        UsageInfo {
            memory_total_mib: total_mib,
            memory_free_mib: total_mib - used_mib,
            memory_used_mib: used_mib,
            utilization_percent,
        }
    }

    fn cards(used_mib: &[f64]) -> Vec<(usize, UsageInfo)> {
        used_mib
            .iter()
            .enumerate()
            .map(|(index, &used)| (index, stats(1024.0, used, 0.0)))
            .collect()
    }

    fn pick(strategy: Strategy, count: u32) -> Pick {
        Pick { count, strategy }
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
        let selected = pick(Strategy::First, 2).select(&candidates, &[1, 3], None);
        assert_eq!(selected, Some(vec![1, 3]));
    }

    #[test]
    fn min_used_orders_by_memory_then_index_and_returns_ascending() {
        let candidates = cards(&[10.0, 50.0, 5.0, 10.0]);
        let selected = pick(Strategy::MinUsed, 2).select(&candidates, &[0, 1, 2, 3], None);
        assert_eq!(selected, Some(vec![0, 2]));
    }

    #[test]
    fn topology_aware_picks_the_max_min_pair_without_fallback() {
        let candidates = cards(&[0.0; 4]);
        let topology = topology_from_pairs(4, &[(0, 1, LinkType::NVLink(8))]);
        let picker = pick(Strategy::TopologyAware, 2);

        let selected = picker.select(&candidates, &[0, 1, 2, 3], Some(&topology));
        assert_eq!(selected, Some(vec![0, 1]));

        // The best pair is busy; the lower-scoring available pair is rejected.
        let selected = picker.select(&candidates, &[2, 3], Some(&topology));
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
        let picker = pick(Strategy::TopologyAware, 3);

        let selected = picker.select(&candidates, &[0, 1, 2, 3], Some(&topology));
        assert_eq!(selected, Some(vec![0, 1, 2]));

        // Same minimum as the target, smaller sum: not an exact match.
        let selected = picker.select(&candidates, &[0, 1, 3], Some(&topology));
        assert_eq!(selected, None);
    }

    #[test]
    fn topology_aware_degrades_to_the_first_idle_index_for_one_card() {
        let candidates = cards(&[0.0; 3]);
        let topology = topology_from_pairs(3, &[(0, 1, LinkType::NVLink(8))]);
        let selected =
            pick(Strategy::TopologyAware, 1).select(&candidates, &[1, 2], Some(&topology));
        assert_eq!(selected, Some(vec![1]));
        assert_eq!(combination_score(&topology, &[0]), (f64::INFINITY, 0.0));
    }

    #[test]
    fn topology_aware_one_card_needs_no_topology() {
        let candidates = cards(&[0.0; 3]);
        let selected = pick(Strategy::TopologyAware, 1).select(&candidates, &[2], None);
        assert_eq!(selected, Some(vec![2]));
    }

    #[test]
    fn returns_none_when_fewer_idle_cards_than_requested() {
        let candidates = cards(&[0.0; 3]);
        assert_eq!(
            pick(Strategy::First, 2).select(&candidates, &[0], None),
            None
        );
        assert_eq!(
            pick(Strategy::TopologyAware, 2).select(&candidates, &[0, 1], None),
            None
        );
    }

    #[test]
    fn detects_ranges_mentioning_absent_cards() {
        let present = [0, 2];
        assert!(!mentions_absent(&(0..1), &present));
        assert!(mentions_absent(&(1..2), &present));
        assert!(mentions_absent(&(0..2), &present));
        assert!(mentions_absent(&(0..3), &present));
        assert!(mentions_absent(&(0..usize::MAX), &present));
    }

    #[test]
    fn selects_indexes_by_include_and_exclude_ranges() {
        let mut filter = filter();
        assert!(filter.accepts_index(3));
        filter.include = Some(vec![2..4]);
        assert!(filter.accepts_index(2));
        assert!(!filter.accepts_index(4));
        filter.include = None;
        filter.exclude = Some(vec![2..4]);
        assert!(!filter.accepts_index(3));
        assert!(filter.accepts_index(4));
    }

    #[test]
    fn defaults_to_one_hundred_mib_used() {
        let filter = filter();
        assert!(filter.accepts_usage(stats(1024.0, 50.0, 0.0)));
        assert!(!filter.accepts_usage(stats(1024.0, 150.0, 0.0)));
    }

    #[test]
    fn selects_usage_by_memory_and_utilization_thresholds() {
        let mut filter = filter();
        filter.min_vram = Some(Vram::Bytes(700 * 1024 * 1024));
        assert!(filter.accepts_usage(stats(1024.0, 324.0, 0.0)));
        filter.min_vram = Some(Vram::Bytes(800 * 1024 * 1024));
        assert!(!filter.accepts_usage(stats(1024.0, 324.0, 0.0)));

        filter.min_vram = None;
        filter.max_vram = Some(Vram::Percent(Percent(25.0)));
        assert!(filter.accepts_usage(stats(1024.0, 256.0, 0.0)));
        assert!(!filter.accepts_usage(stats(1024.0, 256.1, 0.0)));

        filter.max_vram = None;
        filter.max_utilization = Some(Percent(40.0));
        assert!(filter.accepts_usage(stats(1024.0, 0.0, 40.0)));
        assert!(!filter.accepts_usage(stats(1024.0, 0.0, 40.1)));
        filter.max_utilization = Some(Percent(0.0));
        assert!(filter.accepts_usage(stats(1024.0, 0.0, 0.0)));
    }

    #[test]
    fn min_vram_takes_internal_precedence_when_both_fields_are_set() {
        // clap forbids `--max-vram` with `--min-vram`; this covers only the
        // internal branch when both fields are set directly.
        let mut filter = filter();
        filter.max_vram = Some(Vram::Bytes(0));
        filter.min_vram = Some(Vram::Bytes(700 * 1024 * 1024));
        assert!(filter.accepts_usage(stats(1024.0, 324.0, 0.0)));
    }
}

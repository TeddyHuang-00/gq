//! Controls how to filter and pick cards for use/query.

use std::ops::Range;

use clap::{Args, ValueEnum};

use crate::{
    query::UsageInfo,
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
    /// Consecutive blocks of `count` anchored at physical index 0.
    ///
    /// The whole block must be eligible; `count` of 1 degenerates to `first`.
    Aligned,
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

//! Controls how to filter cards for use/query.

use std::ops::Range;

use clap::Args;

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

/// Controls which cards are candidates and idle.
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
    /// Only pick cards with at most this many apps owned by other users.
    ///
    /// Apps whose owner cannot be read from `/proc` count as foreign. The
    /// default of 0 marks any card running another user's app as in use.
    #[arg(long, value_name = "COUNT", default_value_t = 0)]
    pub max_foreign_apps: usize,
    /// Only pick cards with at most this many compute apps in total.
    #[arg(long, value_name = "COUNT")]
    pub max_any_apps: Option<usize>,
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
                    let mention = range_label(range);
                    log::warn!(
                        "{flag} {mention} mentions cards not present; present cards: {present:?}"
                    );
                }
            }
        }
    }

    /// Whether `--include` or `--exclude` narrows the card set.
    pub const fn has_index_filter(&self) -> bool {
        self.include.is_some() || self.exclude.is_some()
    }

    /// Logs the candidate count and the effective include/exclude flags once
    /// per command, after the first successful probe.
    ///
    /// `present` and `candidates` are in ascending index order.
    pub fn log_candidates(&self, present: &[usize], candidates: &[usize]) {
        let ranges = self
            .include
            .as_ref()
            .map(|ranges| ("--include", ranges))
            .or_else(|| self.exclude.as_ref().map(|ranges| ("--exclude", ranges)));
        let note = ranges.map_or_else(String::new, |(flag, ranges)| {
            let ranges = ranges.iter().map(range_label).collect::<Vec<_>>().join(",");
            format!(" ({flag} {ranges})")
        });
        let line = format!(
            "candidates: {}/{} GPUs{note}",
            candidates.len(),
            present.len()
        );
        // An unfiltered `N/N` count is noise at Info; keep it as detail.
        if self.has_index_filter() {
            log::info!("{line}");
        } else {
            log::debug!("{line}");
        }
        if candidates.len() < present.len() {
            let dropped = present
                .iter()
                .copied()
                .filter(|index| !candidates.contains(index))
                .collect::<Vec<_>>();
            log::debug!("not candidates: {dropped:?}");
        }
    }

    /// Logs the effective idle thresholds once per command.
    pub fn log_idle_rule(&self) {
        let (memory, default) = match (self.min_vram, self.max_vram) {
            (Some(min), _) => (format!("free VRAM >= {}", describe_vram(min)), ""),
            (None, Some(max)) => (format!("used VRAM <= {}", describe_vram(max)), ""),
            (None, None) => (
                format!("used VRAM <= {}", describe_vram(DEFAULT_MAX_VRAM)),
                " (default)",
            ),
        };
        let utilization = self.max_utilization.map_or_else(
            || "no utilization limit".to_owned(),
            |max| format!("utilization <= {}%", max.0),
        );
        let apps = self.max_any_apps.map_or_else(
            || format!("foreign apps <= {}", self.max_foreign_apps),
            |max| {
                format!(
                    "foreign apps <= {}; any apps <= {max}",
                    self.max_foreign_apps
                )
            },
        );
        log::info!("idle rule: {memory}{default}; {utilization}; {apps}");
    }

    /// Logs why one candidate is or is not idle, at Debug.
    pub fn log_idle(&self, index: usize, stats: UsageInfo) {
        if !log::log_enabled!(log::Level::Debug) {
            return;
        }
        match self.usage_miss(stats) {
            None => {
                let memory = self.min_vram.map_or_else(
                    || {
                        let max = self.max_vram.unwrap_or(DEFAULT_MAX_VRAM);
                        format!(
                            "used {} <= {}",
                            mib_label(stats.memory_used_mib),
                            mib_label(threshold_mib(max, stats.memory_total_mib))
                        )
                    },
                    |min| {
                        format!(
                            "free {} >= {}",
                            mib_label(stats.memory_free_mib),
                            mib_label(threshold_mib(min, stats.memory_total_mib))
                        )
                    },
                );
                let value = self.min_vram.or(self.max_vram).unwrap_or(DEFAULT_MAX_VRAM);
                log::debug!(
                    "GPU{index}: idle ({memory}{}, util {:.0}%, apps {} ({} foreign))",
                    threshold_note(value),
                    stats.utilization_percent,
                    stats.apps.any,
                    stats.apps.foreign
                );
            }
            Some(UsageMiss::Used {
                used_mib,
                limit_mib,
                value,
            }) => log::debug!(
                "GPU{index}: not idle: used VRAM {} > {}{}",
                mib_label(used_mib),
                mib_label(limit_mib),
                threshold_note(value)
            ),
            Some(UsageMiss::Free {
                free_mib,
                limit_mib,
                value,
            }) => log::debug!(
                "GPU{index}: not idle: free VRAM {} < {}{}",
                mib_label(free_mib),
                mib_label(limit_mib),
                threshold_note(value)
            ),
            Some(UsageMiss::Utilization { actual, limit }) => {
                log::debug!("GPU{index}: not idle: utilization {actual:.0}% > {limit}%");
            }
            Some(UsageMiss::ForeignApps { count, limit }) => {
                log::debug!("GPU{index}: not idle: foreign apps {count} > {limit}");
            }
            Some(UsageMiss::AnyApps { count, limit }) => {
                log::debug!("GPU{index}: not idle: apps (any user) {count} > {limit}");
            }
        }
    }

    /// Whether measured usage meets the configured availability thresholds.
    ///
    /// Bounds are inclusive.
    pub fn accepts_usage(&self, stats: UsageInfo) -> bool {
        self.usage_miss(stats).is_none()
    }

    /// Returns the first failed idle threshold, if any.
    fn usage_miss(&self, stats: UsageInfo) -> Option<UsageMiss> {
        if let Some(min) = self.min_vram {
            let limit_mib = threshold_mib(min, stats.memory_total_mib);
            if stats.memory_free_mib < limit_mib {
                return Some(UsageMiss::Free {
                    free_mib: stats.memory_free_mib,
                    limit_mib,
                    value: min,
                });
            }
        } else {
            let value = self.max_vram.unwrap_or(DEFAULT_MAX_VRAM);
            let limit_mib = threshold_mib(value, stats.memory_total_mib);
            if stats.memory_used_mib > limit_mib {
                return Some(UsageMiss::Used {
                    used_mib: stats.memory_used_mib,
                    limit_mib,
                    value,
                });
            }
        }
        if let Some(limit) = self.max_utilization
            && stats.utilization_percent > limit.0
        {
            return Some(UsageMiss::Utilization {
                actual: stats.utilization_percent,
                limit: limit.0,
            });
        }
        if stats.apps.foreign > self.max_foreign_apps {
            return Some(UsageMiss::ForeignApps {
                count: stats.apps.foreign,
                limit: self.max_foreign_apps,
            });
        }
        if let Some(limit) = self.max_any_apps
            && stats.apps.any > limit
        {
            return Some(UsageMiss::AnyApps {
                count: stats.apps.any,
                limit,
            });
        }
        None
    }
}

/// Why a card fails the idle predicate. The enum itself formats nothing;
/// `log_idle` formats it only when Debug is enabled.
enum UsageMiss {
    Used {
        used_mib: f64,
        limit_mib: f64,
        value: Vram,
    },
    Free {
        free_mib: f64,
        limit_mib: f64,
        value: Vram,
    },
    Utilization {
        actual: f64,
        limit: f64,
    },
    ForeignApps {
        count: usize,
        limit: usize,
    },
    AnyApps {
        count: usize,
        limit: usize,
    },
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

/// Formats an inclusive range as it appears on the command line: `3` or `2-5`.
fn range_label(range: &Range<usize>) -> String {
    let end = range.end.saturating_sub(1);
    if range.start == end {
        range.start.to_string()
    } else {
        format!("{}-{end}", range.start)
    }
}

/// Formats a MiB size for logs, keeping one decimal below 10 MiB when the
/// value is not integral (for example `0.5 MiB` instead of `0 MiB`).
fn mib_label(mib: f64) -> String {
    if mib < 10.0 && mib.fract() > 0.0 {
        format!("{mib:.1} MiB")
    } else {
        format!("{mib:.0} MiB")
    }
}

/// Formats a configured VRAM threshold for logs.
#[allow(clippy::cast_precision_loss)] // VRAM sizes are within f64's exact integer range.
fn describe_vram(value: Vram) -> String {
    match value {
        Vram::Bytes(bytes) => {
            let mib = bytes as f64 / (1024.0 * 1024.0);
            if mib >= 1024.0 {
                format!("{:.1} GiB", mib / 1024.0)
            } else {
                mib_label(mib)
            }
        }
        Vram::Percent(percent) => format!("{}% of total VRAM", percent.0),
    }
}

/// Explains a percentage threshold; absolute sizes are already clear from the
/// MiB limit in the same message.
fn threshold_note(value: Vram) -> String {
    match value {
        Vram::Bytes(_) => String::new(),
        Vram::Percent(percent) => format!(", {}% of total", percent.0),
    }
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
    use crate::query::AppCount;

    fn filter() -> Filter {
        Filter {
            include: None,
            exclude: None,
            max_vram: None,
            min_vram: None,
            max_utilization: None,
            max_foreign_apps: 0,
            max_any_apps: None,
            probe_count: 1,
        }
    }

    fn stats(total_mib: f64, used_mib: f64, utilization_percent: f64) -> UsageInfo {
        UsageInfo {
            memory_total_mib: total_mib,
            memory_free_mib: total_mib - used_mib,
            memory_used_mib: used_mib,
            utilization_percent,
            apps: AppCount::default(),
        }
    }

    /// Idle-by-memory stats carrying the given app counts.
    fn stats_with_apps(any: usize, foreign: usize) -> UsageInfo {
        let mut stats = stats(1024.0, 0.0, 0.0);
        stats.apps = AppCount { any, foreign };
        stats
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
    fn filters_cards_by_foreign_and_total_app_counts() {
        // Default: no foreign app is allowed; the total count is ignored.
        let mut filter = filter();
        assert!(filter.accepts_usage(stats_with_apps(0, 0)));
        assert!(filter.accepts_usage(stats_with_apps(3, 0)));
        assert!(!filter.accepts_usage(stats_with_apps(1, 1)));

        // Bounds are inclusive: a count equal to the limit passes.
        filter.max_foreign_apps = 2;
        assert!(filter.accepts_usage(stats_with_apps(2, 2)));
        assert!(!filter.accepts_usage(stats_with_apps(3, 3)));

        // `--max-any-apps` is disabled unless set; `Some(0)` rejects any app.
        filter.max_foreign_apps = 0;
        filter.max_any_apps = Some(0);
        assert!(filter.accepts_usage(stats_with_apps(0, 0)));
        assert!(!filter.accepts_usage(stats_with_apps(1, 0)));

        filter.max_any_apps = Some(1);
        assert!(filter.accepts_usage(stats_with_apps(1, 0)));
        assert!(!filter.accepts_usage(stats_with_apps(2, 0)));

        // Both limits combine with AND; either can reject on its own.
        filter.max_foreign_apps = 1;
        filter.max_any_apps = Some(2);
        assert!(filter.accepts_usage(stats_with_apps(2, 1)));
        assert!(!filter.accepts_usage(stats_with_apps(1, 2)), "foreign only");
        assert!(!filter.accepts_usage(stats_with_apps(3, 0)), "any only");
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

    #[test]
    fn formats_vram_thresholds_for_logs() {
        assert_eq!(describe_vram(DEFAULT_MAX_VRAM), "100 MiB");
        assert_eq!(
            describe_vram(Vram::Bytes(8 * 1024 * 1024 * 1024)),
            "8.0 GiB"
        );
        assert_eq!(
            describe_vram(Vram::Percent(Percent(25.0))),
            "25% of total VRAM"
        );
        // Sub-MiB and fractional thresholds must not collapse to `0 MiB`.
        assert_eq!(describe_vram(Vram::Bytes(500 * 1024)), "0.5 MiB");
        assert_eq!(mib_label(0.0), "0 MiB");
        assert_eq!(mib_label(5.0), "5 MiB");
        assert_eq!(mib_label(9.5), "9.5 MiB");
        assert_eq!(mib_label(1024.0), "1024 MiB");
        assert_eq!(threshold_note(DEFAULT_MAX_VRAM), "");
        assert_eq!(
            threshold_note(Vram::Percent(Percent(25.0))),
            ", 25% of total"
        );
    }

    #[test]
    fn reports_the_first_failed_idle_threshold() {
        let default_filter = filter();
        assert!(
            default_filter
                .usage_miss(stats(1024.0, 50.0, 0.0))
                .is_none()
        );
        assert!(matches!(
            default_filter.usage_miss(stats(1024.0, 150.0, 0.0)),
            Some(UsageMiss::Used { .. })
        ));

        let mut utilization_filter = filter();
        utilization_filter.max_utilization = Some(Percent(10.0));
        assert!(matches!(
            utilization_filter.usage_miss(stats(1024.0, 0.0, 20.0)),
            Some(UsageMiss::Utilization { .. })
        ));

        let mut min_filter = filter();
        min_filter.min_vram = Some(Vram::Percent(Percent(50.0)));
        assert!(matches!(
            min_filter.usage_miss(stats(1024.0, 700.0, 0.0)),
            Some(UsageMiss::Free { .. })
        ));

        // Memory is checked first when both memory and utilization fail.
        let mut both_used = filter();
        both_used.max_vram = Some(Vram::Bytes(0));
        both_used.max_utilization = Some(Percent(0.0));
        assert!(matches!(
            both_used.usage_miss(stats(1024.0, 150.0, 50.0)),
            Some(UsageMiss::Used { .. })
        ));

        let mut both_free = filter();
        both_free.min_vram = Some(Vram::Percent(Percent(50.0)));
        both_free.max_utilization = Some(Percent(0.0));
        assert!(matches!(
            both_free.usage_miss(stats(1024.0, 700.0, 50.0)),
            Some(UsageMiss::Free { .. })
        ));

        // App limits are checked last: memory and utilization win.
        let mut apps = filter();
        apps.max_any_apps = Some(0);
        assert!(matches!(
            apps.usage_miss(stats_with_apps(1, 0)),
            Some(UsageMiss::AnyApps { .. })
        ));
        apps.max_utilization = Some(Percent(0.0));
        let mut busy_util = stats_with_apps(1, 0);
        busy_util.utilization_percent = 50.0;
        assert!(matches!(
            apps.usage_miss(busy_util),
            Some(UsageMiss::Utilization { .. })
        ));
        apps.max_vram = Some(Vram::Bytes(0));
        let mut busy_memory = stats_with_apps(1, 0);
        busy_memory.memory_used_mib = 150.0;
        assert!(matches!(
            apps.usage_miss(busy_memory),
            Some(UsageMiss::Used { .. })
        ));

        // Foreign apps are checked before the total-count limit.
        let mut foreign_first = filter();
        foreign_first.max_any_apps = Some(0);
        assert!(matches!(
            foreign_first.usage_miss(stats_with_apps(1, 1)),
            Some(UsageMiss::ForeignApps { .. })
        ));
    }
}

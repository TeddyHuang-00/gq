//! Controls how to filter cards for use/query.

use std::ops::Range;

use clap::Args;

use crate::units::{
    quantity::{Percent, Vram},
    range,
};

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

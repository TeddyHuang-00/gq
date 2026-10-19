//! Controls how to select cards for use/query.

use clap::{Args, ValueEnum};

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

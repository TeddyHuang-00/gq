//! Data acquisition layers: nvidia-smi probing, topology, usage, compute apps.

mod apps;
mod probe;
mod topology;
mod usage;

// Named directly only by tests; production code reads it through
// UsageInfo::apps.
#[cfg(test)]
pub use apps::AppCount;
pub use topology::{LinkProfile, LinkType, Quality, Topology};
pub use usage::{AllUsage, UsageInfo};

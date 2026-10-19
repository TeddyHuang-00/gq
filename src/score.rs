//! Configurable link bandwidth, latency, and score weights.

use std::{env, fs, io, path::PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::data::LinkType;

/// One-direction bandwidth for each link class, in GB/s.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BandwidthConfig {
    sys: f64,
    node: f64,
    phb: f64,
    pxb: f64,
    pix: f64,
    /// Per-link bandwidth; [`LinkType::NVLink`] multiplies it by the link
    /// count.
    nvlink: f64,
}

impl Default for BandwidthConfig {
    fn default() -> Self {
        Self {
            sys: 10.0,
            node: 20.0,
            phb: 22.0,
            pxb: 25.0,
            pix: 25.0,
            nvlink: 25.0,
        }
    }
}

/// End-to-end peer-to-peer latency for each link class, in microseconds.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LatencyConfig {
    sys: f64,
    node: f64,
    phb: f64,
    pxb: f64,
    pix: f64,
    /// Per-link latency; a bonded `NVLink(n)` keeps this value.
    nvlink: f64,
}

impl Default for LatencyConfig {
    fn default() -> Self {
        Self {
            sys: 6.0,
            node: 3.0,
            phb: 2.2,
            pxb: 1.8,
            pix: 1.5,
            nvlink: 1.0,
        }
    }
}

/// Weights of the bandwidth and latency terms.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Weights {
    bandwidth: f64,
    latency: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            bandwidth: 1.0,
            latency: 1.0,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LinkConfig {
    bandwidth: BandwidthConfig,
    latency: LatencyConfig,
    score: Weights,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    link: LinkConfig,
}

impl Config {
    fn validate(&self) -> Result<()> {
        for (field, value) in [
            ("link.bandwidth.sys", self.link.bandwidth.sys),
            ("link.bandwidth.node", self.link.bandwidth.node),
            ("link.bandwidth.phb", self.link.bandwidth.phb),
            ("link.bandwidth.pxb", self.link.bandwidth.pxb),
            ("link.bandwidth.pix", self.link.bandwidth.pix),
            ("link.bandwidth.nvlink", self.link.bandwidth.nvlink),
            ("link.latency.sys", self.link.latency.sys),
            ("link.latency.node", self.link.latency.node),
            ("link.latency.phb", self.link.latency.phb),
            ("link.latency.pxb", self.link.latency.pxb),
            ("link.latency.pix", self.link.latency.pix),
            ("link.latency.nvlink", self.link.latency.nvlink),
        ] {
            if !value.is_finite() || value <= 0.0 {
                bail!("{field} must be finite and greater than zero");
            }
        }
        for (field, value) in [
            ("link.score.bandwidth", self.link.score.bandwidth),
            ("link.score.latency", self.link.score.latency),
        ] {
            if !value.is_finite() || value < 0.0 {
                bail!("{field} must be finite and not negative");
            }
        }
        if self.link.score.bandwidth <= 0.0 && self.link.score.latency <= 0.0 {
            bail!("link.score.bandwidth and link.score.latency must not both be zero");
        }
        Ok(())
    }
}

/// Per-class link priors, optionally overridden by the config file.
#[derive(Debug, Default)]
pub struct LinkProfile {
    bandwidth: BandwidthConfig,
    latency: LatencyConfig,
    weights: Weights,
}

impl LinkProfile {
    /// Loads the profile from the config file, or the built-in priors when no
    /// usable config file exists.
    pub fn load() -> Result<Self> {
        let Some(home) = config_home() else {
            log::debug!("no usable config directory; using built-in link priors");
            return Ok(Self::default());
        };
        let path = home.join("gq").join("config.toml");
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                log::debug!(
                    "no config file at {}; using built-in link priors",
                    path.display()
                );
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", path.display()));
            }
        };
        let profile = Self::parse(&text)
            .with_context(|| format!("invalid config file {}", path.display()))?;
        log::debug!("loaded link profile from {}", path.display());
        Ok(profile)
    }

    /// Parses a config document.
    pub(crate) fn parse(text: &str) -> Result<Self> {
        let config: Config = toml::from_str(text)?;
        config.validate()?;
        Ok(Self {
            bandwidth: config.link.bandwidth,
            latency: config.link.latency,
            weights: config.link.score,
        })
    }

    /// Scores a link class, higher is better.
    pub fn score(&self, link: LinkType) -> f64 {
        let (bandwidth_gbps, latency_us) = match link {
            LinkType::X => (0.0, 10.0),
            LinkType::System => (self.bandwidth.sys, self.latency.sys),
            LinkType::SameNode => (self.bandwidth.node, self.latency.node),
            LinkType::HostBridge => (self.bandwidth.phb, self.latency.phb),
            LinkType::MultipleBridges => (self.bandwidth.pxb, self.latency.pxb),
            LinkType::SingleBridge => (self.bandwidth.pix, self.latency.pix),
            LinkType::NVLink(count) => (
                self.bandwidth.nvlink * f64::from(count),
                self.latency.nvlink,
            ),
        };
        self.weights.bandwidth * bandwidth_gbps / 100.0 + self.weights.latency / latency_us
    }
}

/// Returns the config directory, preferring an absolute `XDG_CONFIG_HOME`.
fn config_home() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_or_panic(text: &str) -> LinkProfile {
        LinkProfile::parse(text).unwrap_or_else(|error| panic!("config should parse: {error:#}"))
    }

    #[test]
    fn defaults_match_builtin_scores_bit_for_bit() {
        let profile = LinkProfile::default();
        for link in [
            LinkType::X,
            LinkType::System,
            LinkType::SameNode,
            LinkType::HostBridge,
            LinkType::MultipleBridges,
            LinkType::SingleBridge,
            LinkType::NVLink(0),
            LinkType::NVLink(1),
            LinkType::NVLink(4),
            LinkType::NVLink(8),
        ] {
            assert_eq!(
                profile.score(link).to_bits(),
                link.default_score().to_bits(),
                "{link:?}"
            );
        }
    }

    #[test]
    fn partial_override_changes_only_its_class() {
        let defaults = LinkProfile::default();
        let profile = parse_or_panic("[link.bandwidth]\nsys = 10000\n");
        assert_ne!(
            profile.score(LinkType::System).to_bits(),
            defaults.score(LinkType::System).to_bits()
        );
        for link in [
            LinkType::X,
            LinkType::SameNode,
            LinkType::HostBridge,
            LinkType::MultipleBridges,
            LinkType::SingleBridge,
            LinkType::NVLink(4),
        ] {
            assert_eq!(
                profile.score(link).to_bits(),
                defaults.score(link).to_bits(),
                "{link:?}"
            );
        }
    }

    #[test]
    fn nvlink_override_scales_with_the_link_count() {
        let profile = parse_or_panic("[link.bandwidth]\nnvlink = 10\n");
        assert!((profile.score(LinkType::NVLink(4)) - 1.4).abs() < 1e-12);
        assert!((profile.score(LinkType::NVLink(8)) - 1.8).abs() < 1e-12);
        assert!((profile.score(LinkType::NVLink(0)) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn weights_change_the_score() {
        let profile = parse_or_panic("[link.score]\nbandwidth = 2.0\n");
        let expected = 2.0 * 10.0 / 100.0 + 1.0 / 6.0;
        assert!((profile.score(LinkType::System) - expected).abs() < 1e-12);
        let profile = parse_or_panic("[link.score]\nlatency = 0.5\n");
        let expected = 10.0 / 100.0 + 0.5 / 6.0;
        assert!((profile.score(LinkType::System) - expected).abs() < 1e-12);
    }

    #[test]
    fn parse_rejects_unknown_keys_and_invalid_values() {
        for text in [
            "[link]\nunknown = 1\n",
            "[link.bandwidth]\nunknown = 1\n",
            "[link.bandwidth]\nsys = 0\n",
            "[link.bandwidth]\nsys = -1\n",
            "[link.bandwidth]\nsys = nan\n",
            "[link.bandwidth]\nsys = inf\n",
            "[link.latency]\nsys = 0\n",
            "[link.latency]\nsys = -1\n",
            "[link.score]\nbandwidth = -1\n",
            "[link.score]\nlatency = -1\n",
            "[link.score]\nbandwidth = nan\n",
            "[link.score]\nbandwidth = 0\nlatency = 0\n",
        ] {
            assert!(LinkProfile::parse(text).is_err(), "should reject: {text}");
        }
    }
}

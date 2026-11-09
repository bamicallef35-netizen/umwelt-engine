//! P5.5 — engine config schema + YAML loader.
//!
//! Defines the shape of a single-agent Umwelt Engine configuration
//! as it arrives at `umwelt run`'s front door. The schema is a pure
//! descriptor: it names the inputs (vocabulary path, transport
//! choice), the pipeline parameters (channels, icon ring width), and
//! the runtime knobs (tick rate, mission duration). Parsing and
//! validation live here; binding these into live objects is P5.1's
//! job.
//!
//! ## Design
//!
//! - One flat top-level struct, [`EngineConfig`], with four nested
//!   sections ([`AgentConfig`], [`PipelineConfig`],
//!   [`TransportConfig`], [`RuntimeConfig`]). Sections are grouped
//!   so an operator can scan "what does this config describe?"
//!   without reading every key.
//!
//! - Pipeline and runtime fields have `#[serde(default)]` with
//!   values that match ADR-0013 / ADR-0015: 4 channels, 64-sample
//!   ring, 8 bins, 10 Hz tick, 30 s mission. That keeps the minimum
//!   viable config to two lines: `agent.id` and `agent.vocabulary`.
//!
//! - [`TransportConfig`] is an externally-tagged enum so the YAML
//!   reads as `transport: { kind: loopback }` or `transport: { kind:
//!   udp, bind: "0.0.0.0:4242", peer: "10.0.0.2:4242" }`. The
//!   `sim_lora_over_loopback` variant carries a preset name
//!   (matching [`SimLoraPreset`] exactly) and an RNG seed so replay
//!   is deterministic.
//!
//! - Validation is bounded: we reject obviously invalid values
//!   (`channels == 0`, `icon_window == 0`, `tick_hz == 0`,
//!   `duration_s == 0`, or UDP sockets that fail `parse`) but do
//!   not try to re-derive every constraint the pipeline enforces at
//!   runtime. The engine is still the source of truth for what
//!   combinations are supported; `validate` catches typos, not
//!   architectural mistakes.
//!
//! ## Example
//!
//! See `examples/configs/loopback_agent.yaml` at the workspace root
//! for a minimum-viable single-agent loopback config.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ── Errors ────────────────────────────────────────────────────────────────

/// Reasons config loading or validation can fail.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Failed to read the config file from disk.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// YAML parse error.
    #[error("yaml parse error: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),

    /// A required positive integer was zero or missing after defaults.
    #[error("config.{field} must be > 0 (got {got})")]
    NonPositive {
        /// Dotted path to the offending field.
        field: &'static str,
        /// The value that was rejected.
        got: u64,
    },

    /// A socket address couldn't be parsed.
    #[error("config.{field}: invalid socket address {addr:?}: {reason}")]
    BadSocket {
        /// Dotted path to the offending field.
        field: &'static str,
        /// The string that failed to parse.
        addr: String,
        /// Parser's reason.
        reason: String,
    },
}

// ── Top-level config ──────────────────────────────────────────────────────

/// Full engine configuration.
///
/// This is the single source of truth for one agent's boot
/// parameters. Load it with [`EngineConfig::from_file`] or
/// [`EngineConfig::from_yaml`]; both run [`EngineConfig::validate`]
/// before returning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineConfig {
    /// Agent identity and vocabulary binding.
    pub agent: AgentConfig,

    /// Pipeline dimensions. Uses ADR-0013 defaults if omitted.
    #[serde(default)]
    pub pipeline: PipelineConfig,

    /// Transport binding.
    pub transport: TransportConfig,

    /// Tick rate and mission length. Uses sensible defaults if
    /// omitted.
    #[serde(default)]
    pub runtime: RuntimeConfig,
}

impl EngineConfig {
    /// Parse from a YAML string and validate.
    pub fn from_yaml(yaml: &str) -> Result<Self, ConfigError> {
        let cfg: Self = serde_yaml_ng::from_str(yaml)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Read a YAML file from disk, parse, and validate.
    pub fn from_file(path: &Path) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path)?;
        Self::from_yaml(&contents)
    }

    /// Serialize back to YAML (round-trip friendly).
    pub fn to_yaml(&self) -> Result<String, ConfigError> {
        Ok(serde_yaml_ng::to_string(self)?)
    }

    /// Cheap sanity checks. Every constraint here is a "this config
    /// can't possibly boot an engine" check; deeper semantic
    /// validation is the engine's job.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.pipeline.channels == 0 {
            return Err(ConfigError::NonPositive {
                field: "pipeline.channels",
                got: 0,
            });
        }
        if self.pipeline.icon_window == 0 {
            return Err(ConfigError::NonPositive {
                field: "pipeline.icon_window",
                got: 0,
            });
        }
        if self.pipeline.icon_bins == 0 {
            return Err(ConfigError::NonPositive {
                field: "pipeline.icon_bins",
                got: 0,
            });
        }
        if self.runtime.tick_hz == 0 {
            return Err(ConfigError::NonPositive {
                field: "runtime.tick_hz",
                got: 0,
            });
        }
        if self.runtime.duration_s == 0 {
            return Err(ConfigError::NonPositive {
                field: "runtime.duration_s",
                got: 0,
            });
        }
        if let Some(ref addr) = self.runtime.metrics_bind {
            addr.parse::<SocketAddr>()
                .map_err(|e| ConfigError::BadSocket {
                    field: "runtime.metrics_bind",
                    addr: addr.clone(),
                    reason: e.to_string(),
                })?;
        }
        self.transport.validate()?;
        Ok(())
    }
}

// ── Agent identity ────────────────────────────────────────────────────────

/// Identity and vocabulary binding for one agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Stable 32-bit agent identifier used in SEP frames' `agent_id`
    /// field (ADR-0011). Operators pick this; the engine does not
    /// synthesise one at runtime.
    pub id: u32,

    /// Path to the sign vocabulary YAML (see `umwelt-vocab`).
    pub vocabulary: PathBuf,
}

// ── Pipeline sizing ───────────────────────────────────────────────────────

/// Pipeline geometry. Defaults match ADR-0013.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineConfig {
    /// Maximum concurrent channels in the pipeline. Must be > 0.
    #[serde(default = "default_channels")]
    pub channels: usize,

    /// Per-channel icon-ring window (samples). Must be > 0.
    #[serde(default = "default_icon_window")]
    pub icon_window: usize,

    /// Histogram bin count for the icon-layer entropy gate. Must be
    /// > 0; prefer powers of two.
    #[serde(default = "default_icon_bins")]
    pub icon_bins: usize,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            channels: default_channels(),
            icon_window: default_icon_window(),
            icon_bins: default_icon_bins(),
        }
    }
}

fn default_channels() -> usize {
    4
}
fn default_icon_window() -> usize {
    64
}
fn default_icon_bins() -> usize {
    8
}

// ── Transport binding ─────────────────────────────────────────────────────

/// SimLora preset names. These mirror the constructors on
/// [`umwelt_transport::sim_lora::SimLora`] exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimLoraPreset {
    /// Lossless, zero-latency, zero-jitter (useful as a regression
    /// baseline).
    Clean,
    /// 10% loss, 50 ms base + 50 ms jitter.
    Lossy,
    /// 40% loss, 200 ms base + 400 ms jitter.
    Fringe,
}

/// Transport binding. YAML shape is externally tagged via `kind`:
///
/// ```yaml
/// transport:
///   kind: loopback
/// # or
/// transport:
///   kind: udp
///   bind: "0.0.0.0:4242"
///   peer: "10.0.0.2:4242"
/// # or
/// transport:
///   kind: sim_lora_over_loopback
///   preset: lossy
///   seed: 42
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransportConfig {
    /// In-process loopback transport (P4.1). No peers, no network.
    Loopback,

    /// UDP socket transport (P4.2). `bind` and `peer` are required.
    Udp {
        /// Local bind address.
        bind: String,
        /// Remote peer address.
        peer: String,
    },

    /// Simulated-LoRa (P4.3) layered over the in-process loopback.
    /// Useful for offline mission-profile tests.
    SimLoraOverLoopback {
        /// Preset selecting `(loss_ppm, base_latency_us, jitter_us)`
        /// per ADR-0019.
        preset: SimLoraPreset,
        /// xorshift64 seed. 0 is safe — the constructor folds in a
        /// non-zero constant.
        #[serde(default)]
        seed: u64,
    },
}

impl TransportConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        match self {
            Self::Loopback | Self::SimLoraOverLoopback { .. } => Ok(()),
            Self::Udp { bind, peer } => {
                bind.parse::<SocketAddr>()
                    .map_err(|e| ConfigError::BadSocket {
                        field: "transport.bind",
                        addr: bind.clone(),
                        reason: e.to_string(),
                    })?;
                peer.parse::<SocketAddr>()
                    .map_err(|e| ConfigError::BadSocket {
                        field: "transport.peer",
                        addr: peer.clone(),
                        reason: e.to_string(),
                    })?;
                Ok(())
            }
        }
    }
}

// ── Runtime knobs ─────────────────────────────────────────────────────────

/// Runtime pacing. Defaults to 10 Hz over a 30 s mission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeConfig {
    /// Pipeline tick rate (Hz). Must be > 0.
    #[serde(default = "default_tick_hz")]
    pub tick_hz: u32,

    /// Mission duration (seconds). Must be > 0.
    #[serde(default = "default_duration_s")]
    pub duration_s: u32,

    /// Optional bind address for the live metrics HTTP endpoint
    /// (Prometheus exposition format). Example: `"127.0.0.1:9090"`.
    /// If `None`, no metrics server is started.
    #[serde(default)]
    pub metrics_bind: Option<String>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            tick_hz: default_tick_hz(),
            duration_s: default_duration_s(),
            metrics_bind: None,
        }
    }
}

fn default_tick_hz() -> u32 {
    10
}
fn default_duration_s() -> u32 {
    30
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL_LOOPBACK: &str = r#"
agent:
  id: 1
  vocabulary: vocabularies/swarm.yaml
transport:
  kind: loopback
"#;

    const FULL_EXAMPLE: &str = r#"
agent:
  id: 42
  vocabulary: vocabularies/terrain.yaml
pipeline:
  channels: 8
  icon_window: 128
  icon_bins: 16
transport:
  kind: sim_lora_over_loopback
  preset: fringe
  seed: 12345
runtime:
  tick_hz: 20
  duration_s: 60
"#;

    #[test]
    fn minimal_loopback_config_loads() {
        let cfg = EngineConfig::from_yaml(MINIMAL_LOOPBACK).unwrap();
        assert_eq!(cfg.agent.id, 1);
        // Defaults applied.
        assert_eq!(cfg.pipeline.channels, default_channels());
        assert_eq!(cfg.pipeline.icon_window, default_icon_window());
        assert_eq!(cfg.pipeline.icon_bins, default_icon_bins());
        assert_eq!(cfg.runtime.tick_hz, default_tick_hz());
        assert_eq!(cfg.runtime.duration_s, default_duration_s());
        assert_eq!(cfg.transport, TransportConfig::Loopback);
    }

    #[test]
    fn full_config_round_trips() {
        let cfg = EngineConfig::from_yaml(FULL_EXAMPLE).unwrap();
        let serialized = cfg.to_yaml().unwrap();
        let reparsed = EngineConfig::from_yaml(&serialized).unwrap();
        assert_eq!(cfg, reparsed);
    }

    #[test]
    fn sim_lora_variant_parses() {
        let cfg = EngineConfig::from_yaml(FULL_EXAMPLE).unwrap();
        match cfg.transport {
            TransportConfig::SimLoraOverLoopback { preset, seed } => {
                assert_eq!(preset, SimLoraPreset::Fringe);
                assert_eq!(seed, 12345);
            }
            other => panic!("expected SimLoraOverLoopback, got {other:?}"),
        }
    }

    #[test]
    fn udp_transport_validates_addresses() {
        let yaml = r#"
agent: { id: 2, vocabulary: v.yaml }
transport:
  kind: udp
  bind: "0.0.0.0:4242"
  peer: "10.0.0.2:4242"
"#;
        let cfg = EngineConfig::from_yaml(yaml).unwrap();
        match cfg.transport {
            TransportConfig::Udp { ref bind, ref peer } => {
                assert_eq!(bind, "0.0.0.0:4242");
                assert_eq!(peer, "10.0.0.2:4242");
            }
            _ => panic!("expected UDP variant"),
        }
    }

    #[test]
    fn udp_rejects_garbage_bind() {
        let yaml = r#"
agent: { id: 2, vocabulary: v.yaml }
transport:
  kind: udp
  bind: "not-a-socket"
  peer: "10.0.0.2:4242"
"#;
        let err = EngineConfig::from_yaml(yaml).unwrap_err();
        assert!(
            matches!(err, ConfigError::BadSocket { field, .. } if field == "transport.bind"),
            "expected BadSocket on bind, got {err:?}",
        );
    }

    #[test]
    fn zero_channels_is_rejected() {
        let yaml = r#"
agent: { id: 1, vocabulary: v.yaml }
pipeline: { channels: 0 }
transport: { kind: loopback }
"#;
        let err = EngineConfig::from_yaml(yaml).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::NonPositive {
                field: "pipeline.channels",
                ..
            }
        ));
    }

    #[test]
    fn zero_tick_hz_is_rejected() {
        let yaml = r#"
agent: { id: 1, vocabulary: v.yaml }
transport: { kind: loopback }
runtime: { tick_hz: 0 }
"#;
        let err = EngineConfig::from_yaml(yaml).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::NonPositive {
                field: "runtime.tick_hz",
                ..
            }
        ));
    }

    #[test]
    fn missing_required_field_fails() {
        // `agent` is required — no default.
        let yaml = r#"
transport: { kind: loopback }
"#;
        let err = EngineConfig::from_yaml(yaml).unwrap_err();
        assert!(matches!(err, ConfigError::Yaml(_)));
    }

    #[test]
    fn unknown_transport_kind_fails() {
        let yaml = r#"
agent: { id: 1, vocabulary: v.yaml }
transport: { kind: carrier_pigeon }
"#;
        let err = EngineConfig::from_yaml(yaml).unwrap_err();
        assert!(matches!(err, ConfigError::Yaml(_)));
    }

    #[test]
    fn from_file_reads_example_config() {
        // Locate the workspace example at $CARGO_MANIFEST_DIR/../../examples/.
        // The CARGO_MANIFEST_DIR of this crate is crates/umwelt-cli.
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let path = manifest
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("examples/configs/loopback_agent.yaml");
        let cfg = EngineConfig::from_file(&path).expect("example config must load");
        assert_eq!(cfg.transport, TransportConfig::Loopback);
    }
}

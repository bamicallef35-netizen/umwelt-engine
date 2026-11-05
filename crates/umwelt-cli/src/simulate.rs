//! `umwelt simulate` — spawn N agents on a shared loopback mesh.
//!
//! Each agent runs its own [`SemioticPipeline`] fed a deterministic
//! per-agent signal. Agents share a [`LoopbackMesh`] so future
//! iterations can exchange SEP frames between them; v1 drains the
//! inbox each tick but does not yet broadcast pipeline symbols.
//!
//! When the config's transport is [`TransportConfig::Loopback`] the
//! mesh is used raw. When it's [`TransportConfig::SimLoraOverLoopback`]
//! each agent's endpoint is wrapped in a [`SimLora`] fault injector,
//! seeded deterministically from `(seed ^ agent_id)` so reruns are
//! byte-identical. [`TransportConfig::Udp`] is rejected — simulate
//! runs in-process by design.
//!
//! The supported agent counts are pinned at `{2, 4, 8}`, multiplied
//! against the Small/Medium pipeline profile table (ADR-0020). All
//! other combinations return [`SimulateError::UnsupportedAgentCount`]
//! or the existing [`SimulateError::UnsupportedProfile`].

use std::cell::RefCell;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::Ordering;

use umwelt_core::replay::{PipelineEvent, SemioticPipeline};
use umwelt_core::Q15;
use umwelt_sep::{FrameForm, SepTransport, SepTransportError, SEP_ENVELOPE_BYTES};
use umwelt_transport::mesh::LoopbackMesh;
use umwelt_transport::sim_lora::SimLora;
use umwelt_vocab::SignVocabulary;

use crate::config::{EngineConfig, SimLoraPreset, TransportConfig};
use crate::events::{EventRecord, EventSink, NullSink, WriterSink};
use crate::metrics::{MetricsServer, MissionMetrics};
use crate::snapshot::{GraphSnapshot, SnapshotError};

// ── Fabric constants ──────────────────────────────────────────────────────

/// Per-agent mesh-inbox capacity (frames). Matches the P4.4 swarm test.
const MESH_QUEUE: usize = 64;

/// Per-agent SimLora delivery-queue capacity (frames).
const SIM_QUEUE: usize = 64;

// ── Errors ────────────────────────────────────────────────────────────────

/// Errors that can occur during `umwelt simulate`.
#[derive(Debug, thiserror::Error)]
pub enum SimulateError {
    /// Config file could not be loaded or validated.
    #[error("config: {0}")]
    Config(#[from] crate::config::ConfigError),

    /// Vocabulary file could not be loaded or validated.
    #[error("vocabulary: {0}")]
    Vocab(#[from] umwelt_vocab::VocabError),

    /// `agents` was not in the supported set.
    #[error("unsupported agent count {got}; supported: 2, 4, 8")]
    UnsupportedAgentCount {
        /// The value the caller asked for.
        got: usize,
    },

    /// The `(channels, icon_window, icon_bins)` tuple does not match
    /// a compiled pipeline profile (ADR-0020).
    #[error(
        "unsupported pipeline profile ({channels}/{window}/{bins}); \
         supported: 4/64/8 (small), 8/128/16 (medium)"
    )]
    UnsupportedProfile {
        channels: usize,
        window: usize,
        bins: usize,
    },

    /// UDP transport was requested — simulate is in-process only.
    #[error("transport.kind=udp is not supported under `umwelt simulate`; use loopback or sim_lora_over_loopback")]
    UdpUnsupported,

    /// I/O failure while writing a per-agent snapshot file.
    #[error("snapshot: {0}")]
    Snapshot(#[from] SnapshotError),

    /// Filesystem error creating the snapshot-output directory.
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
}

// ── Mesh endpoint adapter ─────────────────────────────────────────────────

/// A [`SepTransport`] view onto one slot of a shared [`LoopbackMesh`].
/// Mirrors the P4.4 swarm test's adapter — `send` broadcasts to every
/// other agent; `recv` drains this agent's inbox.
struct MeshEndpoint<const A: usize, const Q: usize> {
    mesh: Rc<RefCell<LoopbackMesh<A, Q>>>,
    my_id: usize,
}

impl<const A: usize, const Q: usize> SepTransport for MeshEndpoint<A, Q> {
    fn form(&self) -> FrameForm {
        self.mesh.borrow().form()
    }

    fn send(&mut self, frame: &[u8]) -> Result<(), SepTransportError> {
        self.mesh.borrow_mut().broadcast(self.my_id, frame)
    }

    fn recv(&mut self, buf: &mut [u8]) -> Result<usize, SepTransportError> {
        self.mesh.borrow_mut().recv(self.my_id, buf)
    }
}

// ── Per-agent transport ───────────────────────────────────────────────────

/// Wrapper that hides whether each agent's endpoint is a raw
/// [`MeshEndpoint`] or a `SimLora`-wrapped one. The `Sim` variant is
/// boxed because `SimLora` is ~3 KB and heap-allocating once per
/// agent beats fattening every enum to the largest variant.
enum AgentTransport<const A: usize, const Q: usize> {
    Raw(MeshEndpoint<A, Q>),
    Sim(Box<SimLora<MeshEndpoint<A, Q>, SIM_QUEUE>>),
}

impl<const A: usize, const Q: usize> AgentTransport<A, Q> {
    /// Drain one frame. SimLora needs `now_us` to honour its
    /// deliver-at latency queue; the raw mesh endpoint ignores it.
    fn recv(&mut self, buf: &mut [u8], now_us: u64) -> Result<usize, SepTransportError> {
        match self {
            Self::Raw(t) => t.recv(buf),
            Self::Sim(t) => t.recv_at(buf, now_us),
        }
    }

    fn form(&self) -> FrameForm {
        match self {
            Self::Raw(t) => t.form(),
            Self::Sim(t) => t.form(),
        }
    }
}

// ── Public entry point ────────────────────────────────────────────────────

/// Run a multi-agent simulation. Returns one [`GraphSnapshot`] per
/// agent, in agent-id order. If `snapshot_out_dir` is `Some(_)`, each
/// snapshot is also written to `<dir>/agent_<id>.json`. If
/// `events_out` is `Some(_)`, every [`PipelineEvent`] emitted by any
/// agent is serialised to that single NDJSON path — each line
/// carries its producing `agent_id`. If `metrics_bind` is `Some(_)`, a
/// single Prometheus metrics endpoint is spawned on that address and
/// counts events aggregated across every agent for the duration of the
/// simulation; otherwise falls back to `runtime.metrics_bind` in the
/// config YAML.
pub fn execute(
    config_path: &Path,
    agents: usize,
    snapshot_out_dir: Option<&Path>,
    events_out: Option<&Path>,
    metrics_bind: Option<&str>,
) -> Result<Vec<GraphSnapshot>, SimulateError> {
    let cfg = EngineConfig::from_file(config_path)?;

    if matches!(cfg.transport, TransportConfig::Udp { .. }) {
        return Err(SimulateError::UdpUnsupported);
    }

    let vocab = SignVocabulary::from_file(&cfg.agent.vocabulary)?;
    tracing::info!(
        agents,
        vocab_name = %vocab.name,
        "simulate start"
    );

    let mut sink: Box<dyn EventSink> = match events_out {
        Some(path) => {
            let file = std::fs::File::create(path)?;
            let sink = WriterSink::new(BufWriter::new(file))?;
            tracing::info!(path = %path.display(), "events export open (shared across agents)");
            Box::new(sink)
        }
        None => Box::new(NullSink),
    };

    let (c, w, b) = (
        cfg.pipeline.channels,
        cfg.pipeline.icon_window,
        cfg.pipeline.icon_bins,
    );

    // Metrics endpoint: CLI flag overrides config YAML. One shared
    // Prometheus handle across all agents; labels identify the base id.
    let effective_bind = metrics_bind.or(cfg.runtime.metrics_bind.as_deref());
    let mission_metrics = MissionMetrics::new();
    let _metrics_server = match effective_bind {
        Some(addr) => {
            let labels = vec![
                ("kind".into(), "simulate".into()),
                ("base_agent_id".into(), cfg.agent.id.to_string()),
                ("agents".into(), agents.to_string()),
                ("profile".into(), format!("{c}/{w}/{b}")),
            ];
            let sock: std::net::SocketAddr = addr.parse().map_err(|e| {
                SimulateError::Io(std::io::Error::other(format!("metrics-bind: {e}")))
            })?;
            let server = MetricsServer::spawn(sock, mission_metrics.clone(), labels)?;
            tracing::info!(addr = %server.addr(), "metrics endpoint up");
            Some(server)
        }
        None => None,
    };

    let snapshots = match (agents, (c, w, b)) {
        (2, (4, 64, 8)) => {
            simulate_mission::<2, 4, 64, 8>(&cfg, &vocab, sink.as_mut(), &mission_metrics)
        }
        (2, (8, 128, 16)) => {
            simulate_mission::<2, 8, 128, 16>(&cfg, &vocab, sink.as_mut(), &mission_metrics)
        }
        (4, (4, 64, 8)) => {
            simulate_mission::<4, 4, 64, 8>(&cfg, &vocab, sink.as_mut(), &mission_metrics)
        }
        (4, (8, 128, 16)) => {
            simulate_mission::<4, 8, 128, 16>(&cfg, &vocab, sink.as_mut(), &mission_metrics)
        }
        (8, (4, 64, 8)) => {
            simulate_mission::<8, 4, 64, 8>(&cfg, &vocab, sink.as_mut(), &mission_metrics)
        }
        (8, (8, 128, 16)) => {
            simulate_mission::<8, 8, 128, 16>(&cfg, &vocab, sink.as_mut(), &mission_metrics)
        }
        (n, _) if !matches!(n, 2 | 4 | 8) => {
            return Err(SimulateError::UnsupportedAgentCount { got: n });
        }
        _ => {
            return Err(SimulateError::UnsupportedProfile {
                channels: c,
                window: w,
                bins: b,
            });
        }
    };

    sink.flush()?;

    if let Some(dir) = snapshot_out_dir {
        std::fs::create_dir_all(dir)?;
        for snap in &snapshots {
            let path = dir.join(format!("agent_{}.json", snap.agent_id));
            snap.write_to_file(&path)?;
            tracing::info!(
                agent_id = snap.agent_id,
                signs = snap.signs.len(),
                path = %path.display(),
                "per-agent snapshot written"
            );
        }
    }

    Ok(snapshots)
}

// ── Mission loop ──────────────────────────────────────────────────────────

fn simulate_mission<const A: usize, const C: usize, const W: usize, const B: usize>(
    cfg: &EngineConfig,
    vocab: &SignVocabulary,
    events_sink: &mut dyn EventSink,
    metrics: &MissionMetrics,
) -> Vec<GraphSnapshot> {
    let mesh = Rc::new(RefCell::new(LoopbackMesh::<A, MESH_QUEUE>::new_inner()));

    // One transport + pipeline + snapshot per agent.
    let mut transports: Vec<AgentTransport<A, MESH_QUEUE>> = Vec::with_capacity(A);
    let mut pipelines: Vec<SemioticPipeline<C, W, B>> = Vec::with_capacity(A);
    let mut snapshots: Vec<GraphSnapshot> = Vec::with_capacity(A);

    for i in 0..A {
        let endpoint = MeshEndpoint {
            mesh: mesh.clone(),
            my_id: i,
        };
        let transport = match &cfg.transport {
            TransportConfig::Loopback => AgentTransport::Raw(endpoint),
            TransportConfig::SimLoraOverLoopback { preset, seed } => {
                let per_agent_seed = seed ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
                let sim = match preset {
                    SimLoraPreset::Clean => SimLora::clean(endpoint),
                    SimLoraPreset::Lossy => SimLora::lossy(endpoint, per_agent_seed),
                    SimLoraPreset::Fringe => SimLora::fringe(endpoint, per_agent_seed),
                };
                AgentTransport::Sim(Box::new(sim))
            }
            TransportConfig::Udp { .. } => unreachable!("rejected above"),
        };
        transports.push(transport);
        pipelines.push(SemioticPipeline::<C, W, B>::new());

        // Each agent's snapshot carries a distinct agent_id so output
        // files don't collide. We fan out from cfg.agent.id so the
        // operator keeps a clear base.
        let agent_id = cfg.agent.id.wrapping_add(i as u32);
        snapshots.push(GraphSnapshot::new(agent_id, vocab.name.clone()));
    }

    let total_ticks = cfg.runtime.tick_hz as u64 * cfg.runtime.duration_s as u64;
    let tick_interval_us = 1_000_000 / cfg.runtime.tick_hz as u64;

    let profile = format!("{C}/{W}/{B}");
    let mission_span = tracing::info_span!(
        "mission",
        kind = "simulate",
        base_agent_id = cfg.agent.id,
        agents = A,
        profile = %profile,
        ticks = total_ticks,
    );
    let _mission_guard = mission_span.enter();

    tracing::info!(
        tick_hz = cfg.runtime.tick_hz,
        duration_s = cfg.runtime.duration_s,
        "simulate mission start"
    );

    for snap in snapshots.iter_mut() {
        snap.mission_start_us = 0;
    }

    let mut recv_buf = [0u8; SEP_ENVELOPE_BYTES];

    for tick in 0..total_ticks {
        metrics.ticks.fetch_add(1, Ordering::Relaxed);
        let now_us = tick * tick_interval_us;

        // ── Send phase: advance each agent's pipeline ─────────────
        for i in 0..A {
            // Per-agent phase keeps the signals distinct so identical
            // pipelines do not produce identical snapshots.
            let phase = (i as u64 + tick) % 2;
            let sample = if phase == 0 {
                Q15::from_raw(18_000)
            } else {
                Q15::from_raw(-18_000)
            };
            let snap = &mut snapshots[i];
            let agent_id = snap.agent_id;
            let _ = pipelines[i].observe(0, sample, now_us, |evt| {
                match evt {
                    PipelineEvent::Icon(_) => {
                        metrics.icons.fetch_add(1, Ordering::Relaxed);
                        tracing::debug!(agent = i, tick, "icon");
                    }
                    PipelineEvent::Index(_) => {
                        metrics.indices.fetch_add(1, Ordering::Relaxed);
                        tracing::debug!(agent = i, tick, "index");
                    }
                    PipelineEvent::Symbol(_) => {
                        metrics.symbols.fetch_add(1, Ordering::Relaxed);
                        tracing::info!(agent = i, tick, "symbol");
                    }
                }
                snap.push_event(&evt);
                let _ = events_sink.write_event(&EventRecord::from_pipeline(agent_id, &evt));
            });
        }

        // ── Receive phase: drain inbound frames ────────────────────
        for t in transports.iter_mut() {
            let frame_bytes = t.form().bytes();
            loop {
                match t.recv(&mut recv_buf[..frame_bytes], now_us) {
                    Ok(_) => {
                        metrics.recv_frames.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(SepTransportError::WouldBlock) => break,
                    Err(_) => break,
                }
            }
        }
    }

    let mission_end_us = total_ticks.saturating_sub(1) * tick_interval_us;
    for snap in snapshots.iter_mut() {
        snap.mission_end_us = mission_end_us;
    }

    tracing::info!(
        agents = A,
        total_signs = snapshots.iter().map(|s| s.signs.len()).sum::<usize>(),
        "simulate mission complete"
    );

    snapshots
}

// ── Convenience for binary wiring ────────────────────────────────────────

/// Version of [`execute`] that accepts owned [`PathBuf`]s. Keeps the
/// main.rs wiring one line long.
pub fn execute_owned(
    config_path: &Path,
    agents: usize,
    snapshot_out_dir: Option<PathBuf>,
    events_out: Option<PathBuf>,
    metrics_bind: Option<&str>,
) -> Result<Vec<GraphSnapshot>, SimulateError> {
    execute(
        config_path,
        agents,
        snapshot_out_dir.as_deref(),
        events_out.as_deref(),
        metrics_bind,
    )
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, PipelineConfig, RuntimeConfig};

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn short_cfg(transport: TransportConfig) -> EngineConfig {
        EngineConfig {
            agent: AgentConfig {
                id: 100,
                vocabulary: workspace_root().join("vocabularies/swarm.yaml"),
            },
            pipeline: PipelineConfig::default(),
            transport,
            runtime: RuntimeConfig {
                tick_hz: 10,
                duration_s: 1,
                metrics_bind: None,
            },
        }
    }

    #[test]
    fn two_loopback_agents_produce_two_snapshots() {
        let cfg = short_cfg(TransportConfig::Loopback);
        let vocab = SignVocabulary::from_file(&cfg.agent.vocabulary).unwrap();
        let mut sink = NullSink;
        let metrics = MissionMetrics::new();
        let snaps = simulate_mission::<2, 4, 64, 8>(&cfg, &vocab, &mut sink, &metrics);
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].agent_id, 100);
        assert_eq!(snaps[1].agent_id, 101);
        assert_eq!(snaps[0].vocabulary, vocab.name);
    }

    #[test]
    fn sim_lora_wrapper_runs_without_panic() {
        let cfg = short_cfg(TransportConfig::SimLoraOverLoopback {
            preset: SimLoraPreset::Lossy,
            seed: 42,
        });
        let vocab = SignVocabulary::from_file(&cfg.agent.vocabulary).unwrap();
        let mut sink = NullSink;
        let metrics = MissionMetrics::new();
        let snaps = simulate_mission::<4, 4, 64, 8>(&cfg, &vocab, &mut sink, &metrics);
        assert_eq!(snaps.len(), 4);
    }

    #[test]
    fn execute_rejects_udp_transport() {
        let cfg = short_cfg(TransportConfig::Udp {
            bind: "0.0.0.0:4242".into(),
            peer: "10.0.0.2:4242".into(),
        });
        let cfg_yaml = cfg.to_yaml().unwrap();
        let cfg_path = std::env::temp_dir().join("umwelt-cli-sim-udp.yaml");
        std::fs::write(&cfg_path, cfg_yaml).unwrap();
        let err = execute(&cfg_path, 4, None, None, None).unwrap_err();
        let _ = std::fs::remove_file(&cfg_path);
        assert!(matches!(err, SimulateError::UdpUnsupported));
    }

    #[test]
    fn execute_rejects_bad_agent_count() {
        let cfg = short_cfg(TransportConfig::Loopback);
        let cfg_yaml = cfg.to_yaml().unwrap();
        let cfg_path = std::env::temp_dir().join("umwelt-cli-sim-badn.yaml");
        std::fs::write(&cfg_path, cfg_yaml).unwrap();
        let err = execute(&cfg_path, 7, None, None, None).unwrap_err();
        let _ = std::fs::remove_file(&cfg_path);
        assert!(matches!(
            err,
            SimulateError::UnsupportedAgentCount { got: 7 }
        ));
    }

    #[test]
    fn execute_writes_per_agent_snapshots_to_dir() {
        let cfg = short_cfg(TransportConfig::Loopback);
        let cfg_yaml = cfg.to_yaml().unwrap();
        let cfg_path = std::env::temp_dir().join("umwelt-cli-sim-wr.yaml");
        std::fs::write(&cfg_path, cfg_yaml).unwrap();

        let out_dir = std::env::temp_dir().join("umwelt-cli-sim-snapshots");
        let _ = std::fs::remove_dir_all(&out_dir);

        let snaps = execute(&cfg_path, 2, Some(&out_dir), None, None).unwrap();
        assert_eq!(snaps.len(), 2);

        let snap_a = GraphSnapshot::read_from_file(&out_dir.join("agent_100.json")).unwrap();
        let snap_b = GraphSnapshot::read_from_file(&out_dir.join("agent_101.json")).unwrap();
        assert_eq!(snap_a.agent_id, 100);
        assert_eq!(snap_b.agent_id, 101);

        let _ = std::fs::remove_file(&cfg_path);
        let _ = std::fs::remove_dir_all(&out_dir);
    }

    #[test]
    fn medium_profile_dispatches_under_simulate() {
        let cfg = EngineConfig {
            agent: AgentConfig {
                id: 50,
                vocabulary: workspace_root().join("vocabularies/swarm.yaml"),
            },
            pipeline: PipelineConfig {
                channels: 8,
                icon_window: 128,
                icon_bins: 16,
            },
            transport: TransportConfig::Loopback,
            runtime: RuntimeConfig {
                tick_hz: 10,
                duration_s: 1,
                metrics_bind: None,
            },
        };
        let vocab = SignVocabulary::from_file(&cfg.agent.vocabulary).unwrap();
        let mut sink = NullSink;
        let metrics = MissionMetrics::new();
        let snaps = simulate_mission::<4, 8, 128, 16>(&cfg, &vocab, &mut sink, &metrics);
        assert_eq!(snaps.len(), 4);
    }

    #[test]
    fn deterministic_across_runs_given_same_seed() {
        // Two identical simulate_mission calls produce identical
        // snapshots — no stray wall-clock or OS entropy leaks in.
        let cfg = short_cfg(TransportConfig::SimLoraOverLoopback {
            preset: SimLoraPreset::Lossy,
            seed: 0xBEEF_CAFE,
        });
        let vocab = SignVocabulary::from_file(&cfg.agent.vocabulary).unwrap();
        let mut sink_a = NullSink;
        let mut sink_b = NullSink;
        let metrics_a = MissionMetrics::new();
        let metrics_b = MissionMetrics::new();
        let a = simulate_mission::<2, 4, 64, 8>(&cfg, &vocab, &mut sink_a, &metrics_a);
        let b = simulate_mission::<2, 4, 64, 8>(&cfg, &vocab, &mut sink_b, &metrics_b);
        assert_eq!(a, b);
        assert_eq!(
            metrics_a.ticks.load(Ordering::Relaxed),
            metrics_b.ticks.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn simulate_events_out_captures_all_agents() {
        use crate::events::{EventRecord, EVENTS_HEADER};
        let cfg = short_cfg(TransportConfig::Loopback);
        let cfg_yaml = cfg.to_yaml().unwrap();
        let cfg_path = std::env::temp_dir().join("umwelt-cli-sim-events-cfg.yaml");
        std::fs::write(&cfg_path, cfg_yaml).unwrap();

        let events_out = std::env::temp_dir().join("umwelt-cli-sim-events.ndjson");
        let _ = std::fs::remove_file(&events_out);

        let snaps = execute(&cfg_path, 2, None, Some(&events_out), None).unwrap();
        let body = std::fs::read_to_string(&events_out).unwrap();

        let _ = std::fs::remove_file(&cfg_path);
        let _ = std::fs::remove_file(&events_out);

        let mut lines = body.lines();
        assert_eq!(lines.next(), Some(EVENTS_HEADER));
        let parsed: Vec<EventRecord> = lines.map(|l| serde_json::from_str(l).unwrap()).collect();
        // Every agent_id in the sink must be one of the snapshot ids.
        let valid_ids: std::collections::HashSet<u32> = snaps.iter().map(|s| s.agent_id).collect();
        for rec in &parsed {
            assert!(
                valid_ids.contains(&rec.agent_id),
                "unexpected agent_id {}; valid: {valid_ids:?}",
                rec.agent_id
            );
        }
    }

    #[test]
    fn simulate_mission_increments_tick_counter_once_per_tick() {
        // ticks counter is shared across agents, not per-agent.
        let cfg = short_cfg(TransportConfig::Loopback);
        let vocab = SignVocabulary::from_file(&cfg.agent.vocabulary).unwrap();
        let mut sink = NullSink;
        let metrics = MissionMetrics::new();
        let _ = simulate_mission::<4, 4, 64, 8>(&cfg, &vocab, &mut sink, &metrics);
        let expected_ticks = cfg.runtime.tick_hz as u64 * cfg.runtime.duration_s as u64;
        assert_eq!(metrics.ticks.load(Ordering::Relaxed), expected_ticks);
    }
}

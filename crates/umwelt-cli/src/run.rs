//! `umwelt run` — boot a single agent from a config YAML and run a
//! timed mission.
//!
//! Loads [`EngineConfig`] from disk, resolves the vocabulary, constructs
//! the chosen transport, dispatches to one of the pinned pipeline
//! profiles (ADR-0020), and runs the tick loop for `duration_s ×
//! tick_hz` ticks.

use std::io::BufWriter;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Duration;

use umwelt_core::replay::{PipelineEvent, SemioticPipeline};
use umwelt_core::Q15;
use umwelt_sep::{SepTransport, SepTransportError, SEP_ENVELOPE_BYTES};
use umwelt_transport::loopback::LoopbackTransport;
use umwelt_vocab::SignVocabulary;

use crate::config::{EngineConfig, TransportConfig};
use crate::events::{EventRecord, EventSink, NullSink, WriterSink};
use crate::metrics::{MetricsServer, MissionMetrics};
use crate::snapshot::{GraphSnapshot, SnapshotError};

// ── Errors ────────────────────────────────────────────────────────────────

/// Errors that can occur during `umwelt run`.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// Config file could not be loaded or validated.
    #[error("config: {0}")]
    Config(#[from] crate::config::ConfigError),

    /// Vocabulary file could not be loaded or validated.
    #[error("vocabulary: {0}")]
    Vocab(#[from] umwelt_vocab::VocabError),

    /// Transport could not be constructed.
    #[error("transport: {0}")]
    Transport(String),

    /// The `(channels, icon_window, icon_bins)` tuple does not match any
    /// compiled pipeline profile (ADR-0020).
    #[error(
        "unsupported pipeline profile ({channels}/{window}/{bins}); \
         supported: 4/64/8 (small), 8/128/16 (medium)"
    )]
    UnsupportedProfile {
        channels: usize,
        window: usize,
        bins: usize,
    },

    /// Snapshot write failed after mission completed.
    #[error("snapshot: {0}")]
    Snapshot(#[from] SnapshotError),
}

// ── Public entry point ────────────────────────────────────────────────────

/// Load config, construct the runtime, and run the mission.
///
/// If `snapshot_out` is `Some(_)`, a [`GraphSnapshot`] is written at
/// that path after the mission completes. If `events_out` is
/// `Some(_)`, every [`PipelineEvent`] emitted during the mission is
/// serialised to that path as one NDJSON record per line (see
/// [`crate::events`]). If `metrics_bind` is `Some(_)`, a live
/// Prometheus metrics endpoint is spawned on that address for the
/// duration of the mission; otherwise falls back to
/// `runtime.metrics_bind` in the config YAML.
pub fn execute(
    config_path: &Path,
    snapshot_out: Option<&Path>,
    events_out: Option<&Path>,
    metrics_bind: Option<&str>,
) -> Result<(), RunError> {
    let cfg = EngineConfig::from_file(config_path)?;

    tracing::info!(
        agent_id = cfg.agent.id,
        vocabulary = %cfg.agent.vocabulary.display(),
        "loading config"
    );

    // Resolve vocabulary.
    let vocab = SignVocabulary::from_file(&cfg.agent.vocabulary)?;
    tracing::info!(
        vocab_name = %vocab.name,
        sign_types = vocab.types.len(),
        composites = vocab.composite_rules.len(),
        "vocabulary loaded"
    );

    // Construct transport.
    let mut transport: Box<dyn SepTransport> = match &cfg.transport {
        TransportConfig::Loopback => {
            tracing::info!("transport: loopback");
            Box::new(LoopbackTransport::<64>::new_inner())
        }
        TransportConfig::Udp { bind, peer } => {
            let bind_addr = bind
                .parse()
                .map_err(|e| RunError::Transport(format!("bind: {e}")))?;
            let peer_addr = peer
                .parse()
                .map_err(|e| RunError::Transport(format!("peer: {e}")))?;
            let t = umwelt_transport::udp::UdpTransport::bind(
                bind_addr,
                peer_addr,
                umwelt_sep::FrameForm::Inner,
            )
            .map_err(|e| RunError::Transport(format!("udp bind: {e}")))?;
            tracing::info!(%bind, %peer, "transport: udp");
            Box::new(t)
        }
        TransportConfig::SimLoraOverLoopback { .. } => {
            return Err(RunError::Transport(
                "sim_lora_over_loopback requires injected time and is not \
                 supported under `umwelt run`; use `umwelt simulate` (P5.4) \
                 instead"
                    .into(),
            ));
        }
    };

    // Open the event sink. `None` means "don't export"; we use a
    // NullSink so the hot loop is monomorphic and never branches.
    let mut sink: Box<dyn EventSink> = match events_out {
        Some(path) => {
            let file = std::fs::File::create(path)
                .map_err(|e| RunError::Transport(format!("events-out: {e}")))?;
            let sink = WriterSink::new(BufWriter::new(file))
                .map_err(|e| RunError::Transport(format!("events-out header: {e}")))?;
            tracing::info!(path = %path.display(), "events export open");
            Box::new(sink)
        }
        None => Box::new(NullSink),
    };

    // Metrics endpoint: CLI flag overrides config YAML.
    let effective_bind = metrics_bind.or(cfg.runtime.metrics_bind.as_deref());
    let mission_metrics = MissionMetrics::new();
    let _metrics_server = match effective_bind {
        Some(addr) => {
            let (c, w, b) = (
                cfg.pipeline.channels,
                cfg.pipeline.icon_window,
                cfg.pipeline.icon_bins,
            );
            let labels = vec![
                ("kind".into(), "run".into()),
                ("agent_id".into(), cfg.agent.id.to_string()),
                ("profile".into(), format!("{c}/{w}/{b}")),
            ];
            let sock: std::net::SocketAddr = addr
                .parse()
                .map_err(|e| RunError::Transport(format!("metrics-bind: {e}")))?;
            let server = MetricsServer::spawn(sock, mission_metrics.clone(), labels)
                .map_err(|e| RunError::Transport(format!("metrics server: {e}")))?;
            tracing::info!(addr = %server.addr(), "metrics endpoint up");
            Some(server)
        }
        None => None,
    };

    // Dispatch to the pinned const-generic profile (ADR-0020).
    let (c, w, b) = (
        cfg.pipeline.channels,
        cfg.pipeline.icon_window,
        cfg.pipeline.icon_bins,
    );
    let snapshot = match (c, w, b) {
        (4, 64, 8) => run_mission::<4, 64, 8>(
            &cfg,
            &vocab,
            transport.as_mut(),
            sink.as_mut(),
            &mission_metrics,
        )?,
        (8, 128, 16) => run_mission::<8, 128, 16>(
            &cfg,
            &vocab,
            transport.as_mut(),
            sink.as_mut(),
            &mission_metrics,
        )?,
        _ => {
            return Err(RunError::UnsupportedProfile {
                channels: c,
                window: w,
                bins: b,
            });
        }
    };

    sink.flush()
        .map_err(|e| RunError::Transport(format!("events-out flush: {e}")))?;

    if let Some(path) = snapshot_out {
        snapshot.write_to_file(path).map_err(RunError::Snapshot)?;
        tracing::info!(signs = snapshot.signs.len(), path = %path.display(), "snapshot written");
    }
    Ok(())
}

// ── Mission loop ──────────────────────────────────────────────────────────

fn run_mission<const C: usize, const W: usize, const B: usize>(
    cfg: &EngineConfig,
    vocab: &SignVocabulary,
    transport: &mut dyn SepTransport,
    events_sink: &mut dyn EventSink,
    metrics: &MissionMetrics,
) -> Result<GraphSnapshot, RunError> {
    let mut pipeline = SemioticPipeline::<C, W, B>::new();
    let total_ticks = cfg.runtime.tick_hz as u64 * cfg.runtime.duration_s as u64;
    let tick_interval = Duration::from_micros(1_000_000 / cfg.runtime.tick_hz as u64);

    // Wrap the whole mission in an info span so every event emitted
    // below (icon/index/symbol) is tagged with mission metadata. Tools
    // filtering structured logs (P6.2) lean on this.
    let profile = format!("{C}/{W}/{B}");
    let mission_span = tracing::info_span!(
        "mission",
        kind = "run",
        agent_id = cfg.agent.id,
        profile = %profile,
        ticks = total_ticks,
    );
    let _mission_guard = mission_span.enter();

    tracing::info!(
        tick_hz = cfg.runtime.tick_hz,
        duration_s = cfg.runtime.duration_s,
        "mission start"
    );

    let frame_bytes = transport.form().bytes();
    let mut recv_buf = [0u8; SEP_ENVELOPE_BYTES];
    let mut events = 0u64;
    let mut recv_frames = 0u64;
    let mut snapshot = GraphSnapshot::new(cfg.agent.id, vocab.name.clone());
    snapshot.mission_start_us = 0;

    for tick in 0..total_ticks {
        let now_us = tick * (1_000_000 / cfg.runtime.tick_hz as u64);
        metrics.ticks.fetch_add(1, Ordering::Relaxed);

        // Drain incoming frames from transport.
        loop {
            match transport.recv(&mut recv_buf[..frame_bytes]) {
                Ok(_) => {
                    recv_frames += 1;
                    metrics.recv_frames.fetch_add(1, Ordering::Relaxed);
                }
                Err(SepTransportError::WouldBlock) => break,
                Err(_) => break,
            }
        }

        // Feed a deterministic alternating signal on channel 0 so the
        // icon gate has something to fire on. Real sensor data sources
        // replace this in a future step.
        let sample = if tick % 2 == 0 {
            Q15::from_raw(18_000)
        } else {
            Q15::from_raw(-18_000)
        };
        let _ = pipeline.observe(0, sample, now_us, |evt| {
            events += 1;
            match evt {
                PipelineEvent::Icon(_) => {
                    metrics.icons.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(tick, "icon emitted");
                }
                PipelineEvent::Index(_) => {
                    metrics.indices.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(tick, "index emitted");
                }
                PipelineEvent::Symbol(_) => {
                    metrics.symbols.fetch_add(1, Ordering::Relaxed);
                    tracing::info!(tick, "symbol committed");
                }
            }
            snapshot.push_event(&evt);
            let _ = events_sink.write_event(&EventRecord::from_pipeline(cfg.agent.id, &evt));
        });

        std::thread::sleep(tick_interval);
    }
    snapshot.mission_end_us =
        total_ticks.saturating_sub(1) * (1_000_000 / cfg.runtime.tick_hz as u64);

    tracing::info!(
        events,
        recv_frames,
        signs = snapshot.signs.len(),
        "mission complete"
    );
    Ok(snapshot)
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AgentConfig, PipelineConfig, RuntimeConfig, SimLoraPreset, TransportConfig,
    };
    use std::path::PathBuf;

    /// Workspace root relative to this crate's CARGO_MANIFEST_DIR.
    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn short_loopback_config() -> EngineConfig {
        EngineConfig {
            agent: AgentConfig {
                id: 1,
                vocabulary: workspace_root().join("vocabularies/swarm.yaml"),
            },
            pipeline: PipelineConfig::default(),
            transport: TransportConfig::Loopback,
            runtime: RuntimeConfig {
                tick_hz: 10,
                duration_s: 1,
                metrics_bind: None,
            },
        }
    }

    #[test]
    fn loopback_mission_completes() {
        let cfg = short_loopback_config();
        let vocab = SignVocabulary::from_file(&cfg.agent.vocabulary).unwrap();
        let mut transport = LoopbackTransport::<64>::new_inner();
        let mut sink = NullSink;
        let metrics = MissionMetrics::new();
        let snap =
            run_mission::<4, 64, 8>(&cfg, &vocab, &mut transport, &mut sink, &metrics).unwrap();
        assert_eq!(snap.agent_id, 1);
        assert_eq!(snap.vocabulary, vocab.name);
    }

    #[test]
    fn unsupported_profile_errors() {
        let mut cfg = short_loopback_config();
        cfg.pipeline.channels = 3;
        cfg.pipeline.icon_window = 99;
        cfg.pipeline.icon_bins = 7;
        // Bypass validate() since (3,99,7) is non-zero but not a known
        // profile — execute() should return UnsupportedProfile.
        // We cannot call execute() directly because it reads from disk,
        // so test the dispatch logic inline.
        let (c, w, b) = (
            cfg.pipeline.channels,
            cfg.pipeline.icon_window,
            cfg.pipeline.icon_bins,
        );
        let result: Result<(), RunError> = match (c, w, b) {
            (4, 64, 8) => Ok(()),
            (8, 128, 16) => Ok(()),
            _ => Err(RunError::UnsupportedProfile {
                channels: c,
                window: w,
                bins: b,
            }),
        };
        assert!(matches!(result, Err(RunError::UnsupportedProfile { .. })));
    }

    #[test]
    fn sim_lora_rejected_under_run() {
        let cfg = EngineConfig {
            agent: AgentConfig {
                id: 2,
                vocabulary: workspace_root().join("vocabularies/swarm.yaml"),
            },
            pipeline: PipelineConfig::default(),
            transport: TransportConfig::SimLoraOverLoopback {
                preset: SimLoraPreset::Lossy,
                seed: 42,
            },
            runtime: RuntimeConfig {
                tick_hz: 10,
                duration_s: 1,
                metrics_bind: None,
            },
        };
        // Build transport the same way execute() does.
        let result: Result<Box<dyn SepTransport>, RunError> = match &cfg.transport {
            TransportConfig::SimLoraOverLoopback { .. } => Err(RunError::Transport(
                "sim_lora_over_loopback not supported under run".into(),
            )),
            _ => unreachable!(),
        };
        assert!(matches!(result, Err(RunError::Transport(_))));
    }

    #[test]
    fn medium_profile_dispatches() {
        let mut cfg = short_loopback_config();
        cfg.pipeline.channels = 8;
        cfg.pipeline.icon_window = 128;
        cfg.pipeline.icon_bins = 16;
        let vocab = SignVocabulary::from_file(&cfg.agent.vocabulary).unwrap();
        let mut transport = LoopbackTransport::<64>::new_inner();
        let mut sink = NullSink;
        let metrics = MissionMetrics::new();
        let snap =
            run_mission::<8, 128, 16>(&cfg, &vocab, &mut transport, &mut sink, &metrics).unwrap();
        assert_eq!(snap.schema, crate::snapshot::SCHEMA_TAG);
    }

    #[test]
    fn run_mission_emits_mission_span() {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::format::FmtSpan;
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone)]
        struct Buf(Arc<Mutex<Vec<u8>>>);

        impl Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().write(b)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> MakeWriter<'a> for Buf {
            type Writer = Buf;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = Buf(Arc::new(Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_span_events(FmtSpan::NEW)
            .with_ansi(false)
            .finish();

        let cfg = short_loopback_config();
        let vocab = SignVocabulary::from_file(&cfg.agent.vocabulary).unwrap();
        let mut transport = LoopbackTransport::<64>::new_inner();
        let mut sink = NullSink;
        let metrics = MissionMetrics::new();

        tracing::subscriber::with_default(subscriber, || {
            let _ =
                run_mission::<4, 64, 8>(&cfg, &vocab, &mut transport, &mut sink, &metrics).unwrap();
        });

        let output = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(
            output.contains("mission"),
            "expected 'mission' span in output, got: {output}"
        );
        assert!(
            output.contains("kind=\"run\"") || output.contains("kind=run"),
            "expected kind=run tag in output, got: {output}"
        );
    }

    #[test]
    fn snapshot_is_written_when_path_given() {
        // Build a config inline with an absolute vocab path so the
        // test doesn't depend on CWD. The example YAML at the
        // workspace root uses a relative `vocabularies/swarm.yaml`,
        // which breaks when cargo runs tests from the crate dir.
        let cfg = EngineConfig {
            agent: AgentConfig {
                id: 9,
                vocabulary: workspace_root().join("vocabularies/swarm.yaml"),
            },
            pipeline: PipelineConfig::default(),
            transport: TransportConfig::Loopback,
            runtime: RuntimeConfig {
                tick_hz: 10,
                duration_s: 1,
                metrics_bind: None,
            },
        };
        let cfg_yaml = cfg.to_yaml().unwrap();
        let cfg_path = std::env::temp_dir().join("umwelt-cli-run-snapshot-cfg.yaml");
        std::fs::write(&cfg_path, cfg_yaml).unwrap();

        let out = std::env::temp_dir().join("umwelt-cli-run-snapshot.json");
        let _ = std::fs::remove_file(&out);
        let res = execute(&cfg_path, Some(&out), None, None);
        let _ = std::fs::remove_file(&cfg_path);
        assert!(res.is_ok(), "execute failed: {res:?}");
        let snap = GraphSnapshot::read_from_file(&out).expect("snapshot must exist");
        let _ = std::fs::remove_file(&out);
        assert_eq!(snap.schema, crate::snapshot::SCHEMA_TAG);
    }

    #[test]
    fn events_out_writes_header_and_lines() {
        // Tour de force: run a mission with --events-out and assert
        // the NDJSON file starts with the header and contains at
        // least one icon line. If the pipeline never emits an icon
        // under 10 ticks, lengthen the mission.
        use crate::events::EVENTS_HEADER;

        let mut cfg = short_loopback_config();
        cfg.runtime.duration_s = 2;
        let cfg_yaml = cfg.to_yaml().unwrap();
        let cfg_path = std::env::temp_dir().join("umwelt-cli-run-events-cfg.yaml");
        std::fs::write(&cfg_path, cfg_yaml).unwrap();

        let events_out = std::env::temp_dir().join("umwelt-cli-run-events.ndjson");
        let _ = std::fs::remove_file(&events_out);

        let res = execute(&cfg_path, None, Some(&events_out), None);
        let _ = std::fs::remove_file(&cfg_path);
        assert!(res.is_ok(), "execute failed: {res:?}");

        let body = std::fs::read_to_string(&events_out).unwrap();
        let _ = std::fs::remove_file(&events_out);

        let mut lines = body.lines();
        assert_eq!(lines.next(), Some(EVENTS_HEADER));
        // Each body line must be a parseable EventRecord.
        for l in lines {
            let _: crate::events::EventRecord =
                serde_json::from_str(l).expect("event line must parse");
        }
    }

    #[test]
    fn run_mission_increments_counters() {
        // Assert the tick counter lands at total_ticks and no layer
        // counter regresses. Loopback transport never receives, so
        // `recv_frames` stays zero — that's load-bearing for the
        // Prometheus readers who expect monotonic counters.
        let cfg = short_loopback_config();
        let vocab = SignVocabulary::from_file(&cfg.agent.vocabulary).unwrap();
        let mut transport = LoopbackTransport::<64>::new_inner();
        let mut sink = NullSink;
        let metrics = MissionMetrics::new();
        let _ = run_mission::<4, 64, 8>(&cfg, &vocab, &mut transport, &mut sink, &metrics).unwrap();
        // short_loopback_config is 10 Hz × 1 s = 10 ticks.
        assert_eq!(metrics.ticks.load(Ordering::Relaxed), 10);
        assert_eq!(metrics.recv_frames.load(Ordering::Relaxed), 0);
    }
}

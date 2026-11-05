//! `umwelt replay` — feed a recorded sensor log through the
//! semiotic pipeline at log-native timestamps, emit a snapshot.
//!
//! Replay is the offline sibling of `umwelt run`. It shares the
//! pipeline dispatch and snapshot accumulation, but drops wall-clock
//! pacing: samples are fed at their recorded `tick_us`, as fast as
//! the reader can supply them. A log file carries a header tag on the
//! first line, followed by one JSON object per sample line:
//!
//! ```text
//! umwelt.sensor_log.v1
//! {"tick_us": 0, "channel": 0, "q15": 18000}
//! {"tick_us": 100, "channel": 0, "q15": -18000}
//! ...
//! ```
//!
//! Format rationale: a header line fails loudly if someone feeds the
//! wrong file; JSON-per-line keeps parsing trivial and composes with
//! `jq`. Values map 1:1 to `pipeline.observe(channel, Q15::from_raw(q15),
//! tick_us, …)`.
//!
//! Replay uses the pinned Small/Medium profile table (ADR-0020) so
//! the same `(channels, icon_window, icon_bins)` produced by `umwelt
//! run` is consumed by `umwelt replay`.

use std::io::{BufRead, BufReader, BufWriter};
use std::path::Path;
use std::sync::atomic::Ordering;

use serde::{Deserialize, Serialize};

use umwelt_core::replay::{PipelineEvent, SemioticPipeline};
use umwelt_core::Q15;
use umwelt_vocab::SignVocabulary;

use crate::config::EngineConfig;
use crate::events::{EventRecord, EventSink, NullSink, WriterSink};
use crate::metrics::{MetricsServer, MissionMetrics};
use crate::snapshot::{GraphSnapshot, SnapshotError};

/// Header tag that every sensor log must carry on its first line.
pub const LOG_HEADER: &str = "umwelt.sensor_log.v1";

// ── Errors ────────────────────────────────────────────────────────────────

/// Errors that can occur during `umwelt replay`.
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    /// Config file could not be loaded or validated.
    #[error("config: {0}")]
    Config(#[from] crate::config::ConfigError),

    /// Vocabulary file could not be loaded or validated.
    #[error("vocabulary: {0}")]
    Vocab(#[from] umwelt_vocab::VocabError),

    /// Log file I/O failed.
    #[error("log i/o: {0}")]
    Io(#[from] std::io::Error),

    /// Log header line was missing or did not match [`LOG_HEADER`].
    #[error("log header mismatch: expected {expected:?}, found {found:?}")]
    HeaderMismatch {
        /// First line of the log file, trimmed.
        found: String,
        /// The expected header tag.
        expected: &'static str,
    },

    /// A sample line couldn't be parsed.
    #[error("log line {line}: {source}")]
    BadSample {
        /// 1-based line number (including header).
        line: usize,
        /// Underlying JSON error.
        source: serde_json::Error,
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

    /// Snapshot write failed after replay completed.
    #[error("snapshot: {0}")]
    Snapshot(#[from] SnapshotError),
}

// ── Log record ────────────────────────────────────────────────────────────

/// One line of a sensor log. Units are microseconds for `tick_us`,
/// raw `i16` Q15 for `q15`, and channel index for `channel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SensorSample {
    /// Microseconds since agent epoch.
    pub tick_us: u64,
    /// Channel index; must be < `pipeline.channels`.
    pub channel: u16,
    /// Raw Q15 sample value.
    pub q15: i16,
}

// ── Public entry point ────────────────────────────────────────────────────

/// Load a config + sensor log, replay through the pipeline, return a
/// [`GraphSnapshot`]. If `snapshot_out` is `Some(_)`, the snapshot is
/// also written to that path. If `events_out` is `Some(_)`, every
/// [`PipelineEvent`] produced during replay is serialised as one
/// NDJSON line to that path (see [`crate::events`]). If
/// `metrics_bind` is `Some(_)`, a live Prometheus metrics endpoint is
/// spawned on that address for the duration of the replay; otherwise
/// falls back to `runtime.metrics_bind` in the config YAML.
pub fn execute(
    config_path: &Path,
    log_path: &Path,
    snapshot_out: Option<&Path>,
    events_out: Option<&Path>,
    metrics_bind: Option<&str>,
) -> Result<GraphSnapshot, ReplayError> {
    let cfg = EngineConfig::from_file(config_path)?;

    tracing::info!(
        agent_id = cfg.agent.id,
        log = %log_path.display(),
        "loading replay config"
    );

    let vocab = SignVocabulary::from_file(&cfg.agent.vocabulary)?;
    let samples = read_log(log_path)?;

    tracing::info!(
        samples = samples.len(),
        vocab_name = %vocab.name,
        "log loaded"
    );

    let mut sink: Box<dyn EventSink> = match events_out {
        Some(path) => {
            let file = std::fs::File::create(path)?;
            let sink = WriterSink::new(BufWriter::new(file))?;
            tracing::info!(path = %path.display(), "events export open");
            Box::new(sink)
        }
        None => Box::new(NullSink),
    };

    // Metrics endpoint: CLI flag overrides config YAML.
    let effective_bind = metrics_bind.or(cfg.runtime.metrics_bind.as_deref());
    let (c, w, b) = (
        cfg.pipeline.channels,
        cfg.pipeline.icon_window,
        cfg.pipeline.icon_bins,
    );
    let mission_metrics = MissionMetrics::new();
    let _metrics_server = match effective_bind {
        Some(addr) => {
            let labels = vec![
                ("kind".into(), "replay".into()),
                ("agent_id".into(), cfg.agent.id.to_string()),
                ("profile".into(), format!("{c}/{w}/{b}")),
            ];
            let sock: std::net::SocketAddr = addr.parse().map_err(|e| {
                ReplayError::Io(std::io::Error::other(format!("metrics-bind: {e}")))
            })?;
            let server = MetricsServer::spawn(sock, mission_metrics.clone(), labels)?;
            tracing::info!(addr = %server.addr(), "metrics endpoint up");
            Some(server)
        }
        None => None,
    };

    let snapshot = match (c, w, b) {
        (4, 64, 8) => {
            replay_mission::<4, 64, 8>(&cfg, &vocab, &samples, sink.as_mut(), &mission_metrics)
        }
        (8, 128, 16) => {
            replay_mission::<8, 128, 16>(&cfg, &vocab, &samples, sink.as_mut(), &mission_metrics)
        }
        _ => {
            return Err(ReplayError::UnsupportedProfile {
                channels: c,
                window: w,
                bins: b,
            });
        }
    };

    sink.flush()?;

    if let Some(path) = snapshot_out {
        snapshot
            .write_to_file(path)
            .map_err(ReplayError::Snapshot)?;
        tracing::info!(
            signs = snapshot.signs.len(),
            path = %path.display(),
            "snapshot written"
        );
    }
    Ok(snapshot)
}

// ── Log reader ────────────────────────────────────────────────────────────

/// Parse a sensor-log file. The first line must be the [`LOG_HEADER`]
/// tag; subsequent non-empty lines are JSON [`SensorSample`]s.
pub fn read_log(path: &Path) -> Result<Vec<SensorSample>, ReplayError> {
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut samples = Vec::new();
    let mut header_seen = false;

    for (idx, line) in reader.lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !header_seen {
            if trimmed != LOG_HEADER {
                return Err(ReplayError::HeaderMismatch {
                    found: trimmed.to_string(),
                    expected: LOG_HEADER,
                });
            }
            header_seen = true;
            continue;
        }
        let sample: SensorSample =
            serde_json::from_str(trimmed).map_err(|e| ReplayError::BadSample {
                line: idx + 1,
                source: e,
            })?;
        samples.push(sample);
    }

    if !header_seen {
        return Err(ReplayError::HeaderMismatch {
            found: String::new(),
            expected: LOG_HEADER,
        });
    }

    Ok(samples)
}

// ── Replay loop ───────────────────────────────────────────────────────────

fn replay_mission<const C: usize, const W: usize, const B: usize>(
    cfg: &EngineConfig,
    vocab: &SignVocabulary,
    samples: &[SensorSample],
    events_sink: &mut dyn EventSink,
    metrics: &MissionMetrics,
) -> GraphSnapshot {
    let mut pipeline = SemioticPipeline::<C, W, B>::new();
    let mut snapshot = GraphSnapshot::new(cfg.agent.id, vocab.name.clone());
    let mut events = 0u64;

    snapshot.mission_start_us = samples.first().map(|s| s.tick_us).unwrap_or(0);

    let profile = format!("{C}/{W}/{B}");
    let mission_span = tracing::info_span!(
        "mission",
        kind = "replay",
        agent_id = cfg.agent.id,
        profile = %profile,
        samples = samples.len(),
    );
    let _mission_guard = mission_span.enter();

    tracing::info!("replay start");

    for sample in samples {
        metrics.ticks.fetch_add(1, Ordering::Relaxed);
        // Channel overflow is silently ignored — pipeline.observe
        // will bounds-check and return an error we don't need.
        let _ = pipeline.observe(
            sample.channel,
            Q15::from_raw(sample.q15),
            sample.tick_us,
            |evt| {
                events += 1;
                match evt {
                    PipelineEvent::Icon(_) => {
                        metrics.icons.fetch_add(1, Ordering::Relaxed);
                        tracing::debug!("icon emitted");
                    }
                    PipelineEvent::Index(_) => {
                        metrics.indices.fetch_add(1, Ordering::Relaxed);
                        tracing::debug!("index emitted");
                    }
                    PipelineEvent::Symbol(_) => {
                        metrics.symbols.fetch_add(1, Ordering::Relaxed);
                        tracing::info!("symbol committed");
                    }
                }
                snapshot.push_event(&evt);
                let _ = events_sink.write_event(&EventRecord::from_pipeline(cfg.agent.id, &evt));
            },
        );
    }

    snapshot.mission_end_us = samples.last().map(|s| s.tick_us).unwrap_or(0);

    tracing::info!(events, signs = snapshot.signs.len(), "replay complete");
    snapshot
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, PipelineConfig, RuntimeConfig, TransportConfig};
    use std::path::PathBuf;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn write_tmp(name: &str, contents: &str) -> PathBuf {
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn read_log_parses_header_and_samples() {
        let body = "umwelt.sensor_log.v1\n\
                    {\"tick_us\":0,\"channel\":0,\"q15\":1000}\n\
                    {\"tick_us\":100,\"channel\":0,\"q15\":-1000}\n";
        let path = write_tmp("umwelt-cli-replay-good.ndjson", body);
        let samples = read_log(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].tick_us, 0);
        assert_eq!(samples[1].q15, -1000);
    }

    #[test]
    fn read_log_rejects_wrong_header() {
        let body = "not.the.right.header\n{\"tick_us\":0,\"channel\":0,\"q15\":0}\n";
        let path = write_tmp("umwelt-cli-replay-badhdr.ndjson", body);
        let err = read_log(&path).unwrap_err();
        let _ = std::fs::remove_file(&path);
        assert!(matches!(err, ReplayError::HeaderMismatch { .. }));
    }

    #[test]
    fn read_log_rejects_empty_file() {
        let path = write_tmp("umwelt-cli-replay-empty.ndjson", "");
        let err = read_log(&path).unwrap_err();
        let _ = std::fs::remove_file(&path);
        assert!(matches!(err, ReplayError::HeaderMismatch { .. }));
    }

    #[test]
    fn read_log_rejects_bad_sample_line() {
        let body = "umwelt.sensor_log.v1\n{not json}\n";
        let path = write_tmp("umwelt-cli-replay-badsample.ndjson", body);
        let err = read_log(&path).unwrap_err();
        let _ = std::fs::remove_file(&path);
        assert!(matches!(err, ReplayError::BadSample { line: 2, .. }));
    }

    #[test]
    fn read_log_tolerates_blank_lines() {
        let body = "umwelt.sensor_log.v1\n\n\
                    {\"tick_us\":0,\"channel\":0,\"q15\":5}\n\n";
        let path = write_tmp("umwelt-cli-replay-blanks.ndjson", body);
        let samples = read_log(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(samples.len(), 1);
    }

    fn short_cfg() -> EngineConfig {
        EngineConfig {
            agent: AgentConfig {
                id: 3,
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
    fn replay_mission_produces_snapshot_with_correct_bounds() {
        let cfg = short_cfg();
        let vocab = SignVocabulary::from_file(&cfg.agent.vocabulary).unwrap();
        let samples: Vec<SensorSample> = (0..20)
            .map(|i| SensorSample {
                tick_us: i * 100,
                channel: 0,
                q15: if i % 2 == 0 { 18_000 } else { -18_000 },
            })
            .collect();
        let mut sink = NullSink;
        let m = MissionMetrics::new();
        let snap = replay_mission::<4, 64, 8>(&cfg, &vocab, &samples, &mut sink, &m);
        assert_eq!(snap.agent_id, 3);
        assert_eq!(snap.mission_start_us, 0);
        assert_eq!(snap.mission_end_us, 1_900);
        assert_eq!(snap.vocabulary, vocab.name);
    }

    #[test]
    fn replay_mission_handles_empty_sample_list() {
        let cfg = short_cfg();
        let vocab = SignVocabulary::from_file(&cfg.agent.vocabulary).unwrap();
        let mut sink = NullSink;
        let m = MissionMetrics::new();
        let snap = replay_mission::<4, 64, 8>(&cfg, &vocab, &[], &mut sink, &m);
        assert_eq!(snap.mission_start_us, 0);
        assert_eq!(snap.mission_end_us, 0);
        assert!(snap.signs.is_empty());
    }

    #[test]
    fn execute_round_trips_through_file() {
        // Build config + log on disk with absolute paths; execute;
        // verify snapshot written and readable.
        let cfg = short_cfg();
        let cfg_yaml = cfg.to_yaml().unwrap();
        let cfg_path = write_tmp("umwelt-cli-replay-exec.yaml", &cfg_yaml);

        let mut body = String::from("umwelt.sensor_log.v1\n");
        for i in 0..20u64 {
            let q15 = if i % 2 == 0 { 18_000 } else { -18_000 };
            body.push_str(&format!(
                "{{\"tick_us\":{},\"channel\":0,\"q15\":{}}}\n",
                i * 100,
                q15
            ));
        }
        let log_path = write_tmp("umwelt-cli-replay-exec.ndjson", &body);

        let snap_out = std::env::temp_dir().join("umwelt-cli-replay-exec.json");
        let _ = std::fs::remove_file(&snap_out);

        let snap = execute(&cfg_path, &log_path, Some(&snap_out), None, None).unwrap();
        let on_disk = GraphSnapshot::read_from_file(&snap_out).unwrap();

        let _ = std::fs::remove_file(&cfg_path);
        let _ = std::fs::remove_file(&log_path);
        let _ = std::fs::remove_file(&snap_out);

        assert_eq!(snap, on_disk);
        assert_eq!(snap.schema, crate::snapshot::SCHEMA_TAG);
    }

    #[test]
    fn execute_rejects_missing_log() {
        let cfg = short_cfg();
        let cfg_yaml = cfg.to_yaml().unwrap();
        let cfg_path = write_tmp("umwelt-cli-replay-missinglog.yaml", &cfg_yaml);
        let err = execute(
            &cfg_path,
            Path::new("/no/such/log.ndjson"),
            None,
            None,
            None,
        )
        .unwrap_err();
        let _ = std::fs::remove_file(&cfg_path);
        assert!(matches!(err, ReplayError::Io(_)));
    }

    #[test]
    fn replay_events_out_writes_header_and_at_least_one_line() {
        use crate::events::EVENTS_HEADER;
        let cfg = short_cfg();
        let cfg_yaml = cfg.to_yaml().unwrap();
        let cfg_path = write_tmp("umwelt-cli-replay-events-cfg.yaml", &cfg_yaml);

        // Stream enough samples to trigger at least one icon.
        let mut body = String::from("umwelt.sensor_log.v1\n");
        for i in 0..200u64 {
            let q15 = if i % 2 == 0 { 18_000 } else { -18_000 };
            body.push_str(&format!(
                "{{\"tick_us\":{},\"channel\":0,\"q15\":{}}}\n",
                i * 100,
                q15
            ));
        }
        let log_path = write_tmp("umwelt-cli-replay-events-log.ndjson", &body);

        let events_out = std::env::temp_dir().join("umwelt-cli-replay-events.ndjson");
        let _ = std::fs::remove_file(&events_out);

        let snap = execute(&cfg_path, &log_path, None, Some(&events_out), None).unwrap();
        let body = std::fs::read_to_string(&events_out).unwrap();

        let _ = std::fs::remove_file(&cfg_path);
        let _ = std::fs::remove_file(&log_path);
        let _ = std::fs::remove_file(&events_out);

        let mut lines = body.lines();
        assert_eq!(lines.next(), Some(EVENTS_HEADER));
        let parsed: Vec<crate::events::EventRecord> =
            lines.map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(
            parsed.len(),
            snap.signs.len(),
            "NDJSON rows must match snapshot signs"
        );
    }
}

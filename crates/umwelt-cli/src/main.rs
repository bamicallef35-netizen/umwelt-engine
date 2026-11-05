//! `umwelt` — command-line harness for the Umwelt Engine.
//!
//! Subcommands map 1:1 to the P5 build-plan steps:
//! - `run`       — boot a single agent from config YAML (P5.1)
//! - `inspect`   — dump an Umwelt graph snapshot as JSON (P5.2)
//! - `replay`    — feed a sensor log through semiogenesis (P5.3)
//! - `simulate`  — spawn N agents over loopback transport (P5.4)

mod config;
mod events;
mod inspect;
mod metrics;
mod replay;
mod run;
mod simulate;
mod snapshot;

pub use config::{
    AgentConfig, ConfigError, EngineConfig, PipelineConfig, RuntimeConfig, SimLoraPreset,
    TransportConfig,
};
pub use events::{EventKind, EventRecord, EventSink, NullSink, WriterSink, EVENTS_HEADER};
pub use snapshot::{GraphSnapshot, SnapshotError};

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Umwelt Engine CLI — boot agents, inspect graphs, replay logs.
#[derive(Parser)]
#[command(name = "umwelt", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Boot a single agent with a config YAML and run a timed mission.
    Run {
        /// Path to the engine config YAML file.
        #[arg(short, long)]
        config: PathBuf,

        /// Optional path to write a JSON snapshot of the mission graph.
        /// If omitted, no snapshot is produced.
        #[arg(long = "snapshot-out", value_name = "PATH")]
        snapshot_out: Option<PathBuf>,

        /// Optional path to write an NDJSON event stream. If omitted,
        /// events are not exported. First line is `umwelt.events.v1`;
        /// subsequent lines are one JSON event each.
        #[arg(long = "events-out", value_name = "PATH")]
        events_out: Option<PathBuf>,

        /// Bind address for the live Prometheus metrics endpoint.
        /// Overrides `runtime.metrics_bind` in the config YAML.
        #[arg(long = "metrics-bind", value_name = "ADDR")]
        metrics_bind: Option<String>,
    },

    /// Read a mission snapshot and pretty-print it as JSON on stdout.
    Inspect {
        /// Path to a snapshot JSON file written by `umwelt run
        /// --snapshot-out`.
        #[arg(value_name = "SNAPSHOT")]
        snapshot: PathBuf,
    },

    /// Replay a recorded sensor log through the semiotic pipeline
    /// and emit a snapshot identical to what `umwelt run` would
    /// produce from the same inputs. Runs as fast as the disk reads;
    /// wall-clock pacing is dropped.
    Replay {
        /// Path to the engine config YAML file.
        #[arg(short, long)]
        config: PathBuf,

        /// Path to the sensor log (ndjson). First line must be the
        /// `umwelt.sensor_log.v1` header; subsequent lines are JSON
        /// `{tick_us, channel, q15}` samples.
        #[arg(short, long)]
        log: PathBuf,

        /// Optional path to write a JSON snapshot of the resulting
        /// graph. If omitted, no snapshot is produced.
        #[arg(long = "snapshot-out", value_name = "PATH")]
        snapshot_out: Option<PathBuf>,

        /// Optional path to write an NDJSON event stream produced
        /// during replay. Same `umwelt.events.v1` format as `umwelt run`.
        #[arg(long = "events-out", value_name = "PATH")]
        events_out: Option<PathBuf>,

        /// Bind address for the live Prometheus metrics endpoint.
        /// Overrides `runtime.metrics_bind` in the config YAML.
        #[arg(long = "metrics-bind", value_name = "ADDR")]
        metrics_bind: Option<String>,
    },

    /// Spawn N agents on a shared loopback mesh and run a timed
    /// mission. Emits one snapshot per agent. Agent count must be
    /// 2, 4, or 8; pipeline profile is drawn from the config.
    Simulate {
        /// Path to the engine config YAML file.
        #[arg(short, long)]
        config: PathBuf,

        /// Number of agents to spawn. Must be 2, 4, or 8.
        #[arg(short = 'n', long, default_value_t = 2)]
        agents: usize,

        /// Optional directory to write per-agent snapshot files as
        /// `<dir>/agent_<id>.json`. If omitted, snapshots are kept
        /// in memory and discarded on exit.
        #[arg(long = "snapshot-out-dir", value_name = "DIR")]
        snapshot_out_dir: Option<PathBuf>,

        /// Optional shared path to write a combined NDJSON event stream
        /// for all agents. Each line carries its producing `agent_id`.
        #[arg(long = "events-out", value_name = "PATH")]
        events_out: Option<PathBuf>,

        /// Bind address for the live Prometheus metrics endpoint.
        /// Overrides `runtime.metrics_bind` in the config YAML.
        #[arg(long = "metrics-bind", value_name = "ADDR")]
        metrics_bind: Option<String>,
    },
}

fn main() {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Run {
            config,
            snapshot_out,
            events_out,
            metrics_bind,
        } => run::execute(
            &config,
            snapshot_out.as_deref(),
            events_out.as_deref(),
            metrics_bind.as_deref(),
        )
        .map_err(CliError::from),
        Command::Inspect { snapshot } => inspect::execute(&snapshot)
            .map(|_| ())
            .map_err(CliError::from),
        Command::Replay {
            config,
            log,
            snapshot_out,
            events_out,
            metrics_bind,
        } => replay::execute(
            &config,
            &log,
            snapshot_out.as_deref(),
            events_out.as_deref(),
            metrics_bind.as_deref(),
        )
        .map(|_| ())
        .map_err(CliError::from),
        Command::Simulate {
            config,
            agents,
            snapshot_out_dir,
            events_out,
            metrics_bind,
        } => simulate::execute_owned(
            &config,
            agents,
            snapshot_out_dir,
            events_out,
            metrics_bind.as_deref(),
        )
        .map(|_| ())
        .map_err(CliError::from),
    };

    if let Err(e) = result {
        tracing::error!("{e}");
        std::process::exit(1);
    }
}

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error("run: {0}")]
    Run(#[from] run::RunError),
    #[error("inspect: {0}")]
    Inspect(#[from] inspect::InspectError),
    #[error("replay: {0}")]
    Replay(#[from] replay::ReplayError),
    #[error("simulate: {0}")]
    Simulate(#[from] simulate::SimulateError),
}

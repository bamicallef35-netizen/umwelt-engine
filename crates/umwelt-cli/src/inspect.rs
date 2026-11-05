//! `umwelt inspect` — read a snapshot JSON file, pretty-print it.
//!
//! The v1 inspector is minimal by design: load [`GraphSnapshot`],
//! re-emit the same JSON pretty-printed on stdout. Operators use
//! this as a sanity step before feeding a snapshot into downstream
//! tools (jq, observability export).
//!
//! Future variants (`--by-channel`, `--since <us>`) layer views on
//! top of the flat snapshot; they live here when added.
use std::path::Path;

use crate::snapshot::{GraphSnapshot, SnapshotError};

/// Errors raised by `umwelt inspect`.
#[derive(Debug, thiserror::Error)]
pub enum InspectError {
    /// Snapshot load failed.
    #[error("snapshot: {0}")]
    Snapshot(#[from] SnapshotError),
}

/// Load a snapshot from `path` and print it to stdout as
/// pretty-printed JSON. Returns the loaded snapshot so callers (and
/// tests) can assert on it.
pub fn execute(path: &Path) -> Result<GraphSnapshot, InspectError> {
    let snap = GraphSnapshot::read_from_file(path)?;
    let json = snap
        .to_json_pretty()
        .expect("already round-tripped via read");
    println!("{json}");
    Ok(snap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use umwelt_core::icon::IconEvent;
    use umwelt_core::replay::PipelineEvent;
    use umwelt_core::Q15;

    #[test]
    fn execute_reads_snapshot() {
        let tmp = std::env::temp_dir().join("umwelt-cli-inspect-test.json");
        let mut snap = GraphSnapshot::new(42, "swarm");
        snap.mission_start_us = 0;
        snap.mission_end_us = 1_000_000;
        snap.push_event(&PipelineEvent::Icon(IconEvent {
            channel: 1,
            formed_at: 100,
            entropy: Q15::ZERO,
            magnitude: Q15::from_raw(5_000),
        }));
        snap.write_to_file(&tmp).unwrap();

        let loaded = execute(&tmp).unwrap();
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(loaded, snap);
    }

    #[test]
    fn execute_rejects_missing_file() {
        let err = execute(Path::new("/no/such/snapshot.json")).unwrap_err();
        assert!(matches!(err, InspectError::Snapshot(SnapshotError::Io(_))));
    }
}

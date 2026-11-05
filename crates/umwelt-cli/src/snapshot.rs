//! Umwelt graph snapshot — operator-surface JSON format.
//!
//! A snapshot is a flat array of [`Sign`]s plus a metadata header.
//! Built at the end of an `umwelt run` mission, read by `umwelt
//! inspect`. Schema is versioned (`"schema": "umwelt.snapshot.v1"`)
//! so future changes don't silently corrupt older snapshots.
//!
//! Signs are synthesised from [`PipelineEvent`]s using a monotonic
//! [`SignId`]; the id is unique within this snapshot but does not
//! survive beyond it. Real on-device IDs come from the hash layer
//! (ADR-0014); snapshots are a view of the mission, not a replay
//! source.
use std::path::Path;

use serde::{Deserialize, Serialize};

use umwelt_core::replay::PipelineEvent;
use umwelt_core::{SemioticLayer, Sign, SignDomain, SignId, TimestampUs, Q15};

/// Schema tag written into every snapshot. Mismatch on read is an
/// error — forward-compatible readers must bump this.
pub const SCHEMA_TAG: &str = "umwelt.snapshot.v1";

/// One Umwelt graph snapshot.
///
/// Flat on purpose: a v1 snapshot is a header plus a list of signs.
/// No nesting, no cross-references — a future version can add edges
/// or provenance without breaking this one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphSnapshot {
    /// Schema version tag. Always [`SCHEMA_TAG`] for v1.
    pub schema: String,
    /// Agent that produced this snapshot.
    pub agent_id: u32,
    /// Mission start timestamp (µs since agent epoch).
    pub mission_start_us: TimestampUs,
    /// Mission end timestamp (µs since agent epoch).
    pub mission_end_us: TimestampUs,
    /// Name of the vocabulary used. Empty string if unavailable.
    pub vocabulary: String,
    /// Flat sign list, in emission order.
    pub signs: Vec<Sign>,
}

impl GraphSnapshot {
    /// Construct an empty snapshot with the schema tag set.
    pub fn new(agent_id: u32, vocabulary: impl Into<String>) -> Self {
        Self {
            schema: SCHEMA_TAG.to_string(),
            agent_id,
            mission_start_us: 0,
            mission_end_us: 0,
            vocabulary: vocabulary.into(),
            signs: Vec::new(),
        }
    }

    /// Serialize to pretty-printed JSON.
    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Write the snapshot to `path` as pretty-printed JSON.
    pub fn write_to_file(&self, path: &Path) -> Result<(), SnapshotError> {
        let json = self.to_json_pretty()?;
        std::fs::write(path, json).map_err(Into::into)
    }

    /// Read a snapshot from `path`. Fails if the schema tag is wrong.
    pub fn read_from_file(path: &Path) -> Result<Self, SnapshotError> {
        let contents = std::fs::read_to_string(path)?;
        let snap: Self = serde_json::from_str(&contents)?;
        if snap.schema != SCHEMA_TAG {
            return Err(SnapshotError::SchemaMismatch {
                found: snap.schema,
                expected: SCHEMA_TAG,
            });
        }
        Ok(snap)
    }

    /// Append a pipeline event as a [`Sign`]. The id is a monotonic
    /// counter over the snapshot's current length; intensity and
    /// coherence are taken from the event where available.
    pub fn push_event(&mut self, ev: &PipelineEvent) {
        let id = SignId::new(self.signs.len() as u64 + 1);
        let sign = event_to_sign(id, ev);
        self.signs.push(sign);
    }
}

/// Reasons snapshot I/O can fail.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// Filesystem I/O error.
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    /// JSON parse or serialise error.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// Schema tag didn't match [`SCHEMA_TAG`].
    #[error("schema mismatch: found {found:?}, expected {expected:?}")]
    SchemaMismatch {
        /// The schema tag read from disk.
        found: String,
        /// The schema tag this build expects.
        expected: &'static str,
    },
}

fn event_to_sign(id: SignId, ev: &PipelineEvent) -> Sign {
    match ev {
        PipelineEvent::Icon(e) => Sign {
            id,
            layer: SemioticLayer::Icon,
            domain: SignDomain::Spatial,
            formed_at: e.formed_at,
            duration_us: 0,
            channel: e.channel,
            intensity: e.magnitude,
            coherence: Q15::ZERO,
        },
        PipelineEvent::Index(e) => Sign {
            id,
            layer: SemioticLayer::Index,
            domain: SignDomain::Temporal,
            formed_at: e.formed_at,
            duration_us: 0,
            channel: e.channels[0],
            intensity: Q15::ZERO,
            coherence: e.coherence,
        },
        PipelineEvent::Symbol(e) => Sign {
            id,
            layer: SemioticLayer::Symbol,
            domain: SignDomain::Coherence,
            formed_at: e.formed_at,
            duration_us: 0,
            channel: e.channels[0],
            intensity: Q15::ZERO,
            coherence: e.coherence,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use umwelt_core::icon::IconEvent;

    fn sample_icon() -> PipelineEvent {
        PipelineEvent::Icon(IconEvent {
            channel: 7,
            formed_at: 1_234_567,
            entropy: Q15::from_raw(20_000),
            magnitude: Q15::from_raw(15_000),
        })
    }

    #[test]
    fn new_snapshot_has_schema_tag() {
        let s = GraphSnapshot::new(42, "swarm");
        assert_eq!(s.schema, SCHEMA_TAG);
        assert_eq!(s.agent_id, 42);
        assert_eq!(s.vocabulary, "swarm");
        assert!(s.signs.is_empty());
    }

    #[test]
    fn push_event_assigns_monotonic_ids() {
        let mut s = GraphSnapshot::new(1, "");
        s.push_event(&sample_icon());
        s.push_event(&sample_icon());
        assert_eq!(s.signs[0].id, SignId::new(1));
        assert_eq!(s.signs[1].id, SignId::new(2));
    }

    #[test]
    fn icon_event_maps_to_icon_layer() {
        let mut s = GraphSnapshot::new(1, "");
        s.push_event(&sample_icon());
        assert_eq!(s.signs[0].layer, SemioticLayer::Icon);
        assert_eq!(s.signs[0].channel, 7);
        assert_eq!(s.signs[0].formed_at, 1_234_567);
    }

    #[test]
    fn json_round_trip_preserves_all_fields() {
        let mut s = GraphSnapshot::new(99, "terrain");
        s.mission_start_us = 1000;
        s.mission_end_us = 2000;
        s.push_event(&sample_icon());
        let json = s.to_json_pretty().unwrap();
        let back: GraphSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn wrong_schema_tag_is_rejected() {
        let tmp = std::env::temp_dir().join("umwelt-cli-bad-schema.json");
        std::fs::write(&tmp, r#"{"schema":"umwelt.snapshot.v99","agent_id":0,"mission_start_us":0,"mission_end_us":0,"vocabulary":"","signs":[]}"#).unwrap();
        let err = GraphSnapshot::read_from_file(&tmp).unwrap_err();
        let _ = std::fs::remove_file(&tmp);
        assert!(matches!(err, SnapshotError::SchemaMismatch { .. }));
    }

    #[test]
    fn write_read_file_round_trip() {
        let tmp = std::env::temp_dir().join("umwelt-cli-snapshot-test.json");
        let mut s = GraphSnapshot::new(5, "swarm");
        s.push_event(&sample_icon());
        s.write_to_file(&tmp).unwrap();
        let back = GraphSnapshot::read_from_file(&tmp).unwrap();
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(s, back);
    }
}

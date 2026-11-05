//! `umwelt events` — NDJSON sign-flow export.
//!
//! Every [`PipelineEvent`] emitted during a mission can be mirrored to
//! a caller-chosen sink as a single JSON object per line. The format is
//! deliberately flat and versioned so downstream tools (jq, pandas,
//! Python notebooks) can tail it without a schema registry:
//!
//! ```text
//! umwelt.events.v1
//! {"tick_us":0,"kind":"icon","agent_id":1,"channel":0,"intensity":15000,"coherence":0}
//! {"tick_us":100,"kind":"index","agent_id":1,"channel":0,"intensity":0,"coherence":22000}
//! ...
//! ```
//!
//! `intensity` and `coherence` are raw `i16` Q15 words. Icon events
//! carry magnitude in `intensity` and zero `coherence`; Index and
//! Symbol events carry zero `intensity` and the pairwise coherence.
//! The `channel` column is `channels[0]` for pair events, so a single
//! jq filter (`select(.channel==3)`) works across all three kinds.
//!
//! Host-only by design — this is the firehose counterpart to the
//! [`GraphSnapshot`](crate::snapshot::GraphSnapshot) "final state"
//! JSON. Core crates stay clean of serialisation.
//!
//! The [`EventSink`] trait is behind the wiring so tests can assert
//! line-by-line output by pointing at a `Vec<u8>`; production code
//! points at a [`std::io::BufWriter`] around a file.

use std::io::{self, Write};

use serde::{Deserialize, Serialize};

use umwelt_core::replay::PipelineEvent;

/// Header tag that every events NDJSON file must carry on its first
/// line. Mismatch on read is an error — bump on any schema change.
pub const EVENTS_HEADER: &str = "umwelt.events.v1";

// ── Record ────────────────────────────────────────────────────────────────

/// One NDJSON line. Flat on purpose: downstream filters should not
/// need to branch on shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRecord {
    /// Microseconds since agent epoch (the event's `formed_at`).
    pub tick_us: u64,
    /// Layer kind: `"icon"`, `"index"`, or `"symbol"`.
    pub kind: EventKind,
    /// Producing agent. In a simulate run this disambiguates the
    /// shared sink across peers; in `run`/`replay` it's constant.
    pub agent_id: u32,
    /// Primary channel. For pair events this is `channels[0]`.
    pub channel: u16,
    /// Raw Q15 intensity. Icon carries magnitude; Index/Symbol zero.
    pub intensity: i16,
    /// Raw Q15 coherence. Icon zero; Index/Symbol carry the pair
    /// coherence.
    pub coherence: i16,
}

/// Layer of the producing event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    /// Icon-layer event.
    Icon,
    /// Index-layer event.
    Index,
    /// Symbol-layer event.
    Symbol,
}

impl EventRecord {
    /// Build a record for an event produced by `agent_id`.
    pub fn from_pipeline(agent_id: u32, ev: &PipelineEvent) -> Self {
        match ev {
            PipelineEvent::Icon(e) => Self {
                tick_us: e.formed_at,
                kind: EventKind::Icon,
                agent_id,
                channel: e.channel,
                intensity: e.magnitude.raw(),
                coherence: 0,
            },
            PipelineEvent::Index(e) => Self {
                tick_us: e.formed_at,
                kind: EventKind::Index,
                agent_id,
                channel: e.channels[0],
                intensity: 0,
                coherence: e.coherence.raw(),
            },
            PipelineEvent::Symbol(e) => Self {
                tick_us: e.formed_at,
                kind: EventKind::Symbol,
                agent_id,
                channel: e.channels[0],
                intensity: 0,
                coherence: e.coherence.raw(),
            },
        }
    }
}

// ── Sink trait ───────────────────────────────────────────────────────────

/// A target the mission loop can write events to. Implementations
/// decide whether to serialise, buffer, or discard.
///
/// Errors are [`std::io::Error`] so a sink can wrap any [`Write`]
/// without inventing a new error family.
pub trait EventSink {
    /// Consume one record. Implementations that serialise should emit
    /// exactly one newline-terminated JSON object.
    fn write_event(&mut self, rec: &EventRecord) -> io::Result<()>;

    /// Flush any buffered output. Default is no-op; file-backed sinks
    /// should override.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A sink that throws every event away. The default when
/// `--events-out` is not provided; keeps the mission loop monomorphic
/// and avoids an Option-branch in the hot path.
pub struct NullSink;

impl EventSink for NullSink {
    fn write_event(&mut self, _rec: &EventRecord) -> io::Result<()> {
        Ok(())
    }
}

/// A sink that serialises to an underlying [`Write`]. Writes the
/// [`EVENTS_HEADER`] on construction so the stream is self-describing.
pub struct WriterSink<W: Write> {
    inner: W,
}

impl<W: Write> WriterSink<W> {
    /// Wrap `inner` and emit the header line.
    pub fn new(mut inner: W) -> io::Result<Self> {
        writeln!(inner, "{EVENTS_HEADER}")?;
        Ok(Self { inner })
    }

    /// Release the underlying writer.
    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> EventSink for WriterSink<W> {
    fn write_event(&mut self, rec: &EventRecord) -> io::Result<()> {
        serde_json::to_writer(&mut self.inner, rec)?;
        self.inner.write_all(b"\n")
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use umwelt_core::icon::IconEvent;
    use umwelt_core::index::IndexEvent;
    use umwelt_core::symbol::SymbolEvent;
    use umwelt_core::Q15;

    fn icon_event() -> PipelineEvent {
        PipelineEvent::Icon(IconEvent {
            channel: 3,
            formed_at: 1_000,
            entropy: Q15::from_raw(12_345),
            magnitude: Q15::from_raw(15_000),
        })
    }

    fn index_event() -> PipelineEvent {
        PipelineEvent::Index(IndexEvent {
            channels: [2, 5],
            delta_us: 200,
            formed_at: 2_000,
            coherence: Q15::from_raw(22_000),
        })
    }

    fn symbol_event() -> PipelineEvent {
        PipelineEvent::Symbol(SymbolEvent {
            channels: [2, 5],
            formed_at: 3_000,
            coherence: Q15::from_raw(30_000),
        })
    }

    #[test]
    fn icon_record_carries_magnitude_and_zero_coherence() {
        let r = EventRecord::from_pipeline(7, &icon_event());
        assert_eq!(r.tick_us, 1_000);
        assert_eq!(r.kind, EventKind::Icon);
        assert_eq!(r.agent_id, 7);
        assert_eq!(r.channel, 3);
        assert_eq!(r.intensity, 15_000);
        assert_eq!(r.coherence, 0);
    }

    #[test]
    fn index_record_uses_first_channel_and_coherence() {
        let r = EventRecord::from_pipeline(7, &index_event());
        assert_eq!(r.kind, EventKind::Index);
        assert_eq!(r.channel, 2);
        assert_eq!(r.intensity, 0);
        assert_eq!(r.coherence, 22_000);
    }

    #[test]
    fn symbol_record_uses_first_channel_and_coherence() {
        let r = EventRecord::from_pipeline(7, &symbol_event());
        assert_eq!(r.kind, EventKind::Symbol);
        assert_eq!(r.channel, 2);
        assert_eq!(r.coherence, 30_000);
    }

    #[test]
    fn writer_sink_writes_header_on_construction() {
        let sink = WriterSink::new(Vec::new()).unwrap();
        let bytes = sink.into_inner();
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text.trim(), EVENTS_HEADER);
    }

    #[test]
    fn writer_sink_emits_one_line_per_event() {
        let mut sink = WriterSink::new(Vec::new()).unwrap();
        sink.write_event(&EventRecord::from_pipeline(1, &icon_event()))
            .unwrap();
        sink.write_event(&EventRecord::from_pipeline(1, &index_event()))
            .unwrap();
        sink.flush().unwrap();
        let bytes = sink.into_inner();
        let text = String::from_utf8(bytes).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "header + 2 events; got: {lines:?}");
        assert_eq!(lines[0], EVENTS_HEADER);
        // Each body line parses back as an EventRecord.
        let a: EventRecord = serde_json::from_str(lines[1]).unwrap();
        let b: EventRecord = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(a.kind, EventKind::Icon);
        assert_eq!(b.kind, EventKind::Index);
    }

    #[test]
    fn null_sink_drops_everything() {
        let mut sink = NullSink;
        // Must succeed with no side effects, for any event.
        sink.write_event(&EventRecord::from_pipeline(0, &icon_event()))
            .unwrap();
        sink.write_event(&EventRecord::from_pipeline(0, &index_event()))
            .unwrap();
        sink.flush().unwrap();
    }

    #[test]
    fn event_kind_serde_is_lowercase() {
        let j = serde_json::to_string(&EventKind::Icon).unwrap();
        assert_eq!(j, "\"icon\"");
        let j = serde_json::to_string(&EventKind::Symbol).unwrap();
        assert_eq!(j, "\"symbol\"");
    }

    #[test]
    fn event_record_json_is_flat() {
        // Flatness is part of the contract; break a key and future
        // consumers break with you.
        let rec = EventRecord::from_pipeline(42, &icon_event());
        let j = serde_json::to_string(&rec).unwrap();
        for key in [
            "tick_us",
            "kind",
            "agent_id",
            "channel",
            "intensity",
            "coherence",
        ] {
            assert!(
                j.contains(&format!("\"{key}\"")),
                "expected key {key} in {j}"
            );
        }
    }
}

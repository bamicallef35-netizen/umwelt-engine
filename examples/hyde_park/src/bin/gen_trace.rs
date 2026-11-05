//! Generate `terrain_trace.ndjson` — the checked-in sensor trace the
//! `hyde-park-replay` example reads.
//!
//! The main binary (`hyde-park-replay`) reads this file as-is and
//! never writes it. Regenerate only when the trace needs to change
//! (e.g. a new signal shape, different cadence). The file path is
//! stable: `examples/hyde_park/terrain_trace.ndjson` relative to the
//! repository root.
//!
//! ## Narrative
//!
//! A 20-second walk through Hyde Park, 10 Hz cadence, four channels
//! drawn from the ECI terrain vocabulary (`vocabularies/terrain.yaml`):
//!
//! - **channel 0** — moisture
//! - **channel 1** — temperature
//! - **channel 2** — vibration (footsteps / gravel)
//! - **channel 3** — light
//!
//! The 200 ticks decompose into four 5-second segments:
//!
//! - `0– 5 s` — calm entry, nominal values.
//! - `5–10 s` — footsteps on gravel: vibration spikes, light oscillates
//!   under tree canopy.
//! - `10–15 s` — crossing a puddle: moisture rises, vibration continues,
//!   light still dappled.
//! - `15–20 s` — out of the trees: light climbs, vibration fades,
//!   temperature bumps from sun exposure.
//!
//! All formulas are deterministic and integer-safe. No RNG. Running
//! the generator twice is a no-op diff.
//!
//! ## Output shape
//!
//! NDJSON, `umwelt.sensor_log.v1` header line, then 800 records of
//! `{tick_us, channel, q15}`. Channels are emitted round-robin per
//! tick with 1 ms inter-channel offsets so the index layer's 500 ms
//! co-occurrence window catches every `(channel_a, channel_b)` pair
//! inside a single tick.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use umwelt_core::Q15;

/// Tick period in microseconds (10 Hz → 100 ms per tick).
const TICK_US: u64 = 100_000;

/// Number of ticks in the trace.
const TICKS: u64 = 200;

/// Number of channels.
const CHANNELS: u16 = 4;

/// Inter-channel offset inside a tick (µs).
const CHANNEL_OFFSET_US: u64 = 1_000;

fn main() -> std::io::Result<()> {
    // Resolve the output path relative to the crate manifest so
    // `cargo run --bin gen-trace` from any cwd works.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_path = manifest_dir.join("terrain_trace.ndjson");

    let file = File::create(&out_path)?;
    let mut w = BufWriter::new(file);

    writeln!(w, "umwelt.sensor_log.v1")?;

    for tick in 0..TICKS {
        let base_us = (tick + 1) * TICK_US;
        for channel in 0..CHANNELS {
            let t_us = base_us + u64::from(channel) * CHANNEL_OFFSET_US;
            let q15 = sample(channel, tick).raw();
            writeln!(w, r#"{{"tick_us":{t_us},"channel":{channel},"q15":{q15}}}"#)?;
        }
    }

    w.flush()?;
    println!(
        "wrote {} ({} ticks × {} channels = {} records)",
        out_path.display(),
        TICKS,
        CHANNELS,
        TICKS * u64::from(CHANNELS),
    );
    Ok(())
}

/// Evaluate the trace signal at `(channel, tick)`.
///
/// Integer-only, deterministic. Each channel has a base level and a
/// per-segment modulation that drives the pipeline's entropy gate.
/// The segments mirror the narrative in the module header.
fn sample(channel: u16, tick: u64) -> Q15 {
    // Segments: 0-49 (calm), 50-99 (gravel), 100-149 (puddle),
    // 150-199 (clearing). One segment = 50 ticks = 5 s.
    let segment = tick / 50;
    let phase = tick % 50;

    // Per-channel amplitude in Q15 raw units.
    // ~32767 = 1.0. Use moderate amplitudes so the signal straddles
    // zero (keeps the 8-bin histogram populated across bins).
    let amp = match channel {
        0 => 15_000, // moisture
        1 => 12_000, // temperature
        2 => 25_000, // vibration — hottest channel
        3 => 20_000, // light
        _ => 0,
    };

    // Alternating sign driven by `phase`. A square-ish waveform keeps
    // entropy saturated in the 8-bin histogram without needing sin/cos.
    let sign: i32 = if phase % 2 == 0 { 1 } else { -1 };

    let raw = match (channel, segment) {
        // ── channel 0 — moisture ──────────────────────────────────
        (0, 0) => sign * 3_000,                      // nominal drizzle
        (0, 1) => sign * 4_000,                      // still nominal
        (0, 2) => sign * amp + (phase as i32 * 100), // puddle: rising alt
        (0, 3) => sign * 5_000,                      // drying out
        // ── channel 1 — temperature ───────────────────────────────
        (1, 0) => sign * 2_000, // cool
        (1, 1) => sign * 3_000,
        (1, 2) => sign * 4_000,
        (1, 3) => sign * amp, // sun bump
        // ── channel 2 — vibration (footsteps) ─────────────────────
        (2, 0) => sign * 2_000,                        // idle
        (2, 1) => sign * amp,                          // gravel spikes
        (2, 2) => sign * (amp - (phase as i32 * 200)), // puddle: fading
        (2, 3) => sign * 3_000,                        // grass: quiet
        // ── channel 3 — light ─────────────────────────────────────
        (3, 0) => sign * 5_000,                     // overcast
        (3, 1) => sign * amp,                       // canopy dapple
        (3, 2) => sign * (amp - 4_000),             // under the puddle shade
        (3, 3) => sign * amp + (phase as i32 * 50), // climbing out
        // Unreachable for the 4-channel trace; kept explicit so the
        // match isn't forced into a wildcard that could mask bugs.
        _ => 0,
    };

    Q15::from_raw(raw.clamp(-32_767, 32_767) as i16)
}

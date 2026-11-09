//! `hyde-park-replay` — the Python-free Umwelt Engine walkthrough.
//!
//! This is the P9.5 closer on the v0.1 plan. It takes a checked-in
//! terrain sensor trace (`terrain_trace.ndjson`), drives it through
//! the semiotic pipeline at the small profile (`<4, 64, 8>`), and
//! prints a compact summary of the signs that got committed to the
//! agent's Umwelt graph.
//!
//! ```text
//! cargo run -p hyde-park-replay
//! ```
//!
//! Everything runs in the `umwelt-core` surface — no `umwelt-cli`,
//! no YAML parser, no PyO3. A reviewer with a Rust toolchain can
//! trace the full semiogenesis path end-to-end from this one file.
//!
//! The sibling binary `gen-trace` regenerates the NDJSON from a
//! deterministic formula:
//!
//! ```text
//! cargo run -p hyde-park-replay --bin gen-trace
//! ```
//!
//! See [`src/bin/gen_trace.rs`] for the signal narrative.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use serde::Deserialize;

use umwelt_core::replay::{PipelineEvent, SemioticPipeline};
use umwelt_core::{SemioticLayer, Q15};
use umwelt_vocab::SignVocabulary;

/// Header line every `umwelt.sensor_log.v1` trace must carry.
const LOG_HEADER: &str = "umwelt.sensor_log.v1";

/// Small profile — matches the pinned `<C, W, B>` from ADR-0020.
const CHANNELS: usize = 4;
const ICON_WINDOW: usize = 64;
const ICON_BINS: usize = 8;

#[derive(Debug, Clone, Copy, Deserialize)]
struct Sample {
    tick_us: u64,
    channel: u16,
    q15: i16,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let trace_path = manifest_dir.join("terrain_trace.ndjson");
    let vocab_path = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .ok_or("cannot resolve workspace root")?
        .join("vocabularies/terrain.yaml");

    let samples = load_trace(&trace_path)?;
    let vocab = SignVocabulary::from_file(&vocab_path)?;

    println!("Hyde Park replay — Umwelt Engine v0.1");
    println!("-------------------------------------");
    println!("trace     : {}", trace_path.display());
    println!("vocabulary: {} ({} types)", vocab.name, vocab.types.len());
    println!("profile   : <C={CHANNELS}, W={ICON_WINDOW}, B={ICON_BINS}> (small, ADR-0020)");
    println!("samples   : {}", samples.len());
    println!();

    let summary = replay(&samples);

    println!(
        "Mission span  : tick {} → {} us",
        summary.first_us, summary.last_us
    );
    println!(
        "Total events  : {}",
        summary.icons + summary.indices + summary.symbols
    );
    println!("  icons       : {}", summary.icons);
    println!("  indices     : {}", summary.indices);
    println!("  symbols     : {}", summary.symbols);

    if !summary.icons_per_channel.is_empty() {
        println!();
        println!("Icons per channel:");
        for (ch, count) in &summary.icons_per_channel {
            println!("  channel {ch}: {count}");
        }
    }

    if !summary.index_pairs.is_empty() {
        println!();
        println!("Index co-occurrence pairs:");
        for ((a, b), count) in &summary.index_pairs {
            println!("  ({a}, {b}): {count}");
        }
    }

    if !summary.first_signs.is_empty() {
        println!();
        println!(
            "First {} emissions (layer | channel | formed_at):",
            summary.first_signs.len()
        );
        for (layer, channel, formed_at) in &summary.first_signs {
            println!("  {layer:<6} ch{channel:<2} {formed_at} us");
        }
    }

    println!();
    println!("Done. All processing host-native Rust; no Python, no network.");
    Ok(())
}

#[derive(Default)]
struct Summary {
    first_us: u64,
    last_us: u64,
    icons: u64,
    indices: u64,
    symbols: u64,
    icons_per_channel: BTreeMap<u16, u64>,
    index_pairs: BTreeMap<(u16, u16), u64>,
    /// Up to the first 5 emissions, in causal order. Printed to show
    /// the pipeline is visibly alive without spamming for the full
    /// run.
    first_signs: Vec<(&'static str, u16, u64)>,
}

fn replay(samples: &[Sample]) -> Summary {
    let mut pipeline = SemioticPipeline::<CHANNELS, ICON_WINDOW, ICON_BINS>::new();
    let mut s = Summary {
        first_us: samples.first().map(|x| x.tick_us).unwrap_or(0),
        last_us: samples.last().map(|x| x.tick_us).unwrap_or(0),
        ..Summary::default()
    };

    for sample in samples {
        let _ = pipeline.observe(
            sample.channel,
            Q15::from_raw(sample.q15),
            sample.tick_us,
            |evt| {
                record(&mut s, evt);
            },
        );
    }

    s
}

fn record(s: &mut Summary, evt: PipelineEvent) {
    let (layer, channel, formed_at) = match evt {
        PipelineEvent::Icon(e) => {
            s.icons += 1;
            *s.icons_per_channel.entry(e.channel).or_insert(0) += 1;
            ("icon", e.channel, e.formed_at)
        }
        PipelineEvent::Index(e) => {
            s.indices += 1;
            let pair = order_pair(e.channels[0], e.channels[1]);
            *s.index_pairs.entry(pair).or_insert(0) += 1;
            (layer_name(SemioticLayer::Index), e.channels[0], e.formed_at)
        }
        PipelineEvent::Symbol(e) => {
            s.symbols += 1;
            (
                layer_name(SemioticLayer::Symbol),
                e.channels[0],
                e.formed_at,
            )
        }
    };

    if s.first_signs.len() < 5 {
        s.first_signs.push((layer, channel, formed_at));
    }
}

fn order_pair(a: u16, b: u16) -> (u16, u16) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

fn layer_name(layer: SemioticLayer) -> &'static str {
    match layer {
        SemioticLayer::Icon => "icon",
        SemioticLayer::Index => "index",
        SemioticLayer::Symbol => "symbol",
    }
}

fn load_trace(path: &PathBuf) -> Result<Vec<Sample>, Box<dyn std::error::Error>> {
    let file = File::open(path)?;
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
                return Err(format!(
                    "log header mismatch on line {}: expected {LOG_HEADER:?}, got {trimmed:?}",
                    idx + 1
                )
                .into());
            }
            header_seen = true;
            continue;
        }
        let sample: Sample = serde_json::from_str(trimmed)?;
        samples.push(sample);
    }
    if !header_seen {
        return Err(format!(
            "log header missing; file empty or malformed: {}",
            path.display()
        )
        .into());
    }
    Ok(samples)
}

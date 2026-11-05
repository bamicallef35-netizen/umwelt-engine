# Hyde Park replay

A Python-free Umwelt Engine walkthrough. Drives a 20-second terrain
sensor trace through the full semiogenesis pipeline (Icon → Index →
Symbol) and prints the signs that got committed.

This is the P9.5 closer on the v0.1 plan — the engineer-facing
counterpart to the PyO3 notebook at
[`../notebooks/hyde_park_replay.ipynb`](../notebooks/hyde_park_replay.ipynb).
If you have a Rust toolchain you need nothing else; no maturin, no
Jupyter, no Python.

## Run

```bash
cargo run -p hyde-park-replay
```

Reads [`terrain_trace.ndjson`](./terrain_trace.ndjson) from this
directory, loads [`vocabularies/terrain.yaml`](../../vocabularies/terrain.yaml),
builds a small-profile `SemioticPipeline<4, 64, 8>` (ADR-0020),
observes every sample in causal order, and prints a summary:

```text
Hyde Park replay — Umwelt Engine v0.1
-------------------------------------
trace     : …/examples/hyde_park/terrain_trace.ndjson
vocabulary: terrain (30 types)
profile   : <C=4, W=64, B=8> (small, ADR-0020)
samples   : 800

Mission span  : tick 100000 → 20003000 us
Total events  : 48
  icons       : 18
  indices     : 23
  symbols     : 7

Icons per channel:
  channel 0: 6
  channel 2: 6
  channel 3: 6

Index co-occurrence pairs:
  (0, 2): 6
  (0, 3): 6
  (2, 3): 11

First 5 emissions (layer | channel | formed_at):
  icon   ch0  6400000 us
  icon   ch2  6402000 us
  index  ch0  6402000 us
  icon   ch3  6403000 us
  index  ch0  6403000 us

Done. All processing host-native Rust; no Python, no network.
```

## Regenerate the trace

The NDJSON trace is checked in so `cargo run -p hyde-park-replay`
works on a fresh clone. When you want to change the signal shape,
edit [`src/bin/gen_trace.rs`](./src/bin/gen_trace.rs) and run:

```bash
cargo run -p hyde-park-replay --bin gen-trace
```

The generator is deterministic — no RNG. Running it twice is a no-op
diff. The output path is wired to `CARGO_MANIFEST_DIR` so `cwd`
doesn't matter.

## Trace shape

Four channels from [`vocabularies/terrain.yaml`](../../vocabularies/terrain.yaml)
observed at 10 Hz for 20 s (200 ticks × 4 channels = 800 samples):

| channel | mapping     | notes                                                |
|---------|-------------|------------------------------------------------------|
| 0       | moisture    | rises during the "crossing a puddle" segment         |
| 1       | temperature | low-amplitude throughout — never fires an icon       |
| 2       | vibration   | footsteps: spikes during gravel, fades on grass      |
| 3       | light       | dapples under canopy; climbs in the clearing         |

Channel 1's quiet behaviour is on purpose — the pipeline's activity
gate (ADR-0013, `magnitude_gate ≈ 0.1`) correctly refuses to emit an
icon when the ambient signal stays small. That's the feature.

## Why this example exists

The P7.3 PyO3 notebook shows the same pipeline from Python. This
binary is the Rust-native counterpart:

- Builds against `umwelt-core` and `umwelt-vocab` directly — no
  `umwelt-cli`, no YAML config loader, no async runtime. The
  shortest possible path from sample → Icon/Index/Symbol.
- No unsafe, no Python, no network, no tokio. A reviewer on an
  ordinary laptop gets a green run in <3 s on a cold cache.
- Deterministic trace means output is stable; CI and humans see the
  same numbers.

If you want to go further:

- Point `umwelt replay` at this NDJSON —
  `cargo run -p umwelt-cli -- replay --log examples/hyde_park/terrain_trace.ndjson --config examples/configs/loopback_agent.yaml`
  drives the same trace through the full CLI surface (snapshot +
  events + metrics).
- Copy [`src/bin/gen_trace.rs`](./src/bin/gen_trace.rs) as a
  scaffolding to build your own domain's trace against the
  `umwelt.sensor_log.v1` schema.

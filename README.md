# Umwelt Engine

[![verify](https://github.com/benmicallef/umwelt-engine/actions/workflows/ci.yml/badge.svg)](https://github.com/benmicallef/umwelt-engine/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**An on-device semiotic runtime for physical agents.**

**[▶ Live](https://benmicallef.github.io/umwelt-engine/)** — the `no_std` core compiled to WebAssembly, unchanged, condensing a sensor trace, or your pointer, into icons, indices and symbols in the browser.

Umwelt Engine turns raw sensor streams into structured *signs* — Peirce's
triadic chain of **Icon → Index → Symbol** — and exchanges them with
peers over a partition-tolerant mesh. Written in Rust, `no_std` at the
core, zero `unsafe`, measured at 102 KiB of `.text + .data` on ARM
Cortex-M4F (`thumbv7em-none-eabihf`) — 39 % of its 256 KB budget.

The goal is not another inference stack. VLA models (π₀, GR00T N1,
OpenVLA) tell a robot *what to do next*. Umwelt Engine gives a robot
*what the world means to it* — a compact, legible, shared representation
every agent in a swarm can read and reason over without a datacentre
round-trip.

The name comes from Jakob von Uexküll, for whom every organism lives in
its own *Umwelt*: the world as its sense organs and effectors cut it
out. The sign model is C. S. Peirce's. The engineering question is
cybernetic in the old sense: how a small machine with a few sensors
builds a stable, shareable model of its surroundings under noise, packet
loss and partition. Umwelt Engine is the open core of the autonomy stack
I'm building at Sentinus, for robots that have to keep working without
GPS or a network link.

## At a glance

| | |
|---|---|
| Core language | Rust 1.95.0 (pinned) |
| Core discipline | `no_std`, zero `unsafe`, fixed-point `Q15(i16)` |
| Target | ARM Cortex-M4F (`thumbv7em-none-eabihf`) cross-compile gate green |
| Core footprint | ~102 KiB `.text + .data` (libm-dominated; `umwelt-core` alone is 398 B) |
| Wire protocol | 24-byte inner SEP frame / 32-byte authenticated envelope (ADR-0011/0011a) |
| Authentication | BLAKE3-keyed MAC-64, pre-shared key (ADR-0012) |
| Python | PyO3 0.22 `abi3-py38` bindings — `pip install maturin && maturin develop` |
| Metrics | Live `/metrics` endpoint, Prometheus text format |
| Browser | the core as a 22 KB WebAssembly module, no bindings generator — [live page](https://benmicallef.github.io/umwelt-engine/), [`web/`](web/) |
| Status | v0.1.0 released; on-target hardware validation pending |

## 30-second quickstart

```bash
# One-time: pin the toolchain (rust-toolchain.toml does this on first cargo run)
rustup show

# Gate every crate (fmt, check, no_std gates, Cortex-M cross-compile, test, clippy)
bash scripts/verify.sh

# Boot a single-agent mission over the in-process loopback transport
cargo run -p umwelt-cli -- run --config examples/configs/loopback_agent.yaml
```

`verify.sh` is the one gate every commit passes: fmt, check, both
`no_std` gates, the Cortex-M cross-compile, 300+ tests and clippy with
`-D warnings`. If it exits green, every other command below is safe to run.

## 5-minute tour

The CLI has four verbs. Each is a thin shell over the same
`SemioticPipeline<C,W,B>` monomorphised at one of two pinned profiles
(Small: `<4,64,8>`; Medium: `<8,128,16>` — see ADR-0020).

### `umwelt run` — boot an agent

```bash
cargo run -p umwelt-cli -- run \
    --config examples/configs/loopback_agent.yaml \
    --snapshot-out /tmp/snapshot.json \
    --events-out    /tmp/events.ndjson \
    --metrics-bind  127.0.0.1:9099
```

Loads the YAML config, builds a `SepTransport` (loopback / UDP /
sim-LoRa-over-loopback), runs the `tick_hz × duration_s` mission with
real-time pacing, writes a JSON snapshot of the final Umwelt graph on
exit, and emits one NDJSON line per sign event. While the mission runs,
`curl -s 127.0.0.1:9099/metrics` returns Prometheus counters with
labels `kind=run`, `agent_id`, `profile`, and `layer`.

### `umwelt simulate` — N-agent swarm

```bash
cargo run -p umwelt-cli -- simulate \
    --agents 6 \
    --config examples/configs/loopback_agent.yaml \
    --snapshot-out-dir /tmp/swarm/
```

Spawns an N-agent mesh (N ∈ {2, 4, 8}) over `LoopbackMesh`. Each agent
owns its own `SemioticPipeline` and `GraphSnapshot`; one shared mesh
fabric delivers SEP frames deterministically from a per-agent seed.
Drops per-agent JSON snapshots under the output directory.

### `umwelt replay` — offline log replay

```bash
cargo run -p umwelt-cli -- replay \
    --log   path/to/sensor.ndjson \
    --config examples/configs/loopback_agent.yaml
```

Ingests an NDJSON sensor log (`{tick_us, channel, q15}` per line, with
a `umwelt.sensor_log.v1` header) and drives the pipeline without
wall-clock pacing. Same snapshot / events / metrics flags as `run`.

### `umwelt inspect` — dump a snapshot

```bash
cargo run -p umwelt-cli -- inspect /tmp/snapshot.json
```

Verifies the `umwelt.snapshot.v1` schema tag and pretty-prints the
graph's signs to stdout.

## Architecture

```
┌──────────────────────────── no_std boundary ────────────────────────────┐
│                                                                         │
│   sensors ──► umwelt-core::SemioticPipeline<C,W,B>                      │
│                   │                                                     │
│                   ├─ Icon layer     (threshold + Shannon entropy gate)  │
│                   │      single-channel percepts                        │
│                   │                                                     │
│                   ├─ Index layer   (pairwise temporal correlator)       │
│                   │      cross-channel co-occurrences, geomean coherence│
│                   │                                                     │
│                   └─ Symbol layer  (recurrence-driven commitment)       │
│                          named, content-addressed signs                 │
│                                                                         │
│                   ▼                                                     │
│             UmweltGraph ◄──► umwelt-sep::{encode,decode,SepTransport}   │
│            (heapless or Vec)       24-byte frame / 32-byte envelope     │
│                                    BLAKE3-keyed MAC, anti-replay        │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
                          │                    ▲
                          ▼                    │
     ┌─────────────────────────────────────────┴─────────────────┐
     │                 host-only crates (std)                    │
     │                                                           │
     │  umwelt-transport     loopback, UDP, sim-LoRa fault-inject│
     │  umwelt-cli           run, inspect, replay, simulate      │
     │  umwelt-vocab         YAML vocabulary loader              │
     │  umwelt-pyo3          Python bindings (maturin develop)   │
     └───────────────────────────────────────────────────────────┘
```

Everything above the dashed line is `no_std`-clean and cross-compiles
to `thumbv7em-none-eabihf` on every commit (see `scripts/verify.sh`
and `scripts/size_audit.sh`). Everything below is host-only and opts in
to `std`.

### Why three layers?

A three-stage condensation — threshold → correlate → commit — is the
minimum that yields signs *meaningful across agents*. An Icon is local
to one sensor; an Index ties two sensors together in time; a Symbol is
a recurrence-driven name two agents observing the same phenomenon will
independently agree on (content-addressed via FNV-1a 64, ADR-0014).
Agents share Symbols over SEP. They do not ship raw sensor frames.

### Why SEP?

Because a swarm of edge agents needs a wire format that works in
packet-loss regimes LoRa and ESP-NOW introduce, with no central
coordinator and no cloud. SEP is a 24 B inner frame carrying one sign,
integrity-checked with CRC-16/CCITT, wrapped in a 32 B authenticated
envelope (that frame plus an 8 B BLAKE3-keyed MAC under a pre-shared
key), hop-limited, deduplicated, and anti-replay windowed (ADR-0011
through ADR-0012). One frame carries one sign.

## What it is

- **An intelligence layer for physical agents.** Runs on the silicon
  they already carry. Produces structured meaning; exposes it to
  higher-level planners or LLM agents over a typed API.
- **A mesh protocol.** Agents gossip signs peer-to-peer. A partition
  reconverges by re-gossip once it rejoins.
- **A measurement discipline.** Every performance claim maps to a rig
  in [`docs/performance.md`](docs/performance.md) that runs in the
  verify gate (or is marked `P8-awaiting-hardware`).

## What it is not

- **Not a VLA.** We do not output motor torques or policies. A π₀ /
  GR00T / OpenVLA stack can sit above Umwelt Engine and consume its
  Symbol stream as structured context.
- **Not a cloud service.** There is no server side. Per-agent snapshots
  are local JSON; the metrics endpoint binds to `127.0.0.1` by default.
- **Not a hardware abstraction.** Board-support crates (ESP32-S3, nRF,
  STM32) live in downstream port repos, per ADR-0022 and the P8 non-goal.

## Crate layout

```
crates/
  umwelt-core/       Sign, SignId, Q15, SemioticLayer, SignDomain, pipeline  [no_std]
  umwelt-sep/        Sign Exchange Protocol — 24B inner / 32B envelope        [no_std]
  umwelt-drivers/    Sensor driver traits                                     [host]
  umwelt-transport/  Loopback / UDP / simulated-LoRa transports               [host]
  umwelt-vocab/      YAML vocabulary loader (terrain.yaml, swarm.yaml)        [host]
  umwelt-cli/        `umwelt` binary — run | inspect | replay | simulate      [host]
  umwelt-pyo3/       Python bindings via PyO3 0.22 (abi3-py38, maturin)       [host]
examples/
  configs/           Reference mission configs
  notebooks/         Jupyter — Hyde Park replay over the PyO3 extension
vocabularies/        Reference vocabularies (terrain.yaml, swarm.yaml)
scripts/
  verify.sh          The one gate. Cheap → slow order.
  size_audit.sh      Cortex-M size audit (P8.2)
docs/
  decisions.md            ADRs (ADR-0001 … ADR-0022)
  performance.md          Every perf claim, every rig, every tolerance
  embedded_hal_compat.md  Embassy / embedded-hal compatibility audit
```

## In the browser

[`web/`](web/) is a single-file `cdylib` crate, kept outside the workspace,
that wraps one `SemioticPipeline<4, 64, 8>` in four C-ABI exports and a
flat event buffer. `cargo build --manifest-path web/Cargo.toml --target
wasm32-unknown-unknown --release` produces a 22 KB module; the page in
[`docs/`](docs/) loads it directly, with no wasm-bindgen and no
JavaScript reimplementation of anything. `node web/check.js` replays
the Hyde Park trace through the module and prints the same counts the
Rust example prints — 18 icons, 23 indices, 7 symbols, the same first
five emissions — which is the check that the two builds agree.

## Live metrics

Start a mission with `--metrics-bind 127.0.0.1:9099` and scrape
`http://127.0.0.1:9099/metrics` with Prometheus, or `curl` it. The
endpoint sends `access-control-allow-origin: *`, so a static page can
poll it from the browser.

## Python bindings

```bash
cd crates/umwelt-pyo3
maturin develop --release
python scripts/pyo3_smoke.py
```

The `umwelt` Python module exposes `Pipeline`, `q15_from_f32`,
`q15_to_f32`, and module constants `CHANNELS`, `ICON_WINDOW`,
`ICON_BINS`. See [`crates/umwelt-pyo3/README.md`](crates/umwelt-pyo3/README.md)
for the full surface and [`examples/notebooks/hyde_park_replay.ipynb`](examples/notebooks/hyde_park_replay.ipynb)
for a worked notebook.

## Documentation index

- [`docs/decisions.md`](docs/decisions.md): every ADR, chronological
- [`docs/performance.md`](docs/performance.md): measurement plan and the Cortex-M size audit
- [`docs/embedded_hal_compat.md`](docs/embedded_hal_compat.md): Embassy / embedded-hal compatibility audit
- [`CHANGELOG.md`](CHANGELOG.md): what shipped in each phase

## Contributing

Read [`docs/decisions.md`](docs/decisions.md) first. It records the
constraints any implementation has to respect (`no_std` core, zero
`unsafe`, fixed-point maths, tests before features). A change that
needs a new convention needs a new ADR. Run `bash scripts/verify.sh`
before opening a pull request.

## License

Apache-2.0 — see [`LICENSE`](LICENSE).

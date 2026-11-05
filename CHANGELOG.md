# Changelog

All notable changes to the Umwelt Engine are recorded here. Format
follows [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/);
versioning follows [Semantic Versioning](https://semver.org/). Each
entry is tagged with the phase it shipped in; ADR numbers reference
[`docs/decisions.md`](docs/decisions.md).

## [Unreleased]

_Nothing yet. Post-v0.1 work lands here._

## [0.1.0] — 2026-04-20

First feature-complete release. v0.1 spans the full P0–P9 plan:
scaffolded workspace through to the Hyde Park replay example. The
core runtime is `no_std` on Cortex-M4F (104 586 B `.text + .data`,
≈39 % of a 256 KB budget), ships with PyO3 bindings, four CLI verbs,
a Prometheus `/metrics` endpoint, and a measurement-backed
SEP wire format authenticated with BLAKE3-keyed MAC-64. See
[`docs/decisions.md`](docs/decisions.md) for every ADR landed.

### Added

#### P0 — Scaffold
- Cargo workspace with five crates (`umwelt-core`, `umwelt-sep`,
  `umwelt-drivers`, `umwelt-transport`, `umwelt-cli`).
- ADR log in `docs/decisions.md`.

#### P0.5 — Wire format, auth and tooling
- ADR-0011: canonical 24-byte sign-per-frame SEP wire format.
- ADR-0011a: 32-byte authenticated SEP envelope (inner + BLAKE3-keyed
  MAC-64).
- `rust-toolchain.toml` pinned to Rust 1.95.0.
- `scripts/verify.sh` as the single gate before every commit (fmt,
  check, no_std gates on core/sep, test, clippy with `-D warnings`).
- `Q15(i16)` newtype in `umwelt-core` replacing raw `i16` on `Sign`;
  `from_f32` / `to_f32` / `saturating_add` / `saturating_mul_q15`;
  proptest round-trip within 1 LSB.
- `SignId.0` made crate-private; `SignId::new`/`raw` accessors.
- Optional `serde` feature on `umwelt-core` (`Sign`, `SignId`, `Q15`,
  `SemioticLayer`, `SignDomain`).
- Root `README.md` (initial stub).
- ADR-0012: SEP authentication via pre-shared key + BLAKE3-keyed MAC.
- ADR-0013: signification trigger via gated rolling Shannon entropy.
- `serde_yaml_ng` swapped in for unmaintained `serde_yaml`.
- `umwelt-vocab` crate with `SignVocabulary { types, composite_rules }`;
  reference `terrain.yaml` and `swarm.yaml` under `vocabularies/`.
- `SepTransport` trait in `umwelt-sep` (no_std, blocking, one-frame-
  per-call, `WouldBlock` convention). Loopback impl in
  `umwelt-transport`.
- `docs/performance.md`: single source of truth for every
  quantitative claim.

#### P0.7 — Pre-P1 prerequisites
- ADR-0014: `SignId` content-address hash via FNV-1a 64-bit with
  collision promote-and-log policy.
- ADR-0015: three-class exponential decay profiles
  (OBSTACLE=350 ms, CONTAMINATION=14 s, TERRAIN=70 s) with Q15
  per-tick constants.

#### P1 — Core types
- `Sign` with full field set (layer, domain, magnitude, coherence,
  first-seen, last-seen, intensity, token).
- `SignId` allocator with content-addressed hashing (ADR-0014).
- `UmweltGraph` storage trait with fixed-capacity (`heapless`) and
  growing (`Vec`, host-only) backends.
- Content-addressed insertion with deduplication.
- Query API: by channel, by layer, by time window.
- Proptest invariants: insertion preserves order; dedup is idempotent.

#### P2 — Sign Exchange Protocol
- Little-endian wire format with CRC-16/CCITT integrity on the inner
  frame (ADR-0011); BLAKE3-keyed MAC-64 on the envelope (ADR-0012).
- Encode/decode with property-tested round-trip.
- Hop-limit enforcement and duplicate suppression via `SeenSet`.
- Anti-replay window.
- `cargo-fuzz` target for the decoder.

#### P3 — Semiogenesis
- Icon layer: per-channel threshold + ring-buffer aggregation,
  Shannon-entropy gate (ADR-0013).
- ADR-0017 and Index layer: pairwise temporal correlator with
  500 ms window, geomean coherence, callback emission.
- ADR-0018 and Symbol layer: count-threshold commitment with
  recurrence window, content-addressed symbol-id.
- Exponential decay on cold signs (ADR-0015).
- Offline replay harness from sensor logs.

#### P4 — Transport
- In-memory `Loopback` transport for tests; `LoopbackMesh<A,Q>` for
  multi-agent.
- UDP transport for multi-process local simulation.
- ADR-0019 and `SimLora<T,Q>` simulated-LoRa wrapper: Bernoulli loss,
  base + jitter latency, runtime-toggled partition, injected clock,
  composable over any `SepTransport`.
- Six-agent 30 s swarm integration test with full (i,j)-pair
  coverage assertion.

#### P5 — CLI
- ADR-0020: `clap` (derive) for arg parsing; Small/Medium
  pipeline-profile dispatch (`<4,64,8>` / `<8,128,16>`).
- `umwelt run` — boot an agent from YAML config, loopback / UDP
  transports, real-time tick pacing.
- `umwelt inspect` — dump `umwelt.snapshot.v1` JSON; ADR-0021 pins
  `serde_json` as a host-only workspace dep.
- `umwelt replay` — offline NDJSON sensor-log replay
  (`umwelt.sensor_log.v1`).
- `umwelt simulate` — N-agent swarm over `LoopbackMesh`
  (N ∈ {2, 4, 8}), per-agent deterministic seeds, per-agent snapshots.
- `EngineConfig` schema with YAML example checked in.

#### P6 — Observability
- Tracing spans around every mission phase (`run`/`replay`/
  `simulate`), carrying `kind`, `agent_id`, and `profile`.
- NDJSON sign-flow export via `--events-out <PATH>`
  (`umwelt.events.v1` header, one JSON line per event).
- Live HTTP `/metrics` endpoint (Prometheus text format) via
  `--metrics-bind <ADDR>`; CORS-enabled, hand-rolled
  `TcpListener`-backed server with cooperative shutdown.

#### P7 — Python bindings
- `umwelt-pyo3` crate with PyO3 0.22 (`abi3-py38`) wrapping
  `SemioticPipeline<4,64,8>`: `Pipeline` class,
  `observe(channel, q15, tick_us) -> list[dict]`,
  `q15_from_f32`/`q15_to_f32`, module constants.
- `maturin develop` integration with `extension-module` Cargo
  feature gated opt-in so `cargo check --workspace` still passes.
- Notebook example (`examples/notebooks/hyde_park_replay.ipynb`)
  driving the extension end-to-end.

#### P8 — Hardware profile (paper-only)
- `rust-toolchain.toml` pins `thumbv7em-none-eabihf` as a required
  rustup target; `scripts/verify.sh` runs cross-compile gates on
  `umwelt-core` and `umwelt-sep` when the target is installed.
- `scripts/size_audit.sh`: measured `.text + .data` on Cortex-M4F
  at **104 586 B** (≈39 % of the 256 KB budget), libm-dominated.
- `docs/performance.md` rows B3/B4/B5 backfilled with measured
  values; new "§ P8.2 size audit — measurement record" section.
- ADR-0022 and `docs/embedded_hal_compat.md`: compatibility audit
  for `embedded-hal 1.0`, `embedded-io 0.6`, `embedded-io-async`,
  `embassy-executor`/`embassy-time`, and `lora-phy`. Three adapter
  patterns as pseudocode (lora-phy direct; framed UART with
  length-prefix + CRC16-CCITT reusing `umwelt-sep::crc16_ccitt`;
  Embassy async wrapper). Verdict: `SepTransport` is
  ecosystem-ready with zero trait changes needed for v0.1.

#### P9 — Release
- **P9.1** — root `README.md` upgraded from the P0.5.7 stub to a
  276-line engineer-facing document: at-a-glance table, 30-second
  quickstart, 5-minute CLI tour, ASCII architecture diagram, what
  it is / is not, crate layout, documentation index.
- **P9.2** — this `CHANGELOG.md` (Keep-a-Changelog 1.1.0 format,
  phase-grouped under Added / Changed / Fixed / Security).
- **P9.3** — repo-root `LICENSE` carrying the canonical Apache-2.0
  text verbatim; the legal artefact the `license = "Apache-2.0"`
  SPDX identifier points at.
- **P9.4** — crates.io metadata across every workspace crate
  (`repository` / `homepage` / `readme` via workspace inheritance,
  `documentation = https://docs.rs/<crate>`, 5 `keywords` and 2–3
  pinned `categories`). `umwelt-core` and `umwelt-sep` carry
  `[package.metadata.docs.rs]` with `all-features = true` and
  `rustdoc-args = ["--cfg", "docsrs"]` so docs.rs renders the full
  API surface on its host-target builder. Names are ready to be
  reserved; crates are not yet published.
- **P9.5** — `examples/hyde_park/` — Python-free `cargo run -p
  hyde-park-replay` walkthrough that drives a checked-in 800-sample
  terrain NDJSON trace through the full Icon → Index → Symbol
  pipeline and prints a compact summary. Companion `gen-trace`
  binary regenerates the trace deterministically. Engineer-facing
  counterpart to the P7.3 PyO3 notebook.

### Changed

- `Sign` storage now uses `Q15(i16)` instead of raw `i16` across
  the board (P0.5.4).
- `SepTransport` trait authored in `umwelt-sep` rather than split
  across transport crates, so a downstream port crate implements
  one trait to participate (P0.5.12).
- Default features on `umwelt-transport` now include `loopback`
  so the workspace test gate covers the P4.1 mesh suite and P4.4
  swarm integration unconditionally (P4.4).

### Fixed

- **P0.8.1**: `ReplayGuard::check` previously invoked the
  side-effecting `SeenSet::observe` via `debug_assert_eq!`, which
  the compiler elides in release builds. Literal-duplicate
  detection was silently disabled in release. Fix splits the
  call from the assertion; a release-mode integration test
  guards against regression.
- **P0.8.2**: audited every `debug_assert*!` in the workspace for
  side-effecting arguments; split or justified each instance in
  one commit.

### Security

- **SEP authentication** (ADR-0012): every envelope frame carries
  an 8-byte truncated BLAKE3-keyed MAC under a pre-shared key.
- **Anti-replay window** (P2.5): `ReplayGuard` rejects literal
  duplicates and out-of-order frames beyond the window; the
  release-mode fix (P0.8.1) makes this enforceable.
- **Zero `unsafe`**: `#![deny(unsafe_code)]` on every shipped
  crate, enforced by clippy and CI.
- **Fuzz harness** (P2.6): `cargo-fuzz` target exercises the SEP
  decoder against malformed input.

# Performance Measurement Plan

**Scope:** every quantitative performance claim made about the Umwelt
Engine, in this README or anywhere else.

## Why this document exists

A number the repo cannot reproduce on demand is a liability. This doc
is the single source of truth for how each claim gets measured, what
"pass" means, and when the rig enters the verification gate.

Rules:

1. Every claim listed below maps to a **rig** (how we measure), a
   **target** (the number we assert), a **tolerance** (how close is
   "pass"), and a **phase** (when the rig lands).
2. A claim with no rig is deleted. It is not hedged or starred.
3. Rigs live in-repo, run in CI, and are called from
   `scripts/verify.sh` the moment they're stable. Numbers that drift
   more than tolerance fail the build.
4. Numbers that need hardware not yet on the bench (ESP32-S3,
   RPi Zero 2W) are marked **P8-awaiting-hardware** and stated as
   targets, never as measured values, until a rig exists.

## Measurement rigs (inventory)

| Rig | What it measures | Where it lives | Gate |
|---|---|---|---|
| `criterion` microbenches | per-function wall-clock on host | `crates/*/benches/*.rs` | `cargo bench` (P4.4) |
| `cargo size` | static binary size per crate | `scripts/size_audit.sh` | size gate (P8.2) |
| Graph RSS probe | live heap footprint under trace replay | `crates/umwelt-core/tests/rss_probe.rs` | `cargo test rss_probe` (P3.5) |
| SEP codec round-trip bench | encode + decode + MAC verify | `crates/umwelt-sep/benches/codec.rs` | `cargo bench` (P2.2) |
| Loopback transport bench | send + recv one frame | `crates/umwelt-transport/benches/loopback.rs` | `cargo bench` (P4.1) |
| Replay harness | end-to-end tick latency on recorded sensor log | `examples/replay_bench/` | `cargo bench` (P3.5) |
| Cortex-M size cross-build | `.text` + `.data` on `thumbv7em-none-eabihf` | `scripts/size_audit.sh` | size gate (P8.2) |
| Hardware rig (ESP32-S3) | on-device latency + SRAM | external `umwelt-esp32` repo | deferred (P8) |

All host-side rigs run on a pinned machine profile: Apple M-series,
Rust 1.95.0, release profile, `RUSTFLAGS=""`, no background load.
Numbers are recorded as **median of 1000 iterations**; tolerance is
**±20 %** around the target unless a row says otherwise.

## Claims and rigs

### A. On-device inference / tick budgets

All rows are **targets**, not measurements, until the rig exists.

| # | Claim (where) | Platform | Target | Rig | Phase | Verdict |
|---|---|---|---|---|---|---|
| A1 | Sensing loop tick | RPi Zero 2W | <100 ms | replay harness on RPi | P8-awaiting-hardware | keep as **target** |
| A2 | Sensing loop tick | ESP32-S3 | <80 ms | on-device rig | P8-awaiting-hardware | keep as **target** |
| A3 | Entropy computation / channel | host (proxy for RPi) | <1 ms | `umwelt-core::entropy` criterion bench | P3.1 (rig) → P3.5 (target) | keep, measure on host first |
| A4 | Entropy computation / channel | ESP32-S3 | <5 ms | on-device rig | P8-awaiting-hardware | keep as **target** |
| A5 | Sign formation (rule-based typer) | host | <5 ms | `umwelt-core::semiogenesis` bench on replay slice | P3.3 | keep |
| A6 | Sign formation (GBT typer) | host | <2 ms | same bench, GBT variant | P3.3 | **defer** — GBT typer not in v0.1 scope; mark as v0.2 target or **delete** |
| A7 | Graph update tick | host | <50 ms | `UmweltGraph::tick` bench, 1 000-node fixture | P1.5 | keep |
| A8 | Graph update tick | ESP32-S3 | <200 ms | on-device rig | P8-awaiting-hardware | keep as **target** |
| A9 | Spatial query (100 nodes) | host | <5 ms | `UmweltGraph::query_by_region` bench | P1.5 | keep |
| A10 | Graph queryable (Index layer) | unspecified | <10 ms | superseded by A9 | — | **delete or merge with A9** (imprecise as written) |
| A11 | `SignId` content-address hash (FNV-1a 64, ADR-0014) | host | ≤200 ns / call | `umwelt-core` criterion bench `benches/hash.rs` (`sign_id/from_identity`) | P1.2 (rig landed) | keep — rig lives in-repo; median to be backfilled on first `cargo bench` run |

### B. Memory footprint

| # | Claim (where) | Scope | Target | Rig | Phase | Verdict |
|---|---|---|---|---|---|---|
| B1 | Total RAM footprint (RPi) | whole agent | <256 MB | `ps rss` on replay harness | P8-awaiting-hardware | keep as **target** |
| B2 | Total RAM footprint (ESP32-S3) | whole agent | <128 KB SRAM | on-device rig | P8-awaiting-hardware | keep as **target** |
| B3 | `<1 MB on microcontrollers` (v0.1 budget) | core crate binary | <1 MB on cortex-m | `scripts/size_audit.sh` | P8.2 | **measured 2026-04-20: ~102 KiB** (≈10 % of <1 MB budget) |
| B4 | P8.2 size audit | core crate | <256 KB on cortex-m | same as B3 | P8.2 | **measured 2026-04-20: ~102 KiB** (≈39 % of 256 KB budget) |
| B5 | Stretch target "<64 KB" | core runtime footprint | <64 KB flash + RAM on cortex-m | B3 rig + linker map | P8.2 | **downgrade to target until measured** — 2026-04-20 audit: core + no_std deps ≈ 102 KiB (dominated by libm ≈ 100 KiB; `umwelt-core` itself is ~398 B). Sub-64 KiB only reachable via libm feature-gating; stays a **target**. |

### C. Transport

| # | Claim (where) | Scope | Target | Rig | Phase | Verdict |
|---|---|---|---|---|---|---|
| C1 | SEP encode + transmit | host loopback | <10 ms | `umwelt-transport::loopback` criterion bench | P4.1 (loopback landed P0.5.12) | keep |
| C2 | SEP encode + transmit | ESP32-S3 + radio | <5 ms | on-device rig | P8-awaiting-hardware | keep as **target** |
| C3 | SEP encode round-trip | host | implicit, <50 µs | SEP codec bench | P2.2 | **add explicit target** |
| C4 | ESP-NOW latency | 200 m open field | <10 ms | radio rig | P8-awaiting-hardware | keep as **target**, source-tag as Espressif datasheet |
| C5 | LoRa latency | 2 km open field | ~200 ms | radio rig | P8-awaiting-hardware | keep as **target**, source-tag as SX1262 datasheet |
| C6 | ESP-NOW payload | one frame | 250 B / 10 SEP messages | arithmetic: 250 / 24 = 10.4 | N/A (spec) | keep, this is a *spec* claim, not a measurement |

### D. Decay constants

The "obstacles 350 ms, contamination 14 s, terrain 70 s" claim on
the published spec is a **design parameter**, not a measurement. It is
pinned by **ADR-0015** (Sign decay profiles: three-class
exponential, Q15 per-tick) as of P0.7.3.

| # | Claim | Status | Rig |
|---|---|---|---|
| D1 | Obstacle decay 350 ms (5% retention) | **spec** per ADR-0015 (`ALPHA = 13920`) | proptest in P3.4 confirms 5.00 % ±0.02 % retained after τ |
| D2 | Contamination decay 14 s (5% retention) | **spec** per ADR-0015 (`ALPHA = 32073`) | same |
| D3 | Terrain decay 70 s (5% retention) | **spec** per ADR-0015 (`ALPHA = 32627`) | same |

These are **pinned design constants**, not
targets. P3.4's proptest is the only "measurement" — it asserts the
decay curve matches the spec, it does not tune the constant.

### E. Rates

| # | Claim | Rig | Phase | Verdict |
|---|---|---|---|---|
| E1 | 10 Hz sensor polling loop | scheduler invariant test | P3.1 | keep, cheap |
| E2 | `<5 ms per driver on ESP32` | on-device rig | P8-awaiting-hardware | keep as **target** |

## P8.2 size audit — measurement record

**Rig:** `scripts/size_audit.sh` (2026-04-20, commit pending).
**Target:** `thumbv7em-none-eabihf` (ARM Cortex-M4F with hardware float),
`--release` profile, `--no-default-features`.

### Method

The workspace release profile enables fat LTO, so rustc emits LLVM
bitcode into each `.rcgu.o`. `llvm-size` cannot read bitcode, so the
rig builds into an isolated `target/size_audit/` tree with a
`--config` override that sets `[profile.release] lto = false` and
`rustflags = ["-C", "lto=off", "-C", "embed-bitcode=no", "-C",
"linker-plugin-lto=off"]`. rustc then emits native ARM ELF objects.
For each `.rlib` cargo produces, the rig extracts its `.rcgu.o`
members via rustup's `llvm-ar` and sums `.text + .data` per section
using rustup's `llvm-size -B`.

This is a **worst-case archive-level sum** — no cross-crate inlining,
no linker dead-strip — so the real linked binary is always smaller
than reported.

### Result (2026-04-20)

| Crate | `.text` (B) | `.data` (B) |
|---|---:|---:|
| `libm` (transitive, no_std math) | 100 212 | 0 |
| `bitflags` (transitive) | 3 669 | 0 |
| `umwelt_core` | 398 | 0 |
| `byteorder` (transitive) | 241 | 0 |
| `hash32` (transitive) | 66 | 0 |
| `heapless`, `stable_deref_trait`, `umwelt_sep` (headers-only in this build) | 0 | 0 |
| **Total** | **104 586 B** | **0 B** |

**Headroom:** 157 558 B (≈60 %) below the 256 KB budget.

The `umwelt_sep` rlib measures zero because the audit builds
`umwelt-sep` with `--no-default-features` and no caller instantiates
its public surface here; the SEP codec compiles but emits no code
under dead-code elimination. When the cortex-m harness lands (P8.3
and beyond) the `.text` contribution of `umwelt-sep` will be non-zero
and tracked in this table.

### Observations

- `libm` dominates (96 %). It provides soft-float transcendentals
  (`expf`, `log2f`, `sqrtf`) the `umwelt-core` entropy kernel pulls in.
  Cortex-M4F has hardware `VSQRT`/`VFMA` but not `expf`/`log2f`, so
  libm is unavoidable without the `libm` feature pruning.
- `umwelt-core` and `umwelt-sep` together contribute <5 KB of text.
  The B5 "<64 KB" aspiration is a libm-shaping problem, not a core
  problem.
- The audit passes the B4 gate (<256 KB) by a factor of 2.5×. The
  number is recorded here on every audit run; any future regression
  that lifts it above budget fails the gate with exit code 2.

### How to reproduce

```
bash scripts/size_audit.sh           # human-readable report
bash scripts/size_audit.sh --json    # machine-readable JSON
```

The script exits 0 on pass, 1 on tooling error, 2 on budget violation.
It is not yet wired into `scripts/verify.sh` — it runs as an
on-demand gate because the isolated target-dir build takes ~30 s cold.


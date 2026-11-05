# Architecture Decision Records

Every non-obvious design choice lives here. A choice not covered here gets a new ADR
before it gets code.

---

## ADR-0001 — Primary language is Rust

**Date:** 2026-04-17
**Status:** Accepted

Rust gives us one codebase that runs on the ESP32-S3 (`no_std`, cortex-m),
the Jetson Orin (std + tokio), and a desktop developer machine. Memory
safety matters on hardware we cannot physically reach. We forgo the Python
ergonomics at the edge and regain them through a PyO3 binding (P7).

## ADR-0002 — Python is a companion, not a peer

**Date:** 2026-04-17
**Status:** Accepted

Python is allowed *only* in:
- PyO3 bindings (`umwelt-pyo3` crate, added in P7)
- Notebook-based analysis (`examples/notebooks/`)
- Data prep scripts outside the runtime (`scripts/`)

No Python in the runtime path.

## ADR-0003 — SEP frame layout *(superseded by ADR-0011)*

**Date:** 2026-04-17
**Status:** Superseded by ADR-0011 on 2026-04-18

Original proposal: little-endian, variable payload up to 228 B, CRC-16/CCITT
(poly 0x1021, init 0xFFFF) at tail, 16-bit msg id / 8-bit fragment index /
8-bit total for fragmentation, `u8` hop counter dropped at 8 hops.

Superseded because the public product commitment (see
`/fern-circuit-sim/umwelt.html`, SEP section) is a **24-byte fixed-width
sign-per-frame** protocol, which is cleaner semantically and avoids
fragmentation on the hot path. See ADR-0011 for the binding spec.

The CCITT CRC choice and the "little-endian, fixed-width" principle are
preserved and restated in ADR-0011. Fragmentation and variable payloads
are dropped from SEP v0.1 — if they prove necessary for bulk transfers
(vocabulary updates, replay bundles) they will live in a separate
transport under a future ADR, not in SEP.

## ADR-0004 — Fixed-point over floating-point in the core

**Date:** 2026-04-17
**Status:** Accepted

Core crate (`umwelt-core`) uses Q15 fixed-point for normalised quantities
(intensity, correlation, coherence). `libm` is available for when we
genuinely need float math (quaternion integration, trig). Rationale: not
every microcontroller has an FPU, and fixed-point is deterministic across
platforms.

Conversions:
- Q15: `i16`, range [-1.0, 1.0), step 2^-15
- Encoding a float `x` ∈ [-1, 1]: `(x * 32767.0).round() as i16`

## ADR-0005 — No unsafe, anywhere

**Date:** 2026-04-17
**Status:** Accepted

Every crate has `#![deny(unsafe_code)]`. If an embedded driver requires
unsafe (e.g. DMA), it is quarantined in a separate crate with a hardened
safe API. Exceptions must be added as a new ADR.

## ADR-0006 — Commit granularity

**Date:** 2026-04-17
**Status:** Accepted

One idea per commit. Every commit leaves `scripts/verify.sh` green.
Commit subjects carry the phase and step they belong to:
`P<phase>.<step>: <summary>`.

## ADR-0007 — Tests before features

**Date:** 2026-04-17
**Status:** Accepted

Each step: write the failing test, then the code that makes it pass.
Proptest for invariants, Criterion for anything on a hot path.

## ADR-0008 — No_std preserved at the core

**Date:** 2026-04-17
**Status:** Accepted

`umwelt-core` and `umwelt-sep` must stay `no_std`-compatible. The `std`
feature is additive only. CI must compile both crates with
`--no-default-features` as a gate. This check is added in P1.

## ADR-0009 — Build dashboard *(withdrawn)*

**Date:** 2026-04-17
**Status:** Accepted

The static build-progress dashboard was dropped from the public
release. Runtime counters are still served in Prometheus text format
by `umwelt run --metrics-bind` (see the README).

## ADR-0010 — No on-target hardware validation in v0.1

**Date:** 2026-04-17
**Status:** Accepted

All embedded work in v0.1 is compile-only. Any step that requires
flashing or measurement on real hardware is marked
`P8-awaiting-hardware` and deferred to the first board port.

## ADR-0011 — Canonical SEP wire format: 24-byte sign-per-frame

**Date:** 2026-04-18
**Status:** Accepted (binding at P2.1). Supersedes ADR-0003.

### Decision

SEP v0.1 is a **fixed-width, 24-byte, sign-per-frame** protocol. One
wire message carries exactly one interpreted sign. No fragmentation, no
variable payload. The encoding is little-endian, matches the published
product specification (SEP section),
and is what every consumer of `umwelt-sep` must encode and decode.

### Rationale

Two candidate formats were on the table:

1. **Published 24-byte frame** — agent_id, sign_type, magnitude, ttl|flags,
   spatial_ref (x,y,z), timestamp, confidence, priority. No CRC, no
   fragmentation. Sign-per-frame.
2. **ADR-0003 framed variant** — variable payload up to 228 B, CRC-16
   tail, 16-bit msg-id fragmentation, `u8` hops at 8.

The published frame wins:

- **Public commitment.** The published spec is the product's canonical
  external description. Divergence between spec and product is a reputational
  cost we will not pay.
- **Semiotic cleanliness.** SEP carries *interpreted signs*, not
  payload. A sign is atomic — fragmenting one across frames is a
  category error. Bulk transfers (vocab updates, replay bundles) are
  not signs and belong to a separate protocol layer if they become
  necessary.
- **LoRa-friendly.** 24 B fits comfortably inside both ESP-NOW (250 B
  payload) and LoRa SX1276 (up to 256 B) single frames — no
  fragmentation machinery is needed on the hot path.
- **Deterministic timing.** Fixed-width frames make encode and decode
  branchless and `no_std`-friendly; good for microcontroller latency
  budgets.
- **Smaller code surface.** No reassembly buffers, no out-of-order
  queues, no frag-timeout state machine. Reduces fuzz and attack
  surface for P2.6 (fuzz target).

### Exact layout (little-endian, 24 bytes total)

```
  offset  size  field              notes
  ──────  ────  ─────────────────  ─────────────────────────────────────
    0      4    agent_id : u32     CRC32 of MAC address (per published spec)
    4      1    sign_type : u8     index into vocabulary (P0.5.11)
    5      1    magnitude : u8     unsigned 0..=255 (Q0.8 unit scale)
    6      1    ttl_flags : u8     high nibble = ttl (0..=15, drop at 0)
                                   low  nibble = flags (bit 0 = urgent,
                                                        bit 1 = composite,
                                                        bits 2–3 reserved)
    7      1    _reserved0 : u8    MUST be 0 on send, ignored on recv
    8      2    spatial_x : i16    centimetres, signed, range ±327.67 m
   10      2    spatial_y : i16    centimetres, signed
   12      2    spatial_z : i16    centimetres, signed
   14      2    _reserved1 : u16   MUST be 0 on send, ignored on recv
   16      4    timestamp : u32    agent-local monotonic, milliseconds
                                   since agent epoch; wraps ~49 days
   20      1    confidence : u8    unsigned 0..=255 (Q0.8)
   21      1    priority   : u8    unsigned 0..=255 (Q0.8)
   22      2    crc16      : u16   CCITT, poly 0x1021, init 0xFFFF,
                                   computed over bytes 0..22
  ──────
   24
```

### Reconciled fields (from ADR-0003)

- **CRC** retained. CCITT has a hardware accelerator on ESP32-S3 and
  the 2-byte cost is worth it since ESP-NOW and LoRa both already CRC
  at the PHY — our CRC is belt-and-braces and also covers UDP transport
  where no PHY CRC is present.
- **Hop limit** is 4-bit (0..=15) per the published spec, not 3-bit at 8 as
  ADR-0003 suggested. Drop-at-zero semantics (decrement before relay).
- **Fragmentation** is dropped. Non-goal for SEP v0.1.
- **Variable payload** is dropped. Non-goal for SEP v0.1.

### Reserved bytes

Offsets 7 and 14–15 are reserved for forward compatibility. Encoders
MUST write zero; decoders MUST accept any value (log if non-zero at
debug level). Future ADRs may claim these bits for sequence numbers,
epoch bits, or auth tags.

### Authentication

Out of scope for ADR-0011. A keyed MAC is planned in **ADR-0012**
(P0.5.8). Until then SEP is unauthenticated and MUST NOT be exposed on
untrusted networks.

### Implementation contract (P2.1)

- `umwelt-sep` exposes `const SEP_FRAME_BYTES: usize = 24;`
- `fn encode(frame: &SepFrame, out: &mut [u8; 24])`
- `fn decode(bytes: &[u8; 24]) -> Result<SepFrame, SepError>`
- Both are `no_std`, allocation-free, and branch-predictable.
- Round-trip property tests are P2.2.
- Fuzz target (P2.6) treats any 24-byte input as potentially hostile.

### Consequences

- ADR-0003 is marked superseded (above).
- P2.1 is now scoped: the layout is frozen.
- P0.5.11 (`umwelt-vocab`) must keep the `sign_type` space to 8 bits
  (256 types). Composite rules form higher-order signs but still
  occupy one `sign_type` slot each.
- P0.5.8 (ADR-0012, auth) and P0.5.9 (ADR-0013, signification trigger)
  remain the next decision steps; they do not conflict with this ADR.

## ADR-0011a — SEP authenticated envelope: 32-byte outer frame

**Date:** 2026-04-18
**Status:** Accepted (binding at P2). Amends ADR-0011.

### Decision

The 24-byte layout fixed by ADR-0011 is the **inner frame**. When SEP
is exposed on an untrusted transport, it is wrapped in a 32-byte
**authenticated envelope**: the 24-byte inner frame followed by an
8-byte truncated BLAKE3-keyed MAC (see ADR-0012 for the MAC itself).

```
  offset  size  field
  ──────  ────  ───────────────────────────────────
    0     24    inner : [u8; 24]   (per ADR-0011)
   24      8    mac   : [u8; 8]    (per ADR-0012)
  ──────
   32
```

### Rationale

P0.6.2 of the rolling delta review (2026-04-18) flagged that ADR-0011's
24 B frame has only 3 reserved bytes — too few to carry a useful MAC
tag. The two candidates were:

1. **Extend SEP to 32 B.** Break the "24 B always" promise in favour of
   atomic per-frame auth.
2. **Companion auth frame.** Keep 24 B inviolable; transmit MACs
   out-of-band over a sibling frame type covering one or many signs.

We take option 1. Atomic per-frame auth wins on:

- **Simpler decoder.** One branch (`len == 24` vs `len == 32`), no
  correlation between frames, no window to reconstruct a batched MAC.
- **No separation failure.** On a partition-tolerant mesh, a sibling
  auth frame can arrive arbitrarily late, arbitrarily ordered, or not
  at all. Per-frame auth fails closed without special cases.
- **Still small.** 32 B is well under ESP-NOW's 250 B and LoRa's 256 B
  single-frame budgets. No fragmentation pressure.
- **Preserves ADR-0011's byte table unchanged.** Encoders for the
  unauthenticated path (loopback, unit tests) continue to produce
  exactly 24 B; the envelope is strictly additive.

### Transport contract

Each `SepTransport` (trait landing in P0.5.12) declares at construction
time whether it carries **inner** frames (24 B, unauthenticated) or
**envelope** frames (32 B, authenticated):

- **Inner-only** — in-memory loopback, intra-process test harness.
  Auth is irrelevant; no key material is required.
- **Envelope** — UDP, LoRa, ESP-NOW, any shared-medium transport.
  Key material is required at construction; decoder rejects 24-byte
  inputs.

Mixing the two on one wire is a configuration error and must fail
fast at boot.

### Consequences

- `umwelt-sep` exposes `const SEP_FRAME_BYTES: usize = 24;` **and**
  `const SEP_ENVELOPE_BYTES: usize = 32;`. Both are public; the
  transport picks which applies.
- The encoder/decoder pair in P2.1 covers the 24-byte inner; MAC
  compute/verify lands with P2 auth (implementation follows ADR-0012).
- ADR-0011's reserved bytes (offsets 7 and 14–15) remain reserved —
  they are not consumed by auth.
- ADR-0011's "24 B always" phrasing is hereby refined: **the inner
  frame is always 24 B**.

## ADR-0012 — SEP authentication: pre-shared key, BLAKE3-keyed MAC-64

**Date:** 2026-04-18
**Status:** Accepted (binding at P2). Implementation follows in P2.

### Decision

SEP envelope frames (per ADR-0011a) carry an 8-byte authentication tag
computed as the first 8 bytes of **BLAKE3 in keyed mode** over the
24-byte inner frame. The key is a fleet-wide 256-bit pre-shared secret
loaded at boot from agent configuration.

```
  mac := BLAKE3_keyed(key, inner_frame)[0..8]
```

Verification recomputes `mac` over the received inner and constant-time
compares against the received tag. Failures drop the frame silently
(no error reply — a mesh attacker should not learn which keys are in
rotation from error timing).

### Rationale

- **BLAKE3 keyed mode** is a PRF, drop-in for a MAC, faster than
  HMAC-SHA256 on both ARM Cortex-M and Xtensa (ESP32-S3), and has
  production no_std Rust implementations (`blake3` with
  `default-features = false`).
- **64-bit tag** is sufficient for a mesh context where the attacker
  gets one blind forgery attempt per frame and the replay window is
  short (per-agent monotonic `timestamp` in the inner frame, bound by
  agent uptime). 2⁻⁶⁴ per-frame forgery probability is comparable to
  WireGuard's Poly1305 truncation in similar deployments. A larger
  tag costs frame budget disproportionate to the threat model.
- **Pre-shared key** is the simplest credible v0.1. Fleet operators
  provision a single secret to their swarm; cross-fleet isolation
  comes from using different secrets. Per-agent keys and key agreement
  are deferred to v0.2 (tracked in non-goals).
- **Inner is the auth scope**, not the envelope. The MAC covers what
  was interpreted and encoded by the sender; adding the MAC bytes back
  to the auth scope would create a self-reference and break standard
  MAC constructions.

### Anti-replay

The inner frame carries `agent_id` (offset 0) and `timestamp` (offset
16). A receiver maintains, per `agent_id`, the highest observed
monotonic `timestamp` plus a sliding acceptance window (window size
configurable; default 1024 ms). Frames older than the window are
dropped. Wrapping (timestamps reset every ~49 days) is handled by
watching for large backward jumps and treating them as legitimate
epoch rollover only if the agent re-authenticates a fresh session —
which for v0.1 simply means the receiver resets its high-water mark on
a provable authenticated frame whose timestamp is below the current
one by more than half the wrap. A fuller anti-replay design lives with
P2.5.

### Key provisioning (v0.1)

- Key lives in agent config YAML as a 64-hex-char field `fleet_key`.
- Loaded once at boot into a `[u8; 32]` in `umwelt-sep`'s runtime
  state; never serialised back out, never printed to logs.
- If the config is missing `fleet_key` **and** the configured
  transport is not inner-only, boot fails with a clear error. No
  implicit zero-key fallback.

### Out of scope

- **Per-agent keys, key agreement, key rotation.** Deferred to v0.2;
  P0.5 non-goals.
- **Forward secrecy.** Not a v0.1 claim.
- **Attested identity.** No PKI, no certificates, no TOFU database.
  The operator is trusted to distribute the pre-shared key.
- **Encryption of the payload.** SEP is authenticated but not
  encrypted. Sign traffic is low-cardinality and inference-resistant;
  confidentiality of individual frames is not a v0.1 goal.

### Implementation contract (P2 auth step)

- `umwelt-sep` depends on `blake3` (workspace dep to be added with an
  ADR-lite note on Cargo.toml when P2 auth starts; versioned
  `= "1"` with `default-features = false`).
- `fn seal(inner: &[u8; 24], key: &[u8; 32], out: &mut [u8; 32])`
- `fn open<'a>(envelope: &'a [u8; 32], key: &[u8; 32]) -> Result<&'a [u8; 24], SepError>`
- Both no_std, allocation-free, branch-predictable.
- The `open` comparison MUST be constant-time (subtle-style).
- Fuzz target (P2.6) includes both valid-keyed and random 32-byte
  inputs to confirm `open` rejects tampered envelopes.

### Consequences

- ADR-0011a is the wire-level partner; together they fix SEP's
  authenticated form.
- `umwelt-sep` gains `blake3` as a runtime dep when P2 auth lands.
  Size cost on cortex-m: ~4 KB .text (reference-only; measured under
  P8.2). If this exceeds budget we revisit with SipHash-based
  alternatives.
- Transport setup (P0.5.12 trait) must take `Option<&[u8; 32]>` so
  inner-only transports construct without a key.
- `decisions.md` non-goals for v0.1 now explicitly lists key
  agreement, forward secrecy, and payload confidentiality.

## ADR-0013 — Signification trigger: gated rolling Shannon entropy

**Date:** 2026-04-18
**Status:** Accepted (binding at P3.1). Implementation follows in P3.

### Decision

An **icon is emitted** from a channel's raw sample stream when both of
the following hold:

1. **Activity gate.** `|x_latest| ≥ MAGNITUDE_GATE` — the current
   sample has non-trivial magnitude in [`Q15`] units.
2. **Novelty filter.** The rolling Shannon entropy of the channel's
   last `WINDOW = 64` samples differs from its EWMA baseline by at
   least `ENTROPY_DELTA_BITS`.

The sample stream is per-channel; each channel carries an independent
ring buffer and baseline.

### Rationale

Two candidate designs were on the table:

1. **Plain magnitude threshold.** Cheap O(1), no history. Rejected:
   chronically-high channels emit constantly, chronically-low
   channels never emit, and the trigger has no notion of *novelty*.
2. **Rolling Shannon entropy** (what the public product page says).
   Context-aware, matches the semiotic framing — an icon is a
   *significant* sample, which is information-theoretically precisely
   the language of entropy.

We take option 2, gated by option 1. Pure entropy on a dead channel is
still expensive to compute and uninteresting; the activity gate keeps
the hot-path cost near zero when nothing is happening.

### Parameters (pinned)

| Name                 | Value     | Notes                                                          |
| -------------------- | --------- | -------------------------------------------------------------- |
| `WINDOW`             | 64        | samples per channel ring buffer                                |
| `BINS`               | 8         | uniform bins over `[-1.0, 1.0]` (3 bits of resolution)         |
| `H_MAX_BITS`         | 3         | entropy ceiling = `log2(BINS)`                                 |
| `MAGNITUDE_GATE`     | `Q15(3277)` | ≈ 0.1 — tuneable per channel but this is the default        |
| `ENTROPY_DELTA_BITS` | `Q15::from_f32(0.5)` | absolute delta in bits (Q15-scaled to [0, 3])         |
| `EWMA_ALPHA`         | 1/16      | baseline = `baseline + (H - baseline) >> 4`                    |
| `COOLDOWN_SAMPLES`   | 4         | minimum gap between two consecutive icons on one channel       |

`WINDOW = 64` at a 100 Hz sample rate yields a 640 ms observation
window, long enough to capture gait-scale events but short enough for
a human-tolerable detection latency.

### Computation (all Q15, all no_std, no heap)

- **Ring buffer.** `heapless::HistoryBuffer<Q15, WINDOW>` per channel.
- **Histogram update.** Incremental: on each new sample, decrement the
  outgoing sample's bin count and increment the incoming sample's bin
  count. O(1), no full-window rescans.
- **Entropy.**  `H = -Σ p_i * log2(p_i)` where `p_i = count_i / WINDOW`.
  `log2` is taken from a **256-entry precomputed Q15 table**
  indexed by the bin count (1..=WINDOW), so the hot path does a
  table lookup, not `libm::log2f`. `p_i = 0` contributes zero by
  convention (skip the term; do not compute `log2(0)`).
- **Baseline.** EWMA `baseline ← baseline + ((H - baseline) >> 4)`.
  Integer shift, no division.
- **Trigger.** `|H - baseline| ≥ ENTROPY_DELTA_BITS` **and** activity
  gate holds **and** `samples_since_last_icon ≥ COOLDOWN_SAMPLES`.

### Entropy scale (for reviewers)

| bin distribution        | H (bits) |
| ----------------------- | -------- |
| all mass in one bin     | 0        |
| mass split 50/50 in two | 1        |
| uniform over 4 bins     | 2        |
| uniform over 8 bins     | 3        |

A rising entropy signals a shift from a structured state (one or two
dominant bins) toward noise or a distribution change. A falling
entropy signals a structured state emerging out of prior noise. Both
count as signification events for v0.1; distinguishing rising vs.
falling entropy is deferred to the index layer (P3.2).

### Rejected alternatives

- **Variance / standard deviation.** Cheaper than entropy but fails
  on bimodal and multi-modal shifts with constant variance. Drops a
  legitimate semiotic event class.
- **Prediction error from an EWMA predictor.** Would work; essentially
  a first-order approximation of entropy. Rejected for consistency
  with the published spec's language and because the cost gap (at 64-sample
  window, ≤ 8 bins) is small.
- **`libm::log2f` on the hot path.** Rejected: non-deterministic
  cycle count across ARM Cortex-M variants, and we can afford a
  256-byte table. Table is computed once in `const fn` at build.

### Implementation contract (P3.1)

- `umwelt-core` exposes a per-channel `IconTrigger` type with
  methods `push(sample: Q15) -> Option<Sign>` that returns an Icon
  when the trigger fires.
- All state is owned by the caller; no globals.
- The log2 table is `const` and lives in `umwelt-core`.
- Proptests (P3.1):
  - constant stream → never fires
  - uniform noise → fires at most once per cooldown
  - step function from constant to uniform → fires within `WINDOW /
    2` samples of the step
- Bench (P4.4 deferred): `push` must complete in < 2 µs on a
  laptop-class x86 at release opt-level (reference target; real
  budget is the Cortex-M number from P8.2).

### Consequences

- P0.5.4's `Q15` type is now the unit of trigger-state arithmetic,
  not just sign field storage.
- The vocabulary (P0.5.11) sees an icon's `sign_type` come from
  the channel identity and the sign of `(H - baseline)` — the
  novelty-direction bit — so the 256-slot `sign_type` space is not
  crowded by trigger mechanics.
- Parameters above become defaults in the per-agent config YAML
  (P5.1). Operators can tune `MAGNITUDE_GATE` and
  `ENTROPY_DELTA_BITS` per channel without recompiling.
- Deferred to later ADRs: multi-scale windows (64 and 512),
  cross-channel joint entropy (belongs to P3.2's index layer, not
  here).

## ADR-0014 — SignId content-address hash: FNV-1a 64-bit

**Date:** 2026-04-18
**Status:** Accepted (binding at P1.2). Unblocks P0.7.1.

### Decision

`SignId` values in `umwelt-core` are derived by **FNV-1a 64-bit**
hashing a canonical byte encoding of the sign's identity fields.
The hash function is implemented **in-tree, zero-dep**, not behind a
crate boundary. Collision handling is **detect + promote + log once**
per process lifetime.

### Hash

FNV-1a 64-bit, offset basis `0xcbf29ce484222325`, prime
`0x100000001b3`. Byte-for-byte the same function already present as a
test helper at `crates/umwelt-core/src/lib.rs:439–450`, promoted from
test-only to a crate-public `umwelt_core::hash::fnv1a_64` in P1.2.

### Identity fields (canonical hash input)

The bytes fed into the hasher, in order:

| field           | encoding                                | width |
|-----------------|-----------------------------------------|-------|
| `layer`         | `SemioticLayer as u8`                   | 1 B   |
| `domain`        | `SignDomain as u8`                      | 1 B   |
| `sign_type`     | `u8` (vocabulary index, ADR-0011)       | 1 B   |
| `agent_id`      | `u32` little-endian                     | 4 B   |
| `spatial_ref`   | three `i16` little-endian (x, y, z)    | 6 B   |
| `time_bucket`   | `u64` monotonic ms / 2^14 (≈16 s grain) | 8 B   |

Total input: 21 bytes. Intensity and coherence (Q15) are **not**
hashed — two signs differing only in magnitude are the same sign for
identity purposes; their magnitudes update the existing graph node.

The 16-second time bucket coarsens timestamps so a sign recurring at
the same place within one bucket hashes to the same id (deduplication
at the graph layer); crossing a bucket boundary produces a new id.

### Why not the other candidates

- **ahash / `std::collections::hash_map::DefaultHasher`** — both
  pull `std`. `umwelt-core` is `no_std` (ADR-0008).
- **fxhash** — would cost a new workspace dep for no measurable
  benefit over a 5-line FNV. rustc uses it because rustc hashes a
  lot of strings; we hash 21-byte records, so cache-miss cost
  dominates either way.
- **Truncated BLAKE3-64** — we already depend on BLAKE3 for SEP
  MAC (ADR-0012), so this would reuse a primitive. Rejected
  because:
  - BLAKE3 adds ~50 KB to `umwelt-core` vs. 5 lines for FNV.
  - BLAKE3 is a cryptographic primitive. `SignId` is not a
    security boundary — the security boundary lives at the SEP
    MAC (ADR-0012). Using a crypto hash inside a hot local graph
    op is over-engineering.
  - `umwelt-core` must stay zero-dep at the hash layer so it
    builds on microcontrollers without the crypto toolchain.

### Collision policy

At 64-bit output, birthday collision expectation is ≈ 2^32
insertions (~4 × 10⁹). Agent graphs are bounded to at most ~10⁶
signs per deployment (ADR-0008 memory envelope), so the collision
probability is ≈ 10⁻⁸. Nonetheless:

- **Detect.** `UmweltGraph::insert(sign)` compares the stored
  node's identity-field bytes against the new sign. Hash match +
  identity mismatch ⇒ collision.
- **Promote.** The colliding insertion is rehashed with the
  16-bit "salt" suffix appended to the identity bytes, starting at
  `salt = 1` and incrementing until no collision. The resulting
  `SignId` has no visible marker — callers see a fresh id.
- **Log once.** `umwelt_core::hash::COLLISION_COUNT` is an
  `AtomicUsize` exposed to callers. Under `feature = "std"` the
  crate also emits a single `tracing::warn!` per process when the
  counter first increments.
- **Proptest invariants** (land with P1.2):
  1. Distinct identity bytes → distinct `SignId` (or
     `COLLISION_COUNT > 0`).
  2. Identical identity bytes → identical `SignId`.
  3. Forced collision via a test-only hash stub → second insert
     gets a `SignId` that does not alias the first's node.

### Scope boundary

This ADR pins the *hash function* and *collision policy*. The
*allocator* (who holds the id → node map, how it's backed, how
lookup scales) is the subject of P1.2 + P1.3; those steps must
honour this ADR but are free to choose data structures.

### Consequences

- `umwelt-core` stays dep-free at the hash layer. `blake3` remains
  exclusive to `umwelt-sep` (SEP MAC, ADR-0012).
- The test helper at `crates/umwelt-core/src/lib.rs:439–450` is
  promoted to a public module in P1.2; the test stays but switches
  to importing the canonical function.
- `docs/performance.md` gains (in P1.2's commit) a row: *SignId
  hash per insert ≤ 200 ns on host, criterion bench in
  `crates/umwelt-core/benches/hash.rs`*.
- If collision probability ever needs to drop below 10⁻⁸ (e.g. a
  fleet-wide id space unified across agents), this ADR is revisited
  in favour of BLAKE3-64. Not expected before P4.

## ADR-0015 — Sign decay profiles: three-class exponential, Q15 per-tick

**Date:** 2026-04-18
**Status:** Accepted (binding at P3.1). Unblocks P0.7.3.

### Decision

Sign intensity decays **exponentially** once per 10 Hz sensor tick
(`Δt = 100 ms`, ADR-0013). Every sign belongs to exactly one of
three named **decay classes**, each defined by a
**time-to-5%-retention** constant `τ`. The per-tick Q15 multiplier
is derived from τ at compile time and multiplied in by the graph
maintenance step (P3.4).

### The three classes

| Class          | τ (5% retention) | Rationale (from `umwelt.html`)              |
|----------------|------------------|---------------------------------------------|
| `OBSTACLE`     | **350 ms**       | Transient geometric hazards; must clear within a few ticks once evidence stops. |
| `CONTAMINATION`| **14 s**         | Chemical / biological signs persist; map-level features. |
| `TERRAIN`      | **70 s**         | Composition and surface type; slow-changing environmental context. |

These are the three values given in the published spec.
ADR-0015 pins them as "spec", not "target", from now on.

### Decay model

For intensity `I_n` at tick `n`:

```
  I_{n+1} = Q15_saturating_mul(I_n, ALPHA_class)
```

where `ALPHA_class = (0.05)^(Δt / τ)` evaluated at compile time,
stored as `Q15`. This definition means after exactly `τ` of wall
time, intensity is `0.05 × I_0` regardless of tick alignment (a
deliberately chosen, reproducible design point).

### Pinned constants (Δt = 100 ms, `Q15::MAX = 32767`)

| Class           | τ       | α (real)       | ALPHA (Q15 raw) | Retained after τ |
|-----------------|---------|----------------|-----------------|------------------|
| `OBSTACLE`      | 350 ms  | 0.424 778      | **13 920**      | 5.00 %           |
| `CONTAMINATION` | 14 000 ms | 0.978 830    | **32 073**      | 5.00 %           |
| `TERRAIN`       | 70 000 ms | 0.995 729    | **32 627**      | 5.00 %           |

Derivation check (for review):

```
  α            = 0.05^(0.1 / τ_seconds)
  OBSTACLE     = 0.05^(0.2857) = 0.4248
  CONTAMINATION= 0.05^(0.00714) = 0.9788
  TERRAIN      = 0.05^(0.00143) = 0.9957
  Q15_raw      = round(α × 32767)
```

### Class assignment

Decay class is a property of the **`sign_type`**, carried on the
vocabulary record. `umwelt-vocab`'s YAML schema (P0.5.11) gains a
mandatory `decay_class: OBSTACLE | CONTAMINATION | TERRAIN` field
on each sign type entry when P3.4 lands; that schema bump is
P0.7.3's only code consequence and comes in its own commit.

No per-sign override in v0.1. If a deployment needs a fourth
class (e.g. `WEATHER` at 5 min), that's a v0.2 ADR. The three
classes are sufficient for the two reference vocabularies
(`terrain.yaml`, `swarm.yaml`) because both already split their
sign types along obstacle / contamination / terrain lines.

### Interaction with ADR-0013 (signification trigger)

The entropy trigger has `COOLDOWN = 4` ticks (400 ms) between
re-fires on the same channel. Worked examples:

- **OBSTACLE:** after cooldown (4 ticks), retention is
  `0.4248^4 ≈ 3.3 %` — already below the 5% floor. Cooldown
  naturally dominates decay for obstacle-class signs; no
  re-fire-on-decaying-sign pathology.
- **CONTAMINATION:** after cooldown, retention is
  `0.9788^4 ≈ 91.7 %`. A fresh contamination percept during
  cooldown is blocked by the entropy trigger (by design), but
  the *existing* sign's intensity is essentially unchanged.
  Operator intuition: cooldown bounds sampling rate; decay
  bounds memory lifetime; they don't interfere.
- **TERRAIN:** after cooldown, retention is `0.9957^4 ≈ 98.3 %`.
  Same behaviour — terrain signs persist across cooldowns, which
  is exactly what "terrain composition decay 70 s" promises.

### Scope — what this ADR does not do

- **Does not implement decay.** Implementation is P3.4 (graph
  maintenance). This ADR pins the spec so P3.1 (icon layer) can
  be written against a known contract.
- **Does not define the decay trigger.** Decay runs at the
  maintenance tick, not per insert. Per-tick sweep vs. lazy
  evaluation is a P3.4 implementation choice, not an ADR choice.
- **Does not pin a floor / cutoff.** When `I_n × ALPHA` drops
  below `Q15::from_raw(1)` the result is `Q15::ZERO` by
  saturating arithmetic (ADR-0004); P3.4 decides whether to
  prune the node at that moment or on the next compaction pass.

### Consequences

- `docs/performance.md` D-rows flip from `target` to `spec`;
  the published spec states the three numbers as pinned
  design constants, not approximations.
- `umwelt-core` gains a `decay::{DecayClass, alpha_q15}` module
  in P3.1 with the three pinned constants as `const Q15`.
- `umwelt-vocab`'s YAML schema gains a `decay_class` field on
  each sign-type entry in P3.4. Until then, vocabularies are
  unchanged and the field defaults to `OBSTACLE` at load time
  with a warning.
- If real deployments demand a `WEATHER`-like slow class, extend
  the enum in a new ADR; do not overload `TERRAIN`.
- Reverse-check: the Q15 retained-after-τ numbers above must
  equal 5.00 % to within ±0.02 % — proptest lands with P3.4.

## ADR-0016 — Sign reinforcement on dedup: saturating-add intensity

**Date:** 2026-04-18
**Status:** Accepted (binding at P1.4).

### Decision

When `UmweltGraph::insert` is called with a [`SignIdentity`] whose
21-byte canonical encoding already maps to a stored node (i.e. an
ADR-0014 dedup hit — hash match *and* identity match), the graph
**reinforces** the stored sign in place rather than allocating a new
one. The reinforcement function is pure, total, and deterministic:

| field          | rule                                                                |
|----------------|---------------------------------------------------------------------|
| `id`           | preserved (= stored.id)                                             |
| `layer`        | preserved (identity-keyed)                                          |
| `domain`       | preserved (identity-keyed)                                          |
| `channel`      | preserved (stored wins on mismatch — see below)                     |
| `formed_at`    | **preserved** (sign "birth time" is a stable attribute)             |
| `duration_us`  | `max(stored.duration_us, incoming.duration_us)`                     |
| `intensity`    | `stored.intensity.saturating_add(incoming.intensity)`               |
| `coherence`    | **preserved** (owned by semiogenesis, P3 — not touched on insert)   |

### Why saturating-add

- **Monotone in reinsertion count.** Each recurrence raises the
  stored intensity toward the cap; with ADR-0015 decay running at
  10 Hz between inserts the result is a biologically plausible
  build-up / die-off curve — rising while evidence arrives, decaying
  once it stops.
- **Capped at `Q15::MAX`.** Q15's saturating arithmetic (ADR-0004)
  prevents runaway while preserving sign: an inhibitory icon
  (negative intensity) keeps accumulating toward `Q15::MIN`.
- **Stateless.** No per-node EWMA coefficient, no hidden tick
  counter — easy to reason about in proptest invariants and easy
  to replay offline.
- **Alternatives considered and rejected:**
  - *Max.* Loses sensitivity to recurrence frequency; a single loud
    hit is indistinguishable from a dozen quiet ones.
  - *EWMA with α < 1.* Forces a new constant into every sign-type
    ADR and introduces a coefficient that would need another
    calibration rig. Deferred to a later ADR if ever needed.
  - *Count-based.* Adds an `observation_count` field to `Sign`,
    doubling the storage footprint on microcontrollers; rejected
    on ADR-0008 memory-envelope grounds.

### Why `formed_at` is preserved

`formed_at` is the sign node's birth time. Overwriting it on
recurrence would erase the agent's most useful temporal fact: *when
did this sign first appear?* Icon-layer indexing (P3.2) and replay
(P3.5) both key off first-seen times. The freshest observation time
is a separate concern and, if it becomes necessary, earns a new
field (`last_seen_at`) and a new ADR — it does not displace
`formed_at`.

### Why `channel` is preserved on mismatch

`channel` is *not* part of [`SignIdentity`] (ADR-0014 table). Two
inserts with the same identity but different `channel` values are a
vocabulary bug, not a merge conflict — the agent should not have
two sensors claiming the same semiotic location. The graph keeps
the first-seen channel; debugging this case is a `umwelt-vocab`
lint, not a runtime resolution.

### Why `coherence` is preserved

Coherence is **set by the semiogenesis engine** (P3) as a function
of cross-channel correlation in a window around the sign, not by
the sensor-driver call that inserts an icon. Letting `insert`
clobber it would undo the engine's work. If P3 wants to reinforce
coherence on recurrence, it will do so via a dedicated graph
maintenance call, not through `insert`.

### Invariants (proptest, lands with P1.4)

Let `ident` be any `SignIdentity`, `s0` be the sign inserted first
at `intensity = q0`, and `si` the subsequent inserts with
`intensity = qi`.

1. **Idempotence of id.** After any N inserts under `ident`, the
   graph has `len == 1` and every insert returns the same id.
2. **Monotonicity (signed).** If every `qi > 0`, stored intensity
   is non-decreasing in N. If every `qi < 0`, stored intensity is
   non-increasing in N. (Sign-preserving monotonicity.)
3. **Saturation.** Inserting `Q15::MAX` `N ≥ 2` times yields
   stored intensity `= Q15::MAX`.
4. **`formed_at` stability.** Stored `formed_at` equals `s0.formed_at`
   regardless of later inserts.
5. **`duration_us` high-water-mark.** Stored `duration_us` equals
   `max(s0.duration_us, s1.duration_us, …)`.

### Consequences

- `umwelt-core::graph` gains a private `merge_reinforcement(stored,
  incoming) -> Sign` free function that both backends share; the
  HeaplessGraph and VecGraph `insert` paths apply it on dedup hit.
- No new public API surface — callers still see `insert(identity,
  sign) -> Result<SignId, InsertError>`. The *behaviour* changes;
  the *signature* does not.
- P3.2 (index layer) may read stored intensity directly and trust
  that it reflects observed evidence, not just the latest frame.
- Out of scope: decay ordering. P3.4 runs the decay tick *between*
  inserts; if `insert` and decay interleave, the semantic is still
  well-defined because each operation is pure Q15 arithmetic.

## ADR-0017 — Index layer: pairwise temporal correlation

**Date:** 2026-04-18
**Status:** Accepted (binding at P3.2). Implementation follows in the
next commit.

### Decision

The Index layer (Peirce's second-ness) materialises when two
**distinct-channel** Icon events fire within a short sliding window.
We ship the simplest possible correlator for v0.1:

1. A per-correlator table of the most-recent icon per channel.
2. On each incoming icon, enumerate all **other** channels whose most-
   recent icon lies inside the window, emit one `IndexEvent` per
   qualifying pair, then update the current channel's slot.
3. `IndexEvent` carries a **canonical** pair `[min(c_a, c_b),
   max(c_a, c_b)]`, the absolute delta (`now - earlier_t`), and a
   Q15 **coherence** computed as the **geometric mean** of the two
   icons' magnitudes.

Higher-order correlations (triples, N-way), direction-of-arrival
encoding, and entropy-weighted coherence are explicitly deferred.

### Parameters (pinned)

| Name                 | Value        | Notes                                                           |
| -------------------- | ------------ | --------------------------------------------------------------- |
| `DEFAULT_WINDOW_MS`  | `500`        | 5 ticks at the 10 Hz reference (ADR-0015). Tunable at runtime.  |
| `MAX_WINDOW_MS`      | `3_000`      | Upper bound accepted by `with_window_ms`; values above clamp.   |
| Channel capacity `C` | const-generic| Fixed per-correlator. Typical v0.1 agents run `C <= 16`.        |
| Pair canonical order | sort asc.    | `channels[0] < channels[1]` is an invariant on every emission.  |
| Self-pair policy     | never emit   | Same-channel icons update state but produce no `IndexEvent`.    |

### State shape

```rust
pub struct IndexCorrelator<const C: usize> {
    slots: [Option<LastIcon>; C],      // one per channel index
    channels: [u16; C],                // channel-id at each slot
    occupied: usize,                   // count of Some slots
    window_us: u64,                    // DEFAULT_WINDOW_MS * 1000
}

struct LastIcon { formed_at: TimestampUs, magnitude: Q15 }
```

Channel ids are assigned slot indices on first appearance; the
`channels` map gives `u16 → 0..C`. Full map returns a sentinel verdict
on `observe` (`ObserveError::ChannelFull`) instead of silently
dropping events — mirrors the ReplayGuard `UnknownPeer` pattern
(ADR-0011 family).

### Co-occurrence window: 500 ms

- **Why 500 ms?** 5 ticks at the ADR-0015 reference rate. Long enough
  to survive scheduling jitter and single-hop mesh delivery (SEP
  frame ~= ~50 ms end-to-end typical per ADR-0012's RF budget),
  short enough that two bursts from independent phenomena don't
  accidentally correlate (a one-second gap is an ocean at 10 Hz).
- **Why configurable?** Terrain (ADR-0015, τ=70 s) warrants a larger
  window than Obstacle (τ=350 ms); callers tune per-fleet.
- **Why clamp at 3 s?** Beyond 3 s the correlation signal is lost in
  the decay curve of every ADR-0015 class; there is no useful
  Index event past that horizon. The clamp is also wrap-safe:
  `window_us = 3_000_000` is orders below `u64::MAX`.

### Pair enumeration: flat O(C) per icon

- **Flat array, not heap.** `[Option<LastIcon>; C]` is const-sized.
  For `C <= 16`, a scan is a single cache line and entirely
  deterministic — matches ADR-0008's no_std/no-alloc envelope.
- **O(C) per observe.** On each icon, walk the other `C - 1` slots
  once; emit `IndexEvent` for each slot still inside the window.
  At C=16 and 10 Hz that is 150 compares/s on the worst path; the
  ReplayGuard high-water check (ADR-0011 family) is a comparable
  cost per frame.
- **Rejected: event-driven pair list.** Maintaining a heap of
  `(timestamp, channel_pair)` entries would scale better for
  large C but pulls in heap or a second ring buffer. Revisit if
  C exceeds 64 in any real deployment.
- **Rejected: bitmap of recent channels.** One bit per channel is
  cheaper but loses the per-pair delta and magnitude context the
  coherence scalar needs.

### Coherence: geometric mean of magnitudes

For two icons with magnitudes `m_a, m_b` in `|Q15|`:

```
coherence = Q15::from_raw( sqrt_u32(m_a.raw()_u32 * m_b.raw()_u32) )
```

- **Q15-closed.** `m_a.raw(), m_b.raw() ∈ [0, 32767]` → product
  `≤ 32767² ≈ 1.07 × 10⁹` fits `u32` → `sqrt` fits `i16` exactly.
- **Asymmetry-penalising.** `geomean(m, 0) = 0`; a loud-quiet pair
  is not the same as two loud ones. The arithmetic mean would
  collapse that distinction.
- **Magnitude-only for v0.1.** Entropy is *not* mixed into the
  scalar. Two icons agreeing that *something novel* happened in the
  same place at the same time is the index-layer signal; the shape
  of that novelty (rising vs falling entropy, structured vs uniform)
  is symbol-layer (P3.3) territory.
- **`sqrt_u32` implementation.** Integer Newton iteration, `const fn`
  friendly. Lives in `umwelt-core::icon` as a private helper reused
  by `umwelt-core::index` — zero new deps, no libm, termination in
  `<= 32` iterations for any `u32` input.
- **Rejected alternatives:**
  - *Min.* Pessimistic; drops pairs where one icon is merely strong
    and the other overwhelming.
  - *Arithmetic mean.* See above — masks asymmetry.
  - *Product.* Not scale-invariant; two `0.5` magnitudes would
    produce `0.25` and the semantics of "half-coherent" becomes
    murky.
  - *Weighted by entropy.* Adds a free parameter; deferred until a
    real calibration need surfaces.

### Emission mode: callback

```rust
pub fn observe<F>(&mut self, icon: &IconEvent, on_pair: F)
    -> Result<(), ObserveError>
where F: FnMut(IndexEvent);
```

- **Why callback?** Caller-owned state, zero allocation, no bounded-
  buffer API debate. The common call-site pattern feeds emissions
  straight into `UmweltGraph::insert` — one `f` closure does both.
- **Rejected: `&mut heapless::Vec<IndexEvent, N>`.** Forces callers
  to pick an `N`; the correlator can emit up to `C - 1` events in
  one call, which bakes knowledge of `C` into every call-site.
- **Rejected: `impl Iterator<Item = IndexEvent>`.** Self-borrowed
  iterators over `&mut self` force `for<'a>` wizardry for no
  meaningful win over a callback on stable.

### IndexEvent shape

```rust
pub struct IndexEvent {
    pub channels: [u16; 2],   // sorted ascending; [0] < [1]
    pub delta_us: u64,        // |t_new - t_old|
    pub formed_at: TimestampUs,  // the new icon's timestamp
    pub coherence: Q15,       // geomean of magnitudes, [0, Q15::MAX]
}
```

`formed_at` is the incoming icon's timestamp, not the earlier one,
so downstream consumers see events land in monotonic order.

### Implementation contract (P3.2)

- `umwelt-core::index` exposes `IndexCorrelator<const C: usize>`,
  `IndexEvent`, `ObserveError`, and re-exports nothing else.
- All state caller-owned; no globals.
- `sqrt_u32` lives in `umwelt-core::icon` (private module helper) and
  is `pub(crate)` for reuse by `index`.
- `observe` is O(C) time and allocates nothing.
- Proptests (land with P3.2):
  - Same-channel icons never emit.
  - Two distinct-channel icons within window always emit exactly
    one event; the `channels` array is sorted.
  - Two icons outside the window never emit.
  - Emission count per `observe` ≤ `occupied - 1`.
  - `coherence` is monotone in both magnitudes
    (`m_a ≤ m_a' ∧ m_b = m_b' → coh ≤ coh'`).
  - `geomean(Q15::ZERO, _) == Q15::ZERO`.

### Consequences

- `umwelt-core` gains a second module (`index`) alongside `icon`.
- The Symbol layer (P3.3) will consume `IndexEvent` streams — this
  ADR locks that consumer's wire shape, so P3.3 can be implemented
  without revisiting the pair encoding.
- `UmweltGraph::insert` is the natural sink for emitted events; the
  callback form makes the wiring a one-liner.
- Out of scope: higher-order correlations, direction encoding,
  entropy weighting. Each is a future ADR when a concrete need
  arrives.

## ADR-0018 — Symbol layer: recurrence-driven commitment

**Date:** 2026-04-18
**Status:** Accepted (binding at P3.3). Implementation follows in the
next commit.

### Decision

The Symbol layer (Peirce's third-ness) materialises when a canonical
pair from the Index layer recurs above a **count threshold** within a
**recurrence window**. We ship the simplest possible recogniser for
v0.1:

1. A `SymbolRecogniser<const C: usize>` holds a channel→slot map
   (same pattern as `IndexCorrelator`) and a `[[u8; C]; C]` upper-
   triangle count matrix indexed by `(min_slot, max_slot)`.
2. On each incoming `IndexEvent`, map both channel IDs to slot
   indices, increment `counts[min_slot][max_slot]`, and record the
   timestamp.
3. When a pair's count reaches `COMMITMENT_THRESHOLD`, emit one
   `SymbolEvent` via callback and reset the count to zero.
4. If the oldest observation for a pair is older than
   `RECURRENCE_WINDOW_MS` from the current event, reset the count
   before incrementing — stale accumulations do not carry forward.

Higher-order recurrence (triples, pattern sequences), entropy-weighted
commitment, and vocabulary-driven naming are explicitly deferred.

### Parameters (pinned)

| Name                     | Value          | Notes                                                           |
| ------------------------ | -------------- | --------------------------------------------------------------- |
| `COMMITMENT_THRESHOLD`   | `3`            | Three co-occurrences to commit. Tunable via `with_threshold`.   |
| `RECURRENCE_WINDOW_MS`   | `10_000`       | 10 s. Pairs older than this reset. Tunable via constructor.     |
| `MAX_RECURRENCE_WINDOW_MS` | `60_000`     | Upper clamp on the recurrence window.                           |
| Channel capacity `C`     | const-generic  | Same as `IndexCorrelator`; typical v0.1 agents: `C <= 16`.     |
| Self-pair policy         | impossible     | `IndexEvent.channels` is always distinct; no guard needed.      |

### Why count-threshold (not EWMA)

- **Discrete question.** "Has this pair recurred N times?" is a
  counting problem, not a rate-estimation problem. A count answers
  it directly; an EWMA adds a free parameter (α) with no calibration
  target.
- **Testable.** "Feed 3 IndexEvents for pair [2,5] → one SymbolEvent"
  is a deterministic proptest. EWMA rate-crossing depends on timing
  and decay, making tests fragile.
- **Minimal state.** One `u8` per pair plus one timestamp for window
  enforcement — 129 bytes for C=16 (120 counts + 9 bytes overhead).
  EWMA would use one `Q15` (2 bytes) per pair and still need a
  timestamp for window gating.
- **Reset-on-commit.** After emitting a symbol, the counter resets
  to zero, allowing re-commitment if the pair keeps recurring. This
  feeds naturally into ADR-0016's saturating-add reinforcement when
  the symbol is re-inserted into the graph.

### State shape

```rust
pub struct SymbolRecogniser<const C: usize> {
    channels: [u16; C],         // channel-id at each slot
    occupied: usize,            // count of registered channels
    counts: [[u8; C]; C],       // upper-triangle pair counts
    first_seen: [[u64; C]; C],  // timestamp of first event in current window
    window_us: u64,             // RECURRENCE_WINDOW_MS * 1000
    threshold: u8,              // COMMITMENT_THRESHOLD
}
```

Channel IDs are assigned slot indices on first appearance, mirroring
`IndexCorrelator`'s channel→slot map. Capacity exhaustion returns
`ObserveError::ChannelFull`.

### SymbolEvent shape

```rust
pub struct SymbolEvent {
    pub channels: [u16; 2],       // the pair that committed (sorted asc)
    pub formed_at: TimestampUs,   // triggering IndexEvent's timestamp
    pub coherence: Q15,           // triggering IndexEvent's coherence
}
```

`formed_at` and `coherence` are copied from the triggering
`IndexEvent`, providing the downstream graph-insert path with
everything it needs to construct a `Sign` of layer `Symbol`.

### Emission mode: callback

```rust
pub fn observe<F>(&mut self, idx: &IndexEvent, on_commit: F)
    -> Result<(), ObserveError>
where F: FnMut(SymbolEvent);
```

Same pattern as `IndexCorrelator::observe`. One event per commitment;
at most one per `observe` call (a pair can only reach threshold once
per reset cycle).

### Symbol-id assignment: content-addressed via ADR-0014

Symbols use `SignId::from_identity` with:

- `layer = SemioticLayer::Symbol`
- `domain = SignDomain::Coherence` (cross-channel correlation)
- `sign_type = 0` (v0.1 has no vocabulary-driven type assignment)
- `spatial_ref = [ch_lo as i16, ch_hi as i16, 0]` — encodes the
  canonical pair into the spatial-ref triple so that distinct pairs
  produce distinct `SignIdentity` values. Semantic stretch
  acknowledged; the "implementation-defined units" clause in ADR-0014
  permits it. A future ADR may add a dedicated `pair_ref` field.
- `agent_id`, `time_bucket` — from caller context, same as Icon/Index.

Re-commitment of the same pair in the same time bucket deduplicates
via `UmweltGraph::insert` + ADR-0016 saturating-add reinforcement.
Different time buckets produce distinct nodes — the graph remembers
*when* each symbol was active.

### Vocabulary integration: deferred

For v0.1, `sign_type = 0` for all recurrence-committed symbols.
`umwelt-vocab::SignVocabulary` does not gain a reverse-index. When
vocabulary-driven naming is needed (mapping pair-class → human-
readable symbol name), a future ADR will define the lookup path and
the `sign_type` encoding. The SymbolEvent shape is forward-compatible
— adding a `sign_type` field later is non-breaking.

### Implementation contract (P3.3)

- `umwelt-core::symbol` exposes `SymbolRecogniser<const C: usize>`,
  `SymbolEvent`, and re-uses `index::ObserveError` (or defines its
  own `SymbolObserveError` if the variant set differs).
- All state caller-owned; no globals.
- `observe` is O(C) time (channel lookup) and allocates nothing.
- Proptests (land with P3.3):
  - Below-threshold never emits.
  - Exactly at threshold emits once.
  - Count resets after commitment (N+threshold more → second emission).
  - Stale-window reset: events spread beyond the window do not
    accumulate toward threshold.
  - `SymbolEvent.channels` matches the `IndexEvent.channels`.
  - ChannelFull when capacity exceeded.
  - Coherence and formed_at are passed through from IndexEvent.

### Consequences

- `umwelt-core` gains a third semiotic module (`symbol`) alongside
  `icon` and `index`.
- The P3.4 decay layer (exponential decay on cold signs) operates on
  symbols in the graph via the same `Sign.intensity` field that
  ADR-0016 reinforces — the count-threshold commitment feeds
  naturally into build-up / die-off.
- The P3.5 offline replay harness can drive the full Icon→Index→Symbol
  pipeline from sensor logs once P3.3 lands.
- Out of scope: vocabulary-driven naming, multi-pair symbol classes,
  entropy-weighted commitment. Each is a future ADR.

## ADR-0019 — Simulated-LoRa transport: loss, jitter, partition

**Date:** 2026-04-19
**Status:** Accepted (binding at P4.3). Implementation follows in the
next commit.

### Decision

P4.3 ships a generic, test-only wrapper `SimLora<T: SepTransport, const Q>`
that composes over any `SepTransport` (UDP, loopback, future ports) and
injects three independent fault modes into the `send` / `recv` path:

1. **Bernoulli packet loss** — each frame passed to `send` is dropped
   silently with probability `loss_ppm / 1_000_000`. Drops are counted.
2. **Latency + jitter** — each frame that survives the loss roll is
   stamped with a delivery time `now_us + base_latency_us + u(0, jitter_us)`
   and held in an in-wrapper delivery queue. `recv` drains the underlying
   transport into the queue, then returns the oldest frame whose delivery
   time has elapsed.
3. **Partition** — a boolean toggle. While `true`, `send` drops all
   frames (counted separately from loss) and `recv` discards any frames
   pulled from the underlying transport. Frames already in the delivery
   queue when partition activates are retained; when partition clears,
   normal `recv` resumes.

The three fault modes are **independent and composable**. Loss fires
first (cheap, terminal), then jitter (queued), then partition gates
both ingress and egress.

### Parameters (pinned)

| Name               | Value / range           | Notes                                                                                 |
| ------------------ | ----------------------- | ------------------------------------------------------------------------------------- |
| `loss_ppm`         | `u32`, 0..=1_000_000    | Parts-per-million loss probability. `0` = lossless, `1_000_000` = always drop.         |
| `base_latency_us`  | `u64`, 0..=60_000_000   | Fixed delay applied to every surviving frame. Upper clamp = 60 s.                     |
| `jitter_us`        | `u64`, 0..=60_000_000   | Random uniform delay added on top of base. `0` = no jitter.                           |
| `partitioned`      | `bool`                  | Mutable at runtime. Tests flip this to model link loss and recovery.                  |
| Queue capacity `Q` | const-generic           | Per-wrapper delivery-queue slots. Typical v0.1 test configs: `Q = 32`. Overflow drops. |

Three presets are exposed as constructors on `SimLora`:

| Preset        | `loss_ppm` | `base_latency_us` | `jitter_us` |
| ------------- | ---------- | ----------------- | ----------- |
| `clean()`     | 0          | 0                 | 0           |
| `lossy()`     | 100_000    | 50_000            | 50_000      |
| `fringe()`    | 400_000    | 200_000           | 400_000     |

Presets are calibration targets for the P4.4 swarm test, not
production knobs. Real deployments will tune per-agent from telemetry.

### Time source: injected, not wall-clock

`SimLora` takes its current time via an explicit `now_us: u64` argument
to every `send` and `recv` call, **not** via `std::time::Instant::now`.
Rationale:

- **Deterministic tests.** Jitter that depends on a system clock makes
  tests flaky under load. An injected clock (a counter incremented by
  the test) is flaky-free.
- **Replay-friendly.** Offline replay (P3.5) feeds a monotonic log-time
  clock; `SimLora` composes into that harness unchanged.
- **Free-running tests still work.** A helper `SystemClock` or
  closure-based clock can be added later without changing the wrapper's
  shape.

### Loss / partition drop accounting

`SimLora` exposes three `u32` saturating counters, mirroring
`LoopbackMesh`:

- `dropped_loss()` — lost to Bernoulli roll
- `dropped_partition()` — dropped because link was partitioned on send,
  or discarded on recv during partition
- `dropped_queue_full()` — delivery queue had no free slot for a
  jittered frame

Counters are non-decreasing. Used by P4.4 swarm tests to prove that the
fabric is lossy-but-correct: the system keeps working and the counts
match what the test injected.

### Out-of-order delivery is a feature, not a bug

Because each frame's `delivery_at_us` is drawn independently, a later
send with a short random jitter can leapfrog an earlier send with a
long jitter. The delivery queue is a plain `heapless::Vec<Slot, Q>`
scanned on each `recv` for the oldest ready frame. This is realistic
for LoRa (collisions, duty-cycle back-off, retransmits) and is covered
by an explicit unit test.

### RNG: deterministic xorshift64

Seeded at construction; advanced on each send (one u32 for the loss
roll, one u32 for the jitter draw). No external dependency; stays
`#![deny(unsafe_code)]`-clean.

### Wrapper shape (sketch)

```rust
pub struct SimLora<T: SepTransport, const Q: usize> {
    inner: T,
    loss_ppm: u32,
    base_latency_us: u64,
    jitter_us: u64,
    partitioned: bool,
    rng_state: u64,
    queue: heapless::Vec<Slot, Q>,
    dropped_loss: u32,
    dropped_partition: u32,
    dropped_queue_full: u32,
}

struct Slot {
    deliver_at_us: u64,
    len: u8,
    buf: [u8; SEP_ENVELOPE_BYTES],
}

impl<T: SepTransport, const Q: usize> SimLora<T, Q> {
    pub fn new(inner: T, now_us: u64) -> Self { ... }
    pub fn clean(inner: T) -> Self { ... }
    pub fn lossy(inner: T, now_us: u64) -> Self { ... }
    pub fn fringe(inner: T, now_us: u64) -> Self { ... }

    pub fn set_loss_ppm(&mut self, ppm: u32);
    pub fn set_latency(&mut self, base_us: u64, jitter_us: u64);
    pub fn set_partitioned(&mut self, partitioned: bool);

    pub fn send_at(&mut self, frame: &[u8], now_us: u64) -> Result<(), SepTransportError>;
    pub fn recv_at(&mut self, out: &mut [u8], now_us: u64) -> Result<usize, SepTransportError>;

    pub fn dropped_loss(&self) -> u32;
    pub fn dropped_partition(&self) -> u32;
    pub fn dropped_queue_full(&self) -> u32;
}
```

### SepTransport impl: optional and wall-clock

For convenience, `SimLora` also implements `SepTransport` by reading
`std::time::Instant` on `send` / `recv`. This is feature-gated under
`std` (already required by `udp` and `loopback`) and is a thin wrapper
around `send_at` / `recv_at`. Tests prefer the `_at` variants for
deterministic control; production use (via the swarm simulator in P4.4)
prefers the trait impl for composability.

### Out of scope

- **Duty-cycle modelling.** Real LoRa has 1% duty-cycle limits on most
  ISM bands; we don't enforce those here. Add via a future ADR if a
  deployment surfaces the need.
- **Per-peer loss/jitter.** This v0.1 wrapper applies uniform fault
  parameters to all traffic. Per-peer variation layered in P4.4 via
  the mesh, not the transport.
- **SNR-driven loss.** Loss is flat Bernoulli, not a function of link
  quality. A future ADR can swap the model if telemetry shows the flat
  model misleads.

### Consequences

- `crates/umwelt-transport/src/sim_lora.rs` lands in P4.3, gated behind
  a new `sim-lora` feature (enabled by default alongside `udp`).
- P4.4 swarm test composes `SimLora` over `LoopbackMesh` endpoints,
  not over `UdpTransport` — keeping the swarm test hermetic.
- P5.4 (`umwelt simulate`) exposes loss/latency/partition knobs on the
  CLI by passing through to `SimLora` setters.

## ADR-0020 — CLI arg parser and pipeline profile dispatch

**Date:** 2026-04-19
**Status:** Accepted

### Context

P5.1 introduces `umwelt run`, the first subcommand on the CLI binary.
Two questions needed resolution before code:

1. **Arg parser.** No crate in the workspace handles CLI argument
   parsing today. The binary needs subcommand dispatch (`run`, `inspect`,
   `replay`, `simulate`) and per-subcommand flags.

2. **Pipeline const-generic dispatch.** `SemioticPipeline<C, W, B>` is
   parameterised on const generics that must be known at compile time.
   YAML config supplies these values at runtime. The CLI must bridge the
   gap.

### Decision

**Arg parser — `clap` (derive).** `clap` is the ecosystem-standard
argument parser for Rust CLIs. It supports derive-based subcommand
declaration, built-in `--help` / `--version`, shell completions, and
has a stable 4.x API. The binary size increase (~200 KB) is acceptable
for a host-only CLI; `umwelt-core` and `umwelt-sep` are not affected.
Added to `[workspace.dependencies]` as `clap = { version = "4",
features = ["derive"] }`.

**Pipeline profiles — pinned monomorphisation set.** The CLI matches
the `(channels, icon_window, icon_bins)` tuple from config against a
compile-time table of known profiles and dispatches to the
corresponding `SemioticPipeline<C, W, B>` monomorphisation. For v0.1
two profiles are supported:

| Name   | C  | W   | B  | Notes                    |
|--------|----|-----|----|--------------------------|
| Small  |  4 |  64 |  8 | ADR-0013 default         |
| Medium |  8 | 128 | 16 | double-capacity variant  |

Unsupported combinations return a clear error listing the valid
profiles. This avoids trait-object dispatch, macro complexity, or
open-ended monomorphisation, and extends trivially — add a row to the
match statement when a new profile is needed.

### Consequences

- `clap` added to `[workspace.dependencies]` and to `umwelt-cli`
  `[dependencies]`.
- `umwelt run --config <path>` is the primary subcommand entry point.
- `SimLoraOverLoopback` transport is not supported under `umwelt run`
  (it requires injected time); users should use `umwelt simulate`
  (P5.4) instead. The `run` subcommand errors with a clear message.
- Future profiles are added by extending the match table; no ADR
  required unless the dispatch mechanism itself changes.


## ADR-0021 — `serde_json` for operator-surface snapshots

**Date:** 2026-04-19
**Status:** Accepted

### Context

P5.2 (`umwelt inspect`) must emit a JSON snapshot of an Umwelt graph.
P5.3 (`umwelt replay`) must ingest NDJSON sensor logs. P6.2 will
stream per-sign NDJSON events to disk. None of these formats is
optional; all three sit at the operator surface and are read by humans
and by tools outside the Rust workspace. YAML is already available
via `serde_yaml_ng`, but the operator-surface deliverable is JSON.

### Decision

Add `serde_json = "1"` to `[workspace.dependencies]`. Use it only in
host-only crates — never in `umwelt-core` or `umwelt-sep`, which stay
`no_std` and ship no snapshot format. Enable the `serde` feature on
`umwelt-core` in crates that need to serialize domain types (today:
`umwelt-cli`).

### Consequences

- `serde_json` becomes a host-only workspace dep alongside
  `serde_yaml_ng`. Both run through the same `serde` derive path on
  shared types.
- `umwelt-cli` enables `umwelt-core/serde` to serialize `Sign`, `SignId`,
  `Q15`, `SemioticLayer`, and `SignDomain`.
- P5.2's JSON snapshot schema is a flat array of `Sign` objects plus a
  metadata header (agent id, mission timestamps, vocabulary name).
  The schema is versioned via a `"schema": "umwelt.snapshot.v1"` tag
  so future format changes don't silently corrupt older snapshots.
- P5.3 and P6.2 will reuse the same dep — no second ADR required for
  either step.

## ADR-0022 — P8.3 Embassy / embedded-hal compatibility: audit-only, adapters live in port crates

**Date:** 2026-04-20
**Status:** Accepted

### Context

P8.3 asks whether the public surface of `umwelt-core` (the
`SemioticPipeline` tick API, sensor-frame types) and `umwelt-sep` (the
`SepTransport` trait) is structurally compatible with the mainstream
embedded Rust ecosystem: `embedded-hal 1.0`, `embedded-io 0.6` (and
its `embedded-io-async` sibling), the Embassy executor and time
crates, and the `lora-phy` radio abstraction.

Two questions hide inside this: (a) can a downstream integrator
embed the runtime on an STM32 / nRF / ESP target without patching
our crates? (b) if adapter code is required, should it live in this
repo or in a downstream port crate?

The P8 phase plan already answers (b) negatively for
hardware-specific code: "Hardware-specific board-support crates live
in a separate repo once the core is stable." P8.3 is explicitly
paper-only ("P8 — Hardware profile (paper-only)"). The remaining
open question is whether the current trait shape needs a revision
*in this repo* before adapters become writable downstream.

### Decision

The P8.3 deliverable is a compatibility audit document
(`docs/embedded_hal_compat.md`), not adapter code. We land:

1. A compatibility matrix row-per-ecosystem-crate with a one-word
   verdict (compatible / compatible-via-framer / compatible-via-async-wrap /
   incompatible) and the adapter pattern for each row.
2. A risk list covering embedded-hal 2.0 drift, async-only futures,
   multi-radio ownership, and no_std-stable MSRV interactions.
3. Adapter sketches as documented pseudocode, not as compiled code
   in the workspace.

We do **not**:

- Add `embedded-hal`, `embedded-io`, `embassy-*`, or `lora-phy` to
  `[workspace.dependencies]`. Doing so would grow the dependency
  surface of crates that cannot use them and would bind us to a
  particular ecosystem version before any downstream port exists.
- Land an `embedded-io` adapter module in `umwelt-transport`. The
  first port crate (`umwelt-esp32` or similar) can decide its own
  framer, backoff policy, and feature-flag shape; upstream
  pre-commitment would constrain that decision pointlessly.
- Introduce a `SepTransportAsync` trait. Our blocking + `WouldBlock`
  shape composes cleanly with Embassy via a ~10-line `async fn`
  wrapper, and no surveyed driver today requires an async-only
  trait.

### Consequences

- No new workspace dependencies. Zero build-surface change in v0.1.
- `SepTransport`'s shape is validated as ecosystem-ready: blocking,
  `&mut self`, frame-oriented, Copy errors. This matches the
  `lora-phy` tx/rx surface directly and wraps cleanly around
  `embedded-io` byte streams with a length-prefix + CRC framer.
- Embassy integration pattern is pinned in the audit doc:
  `#[embassy_executor::task]` polling `recv()` with
  `Timer::after_millis(1)` on `WouldBlock`.
- If `embedded-hal 2.0` lands with breaking trait splits, adapters
  rebuild against new signatures; this repo's traits are insulated.
- A future phase (post-v0.1 or during the first port) may introduce
  `SepTransportAsync` if a radio driver publishes async-only APIs
  where `block_on` becomes unacceptable under Embassy. That decision
  belongs with a concrete driver, not speculated here.
- The audit doc becomes the source of truth for "is the core
  embeddable?"; it is cited from the next-steps block when P9
  (release) asks about hardware-readiness claims in the published spec.

### Alternatives considered

1. **Land an `embedded-io` adapter in `umwelt-transport`** under
   `#[cfg(feature = "embedded-io")]`. Rejected: forces a transitive
   dep on a host-only crate's API surface, and the first port crate
   will want a different framer / backoff policy regardless.
2. **Introduce `SepTransportAsync` speculatively.** Rejected:
   `async fn` in traits is stable as of Rust 1.75 and our MSRV
   (1.85) comfortably supports it, but no surveyed driver requires
   it. Adding it now would churn the trait universe before any user
   of the async variant exists.
3. **Fork `SepTransport` into blocking + embedded-hal variants.**
   Rejected: splits the trait universe for zero operational benefit
   and doubles the test surface.
4. **Publish an `umwelt-embedded` companion crate from this repo.**
   Rejected: duplicates what `umwelt-esp32` / `umwelt-nrf` / etc. will
   do in their own workspaces; only a compatibility audit belongs
   here.

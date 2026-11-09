//! P4.4 — multi-agent swarm integration test.
//!
//! Six agents share one [`LoopbackMesh`]. Each agent's endpoint is
//! wrapped in a [`SimLora`] under the `lossy` preset (10% loss,
//! 50 ms base + 50 ms jitter latency) so cross-agent delivery passes
//! through both the mesh fabric and ADR-0019's fault injector.
//!
//! Every agent runs a full [`SemioticPipeline`] driven by a
//! deterministic synthetic signal — the pipeline is exercised
//! end-to-end, but the coverage metric is not gated on pipeline
//! emissions. Instead each agent broadcasts a deterministic *herald*
//! symbol-frame on a staggered schedule, guaranteeing the (sender,
//! receiver) delivery matrix can be asserted without flakiness.
//!
//! Coverage assertion: by the end of the 30 s mission (and a 1 s
//! drain tail for in-flight jittered frames), every pair
//! `(sent, recv)` with `sent != recv` must have delivered at least
//! one frame.
//!
//! The test is hermetic: no UDP, no wall-clock. Every `send_at` /
//! `recv_at` takes an injected µs timestamp, and the RNG is
//! deterministic. Re-running the test produces identical results.
//!
//! ## Why a hand-rolled `MeshEndpoint` adapter
//!
//! [`LoopbackMesh`] intentionally exposes `send(from, to, ...)` /
//! `recv(agent, ...)` as plain methods rather than a per-endpoint
//! `SepTransport` impl (see `mesh.rs` module docs). P4.4 needs the
//! SimLora wrapper to see each agent through a `SepTransport`
//! interface, so this integration test defines a tiny
//! `MeshEndpoint` adapter that closes over a shared
//! `Rc<RefCell<LoopbackMesh>>` and forwards `send` as a broadcast
//! (LoRa-like radio semantics) and `recv` as a per-agent inbox drain.
//! The `Rc<RefCell<_>>` pattern is acceptable here because this file
//! is `std`-only and test-scoped.

#![cfg(all(feature = "loopback", feature = "sim-lora"))]

use std::cell::RefCell;
use std::rc::Rc;

use umwelt_core::replay::{PipelineEvent, SemioticPipeline};
use umwelt_core::Q15;
use umwelt_sep::{
    decode, encode, FrameForm, SepFrame, SepTransport, SepTransportError, SEP_FRAME_BYTES,
};
use umwelt_transport::mesh::LoopbackMesh;
use umwelt_transport::sim_lora::SimLora;

// ── Mission parameters ────────────────────────────────────────────────────
//
// These are test-local, not architectural — no ADR-0020 is needed
// (same pattern as the P3.5 replay tests). Calibrated to give the
// lossy preset plenty of redundancy for 100% pair coverage while
// staying under 1 s of wall-clock runtime.

/// Agents on the mesh.
const AGENTS: usize = 6;

/// Per-agent mesh inbox capacity (frames).
const MESH_QUEUE: usize = 64;

/// Per-agent SimLora delivery-queue capacity (frames).
const SIM_QUEUE: usize = 64;

/// Pipeline channel capacity.
const CHANNELS: usize = 4;

/// Icon-ring window length (samples).
const ICON_WINDOW: usize = 64;

/// Icon-ring histogram bin count.
const ICON_BINS: usize = 8;

/// Mission length in ticks.
const TICKS: usize = 300;

/// Tick interval in µs (100 ms ⇒ 10 Hz).
const TICK_US: u64 = 100_000;

/// One heartbeat per agent every `HEARTBEAT_EVERY` ticks, staggered
/// by agent id. Agent `i` broadcasts at ticks `i, i+30, i+60, …`.
/// Over 300 ticks that's 10 broadcasts per agent; at 10% loss the
/// per-pair miss probability is `0.1^10 ≈ 1e-10`.
const HEARTBEAT_EVERY: usize = 30;

// ── Mesh endpoint adapter ─────────────────────────────────────────────────

/// A [`SepTransport`] view onto one slot of a shared [`LoopbackMesh`].
///
/// `send` broadcasts (one-to-many, LoRa-radio semantics); `recv`
/// drains this agent's inbox.
struct MeshEndpoint {
    mesh: Rc<RefCell<LoopbackMesh<AGENTS, MESH_QUEUE>>>,
    my_id: usize,
}

impl SepTransport for MeshEndpoint {
    fn form(&self) -> FrameForm {
        self.mesh.borrow().form()
    }

    fn send(&mut self, frame: &[u8]) -> Result<(), SepTransportError> {
        self.mesh.borrow_mut().broadcast(self.my_id, frame)
    }

    fn recv(&mut self, buf: &mut [u8]) -> Result<usize, SepTransportError> {
        self.mesh.borrow_mut().recv(self.my_id, buf)
    }
}

// ── Agent stack ───────────────────────────────────────────────────────────

/// Per-agent state: fault-injected transport + full semiogenesis
/// pipeline. Split into a named struct so callers can destructure
/// into disjoint mutable borrows (`&mut sim` + `&mut pipe`) on the
/// hot path inside the per-tick loop.
struct AgentStack {
    sim: SimLora<MeshEndpoint, SIM_QUEUE>,
    pipe: SemioticPipeline<CHANNELS, ICON_WINDOW, ICON_BINS>,
}

// ── Helpers ───────────────────────────────────────────────────────────────

/// Deterministic two-channel alternating signal.
///
/// Designed to pump each channel's icon ring past warm-up with high
/// entropy; per-agent phase offset keeps the index-layer correlator
/// fed across the 500 ms window.
fn signal_for(agent: usize, tick: usize, channel: u16) -> Q15 {
    let phase = (agent + channel as usize + tick) % 2;
    if phase == 0 {
        Q15::from_raw(20_000)
    } else {
        Q15::from_raw(-20_000)
    }
}

/// Encode a herald SepFrame for agent `src` at timestamp `ts_ms`.
///
/// `sign_type = 1` is our test-local sentinel for "symbol heartbeat";
/// only the agent_id field is asserted against on the receive side.
fn herald_frame(src: usize, ts_ms: u32) -> [u8; SEP_FRAME_BYTES] {
    let f = SepFrame {
        agent_id: src as u32,
        sign_type: 1,
        magnitude: 200,
        ttl: 4,
        flags: 0,
        spatial: [0, 0, 0],
        timestamp_ms: ts_ms,
        confidence: 200,
        priority: 128,
    };
    let mut buf = [0u8; SEP_FRAME_BYTES];
    encode(&f, &mut buf);
    buf
}

/// Drain every ready frame from `sim` into `observed`, recording
/// every `(sent -> recv)` delivery. `recv` is the agent index that
/// owns this `sim`.
fn drain_ready(
    sim: &mut SimLora<MeshEndpoint, SIM_QUEUE>,
    now_us: u64,
    recv: usize,
    observed: &mut [[bool; AGENTS]; AGENTS],
) {
    let mut buf = [0u8; SEP_FRAME_BYTES];
    loop {
        match sim.recv_at(&mut buf, now_us) {
            Ok(n) => {
                assert_eq!(n, SEP_FRAME_BYTES);
                if let Ok(frame) = decode(&buf) {
                    let src = frame.agent_id as usize;
                    if src < AGENTS && src != recv {
                        observed[recv][src] = true;
                    }
                }
            }
            Err(SepTransportError::WouldBlock) => break,
            Err(e) => panic!("unexpected recv error at agent {recv}: {e:?}"),
        }
    }
}

// ── The mission ───────────────────────────────────────────────────────────

#[test]
fn six_agent_swarm_30s_full_coverage() {
    let mesh = Rc::new(RefCell::new(LoopbackMesh::<AGENTS, MESH_QUEUE>::new_inner()));

    // Build one stack per agent with per-agent distinct RNG seed so
    // each SimLora's loss-roll and jitter draws are independent.
    let mut stacks: Vec<AgentStack> = (0..AGENTS)
        .map(|i| {
            let endpoint = MeshEndpoint {
                mesh: mesh.clone(),
                my_id: i,
            };
            let sim = SimLora::lossy(endpoint, 0xABCD_0000 ^ (i as u64).wrapping_mul(0x9E37));
            let pipe = SemioticPipeline::<CHANNELS, ICON_WINDOW, ICON_BINS>::new();
            AgentStack { sim, pipe }
        })
        .collect();

    // Delivery matrix: observed[recv][sent] = true once `recv` has
    // received at least one frame originating from `sent`.
    let mut observed = [[false; AGENTS]; AGENTS];

    // Informational — count pipeline-driven symbol emissions per agent
    // so a zero result is visible when debugging without failing the
    // test (exact emission count is timing-fragile).
    let mut pipeline_symbols = [0u32; AGENTS];

    for tick in 0..TICKS {
        let now_us = tick as u64 * TICK_US;

        // ── Send phase: drive pipelines and issue heartbeats ────
        for i in 0..AGENTS {
            let AgentStack { sim, pipe } = &mut stacks[i];

            // Two channels is enough to exercise the index layer's
            // pairwise correlator. Pipeline symbol emissions are
            // opportunistic broadcast via `sim.send_at`.
            for ch in 0..2u16 {
                let sample = signal_for(i, tick, ch);
                let ts_ms = (now_us / 1_000) as u32;
                pipe.observe(ch, sample, now_us, |evt| {
                    if let PipelineEvent::Symbol(_) = evt {
                        pipeline_symbols[i] = pipeline_symbols[i].saturating_add(1);
                        let frame = herald_frame(i, ts_ms);
                        let _ = sim.send_at(&frame, now_us);
                    }
                })
                .expect("pipeline observe failed");
            }

            // Deterministic heartbeat: agent i fires at tick i, i+30,
            // i+60, … Guarantees every pair has plenty of chances
            // regardless of whether the pipeline ever emits a Symbol.
            if tick >= i && (tick - i).is_multiple_of(HEARTBEAT_EVERY) {
                let ts_ms = (now_us / 1_000) as u32;
                let frame = herald_frame(i, ts_ms);
                let _ = sim.send_at(&frame, now_us);
            }
        }

        // ── Receive phase: drain every agent's SimLora queue ────
        for (i, stack) in stacks.iter_mut().enumerate() {
            drain_ready(&mut stack.sim, now_us, i, &mut observed);
        }
    }

    // ── Post-mission drain ──────────────────────────────────────
    //
    // The final heartbeat batch goes out on ticks 270..=275 and is
    // stamped with deliver-at up to `275·TICK_US + 100 000 = 27 600 000
    // + 100 000 = 27.6 s` — comfortably within the mission. But
    // jitter-scheduled frames from the last few ticks may still be
    // sitting in per-agent queues waiting for their deliver_at to
    // elapse. Pump one final `recv_at` at (mission_end + 1 s) to
    // drain them before the coverage assertion.
    let drain_until = TICKS as u64 * TICK_US + 1_000_000;
    for (i, stack) in stacks.iter_mut().enumerate() {
        drain_ready(&mut stack.sim, drain_until, i, &mut observed);
    }

    // ── Coverage assertion ──────────────────────────────────────
    for (recv, row) in observed.iter().enumerate() {
        for (sent, seen) in row.iter().enumerate() {
            if recv == sent {
                continue;
            }
            assert!(
                *seen,
                "coverage gap: agent {recv} never observed any frame from agent {sent}",
            );
        }
    }

    // ── Sanity on drop counters ─────────────────────────────────
    //
    // The mesh itself never partitioned, so `dropped_partition()`
    // must be zero across the board. `dropped_loss()` is bounded by
    // the total send count (< TICKS per agent). `dropped_queue_full`
    // must be zero — the 64-slot SIM_QUEUE has ample headroom because
    // we drain every tick.
    for (i, stack) in stacks.iter().enumerate() {
        let sim = &stack.sim;
        assert_eq!(
            sim.dropped_partition(),
            0,
            "agent {i} reported partition drops in a non-partitioning test",
        );
        assert!(
            sim.dropped_loss() <= TICKS as u32,
            "agent {i} lost more frames than total ticks: {}",
            sim.dropped_loss(),
        );
        assert_eq!(
            sim.dropped_queue_full(),
            0,
            "agent {i} overflowed SimLora queue — increase SIM_QUEUE or drain more often",
        );
    }

    // ── Accessory signal: pipeline drove without erroring ──────
    //
    // We don't assert a specific symbol-emission count — that'd be
    // brittle under the pipeline's exact warmup curve. A zero here
    // is acceptable: the swarm coverage assertion above already
    // proves the transport+SimLora stack is sound. This binding
    // exists so the informational counter isn't dead-code-eliminated.
    let _pipeline_symbols = pipeline_symbols;
}

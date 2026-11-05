//! Simulated-LoRa fault injector (P4.3).
//!
//! A test-only wrapper that composes over any [`SepTransport`] and
//! injects three independent fault modes into the `send` / `recv` path,
//! per ADR-0019:
//!
//! 1. **Bernoulli packet loss** — each frame passed to `send_at` is
//!    dropped silently with probability `loss_ppm / 1_000_000`. Drops
//!    are counted in [`SimLora::dropped_loss`].
//! 2. **Latency + jitter** — each frame that arrives on the underlying
//!    transport is stamped with a delivery time `now_us +
//!    base_latency_us + u(0, jitter_us)` and held in an in-wrapper
//!    delivery queue. [`SimLora::recv_at`] returns the oldest frame
//!    whose delivery time has elapsed.
//! 3. **Partition** — a boolean toggle. While `true`, `send_at` drops
//!    every frame (counted separately from loss), and any frame pulled
//!    from the underlying transport by `recv_at` is silently discarded
//!    (also counted as partition-drop). Frames already in the delivery
//!    queue when partition activates are retained; when partition
//!    clears, normal delivery resumes.
//!
//! ## Where the queue lives
//!
//! ADR-0019's "each frame that survives the loss roll is stamped"
//! reads naturally as a send-side queue; ADR-0019's "`recv` drains the
//! underlying transport into the queue" reads as a receive-side queue.
//! We implement the receive-side model because it requires no implicit
//! pump and produces identical observable behaviour in swarm tests
//! (loss is absorbed on send, jitter is observable as delivery delay
//! from the caller's perspective). The `Slot::deliver_at_us` carries
//! the same semantics either way — when this slot becomes visible to
//! `recv_at`.
//!
//! ## Why an injected clock
//!
//! Every `send_at` / `recv_at` takes an explicit `now_us: u64`. Jitter
//! that depends on `Instant::now()` is flaky under CI load; an injected
//! clock is flaky-free and replay-friendly. A `std`-only convenience
//! [`SepTransport`] impl (feature-gated in the same crate) is provided
//! for callers that prefer wall-clock composition.
//!
//! ## Host-only
//!
//! This wrapper is `std` (uses `heapless::Vec` for the queue but pulls
//! no OS syscalls). Embedded targets use a separate adapter crate.

use heapless::Vec as HVec;
use umwelt_sep::{FrameForm, SepTransport, SepTransportError, SEP_ENVELOPE_BYTES};

// ── Parameter clamps (ADR-0019 pinned values) ─────────────────────────────

/// Maximum `loss_ppm` value (100% loss).
pub const MAX_LOSS_PPM: u32 = 1_000_000;

/// Maximum base latency accepted by setters (60 s).
pub const MAX_BASE_LATENCY_US: u64 = 60_000_000;

/// Maximum jitter accepted by setters (60 s).
pub const MAX_JITTER_US: u64 = 60_000_000;

// ── Preset tables (ADR-0019) ──────────────────────────────────────────────

const PRESET_CLEAN: (u32, u64, u64) = (0, 0, 0);
const PRESET_LOSSY: (u32, u64, u64) = (100_000, 50_000, 50_000);
const PRESET_FRINGE: (u32, u64, u64) = (400_000, 200_000, 400_000);

/// One queued frame awaiting delivery at or after `deliver_at_us`.
///
/// Storage is fixed at `SEP_ENVELOPE_BYTES` so the same `Slot` carries
/// either an inner or an envelope frame; `len` records the true length.
#[derive(Clone, Copy)]
struct Slot {
    deliver_at_us: u64,
    len: u8,
    buf: [u8; SEP_ENVELOPE_BYTES],
}

/// Simulated-LoRa wrapper. Generic over any [`SepTransport`] inner; `Q`
/// is the delivery-queue capacity in slots.
///
/// Frame form is inherited from the inner transport and reported
/// unchanged via [`SimLora::form`].
pub struct SimLora<T: SepTransport, const Q: usize> {
    inner: T,
    loss_ppm: u32,
    base_latency_us: u64,
    jitter_us: u64,
    partitioned: bool,
    rng_state: u64,
    queue: HVec<Slot, Q>,
    dropped_loss: u32,
    dropped_partition: u32,
    dropped_queue_full: u32,
}

impl<T: SepTransport, const Q: usize> SimLora<T, Q> {
    /// Construct a fault-free wrapper with the given RNG seed.
    ///
    /// Use [`SimLora::set_loss_ppm`], [`SimLora::set_latency`], and
    /// [`SimLora::set_partitioned`] to dial in fault modes.
    pub fn new(inner: T, seed: u64) -> Self {
        Self {
            inner,
            loss_ppm: 0,
            base_latency_us: 0,
            jitter_us: 0,
            partitioned: false,
            // xorshift64 is undefined for seed=0; fold in a constant so
            // callers can safely pass 0.
            rng_state: seed ^ 0x9E37_79B9_7F4A_7C15,
            queue: HVec::new(),
            dropped_loss: 0,
            dropped_partition: 0,
            dropped_queue_full: 0,
        }
    }

    /// Preset: lossless, zero-latency, zero-jitter. Useful as a regression
    /// baseline when wrapping a transport with `SimLora` for
    /// composability but wanting no fault injection.
    pub fn clean(inner: T) -> Self {
        let mut s = Self::new(inner, 0);
        let (l, b, j) = PRESET_CLEAN;
        s.loss_ppm = l;
        s.base_latency_us = b;
        s.jitter_us = j;
        s
    }

    /// Preset: 10% loss, 50 ms base latency, 50 ms uniform jitter.
    pub fn lossy(inner: T, seed: u64) -> Self {
        let mut s = Self::new(inner, seed);
        let (l, b, j) = PRESET_LOSSY;
        s.loss_ppm = l;
        s.base_latency_us = b;
        s.jitter_us = j;
        s
    }

    /// Preset: 40% loss, 200 ms base latency, 400 ms uniform jitter.
    pub fn fringe(inner: T, seed: u64) -> Self {
        let mut s = Self::new(inner, seed);
        let (l, b, j) = PRESET_FRINGE;
        s.loss_ppm = l;
        s.base_latency_us = b;
        s.jitter_us = j;
        s
    }

    // ── Mutators ──────────────────────────────────────────────────────────

    /// Set the Bernoulli loss rate in parts-per-million. Clamped to
    /// [`MAX_LOSS_PPM`].
    #[inline]
    pub fn set_loss_ppm(&mut self, ppm: u32) {
        self.loss_ppm = ppm.min(MAX_LOSS_PPM);
    }

    /// Set the latency parameters. Both are clamped to their respective
    /// maxima ([`MAX_BASE_LATENCY_US`], [`MAX_JITTER_US`]).
    #[inline]
    pub fn set_latency(&mut self, base_us: u64, jitter_us: u64) {
        self.base_latency_us = base_us.min(MAX_BASE_LATENCY_US);
        self.jitter_us = jitter_us.min(MAX_JITTER_US);
    }

    /// Toggle the partition flag. Frames already queued for delivery
    /// survive the transition; ingress and egress after the transition
    /// are suppressed.
    #[inline]
    pub fn set_partitioned(&mut self, partitioned: bool) {
        self.partitioned = partitioned;
    }

    // ── Accessors ─────────────────────────────────────────────────────────

    /// Frame form reported by the inner transport.
    #[inline]
    pub fn form(&self) -> FrameForm {
        self.inner.form()
    }

    /// Count of frames dropped by the Bernoulli loss roll. Non-decreasing.
    #[inline]
    pub fn dropped_loss(&self) -> u32 {
        self.dropped_loss
    }

    /// Count of frames dropped because the link was partitioned at the
    /// time the frame was being sent or received. Non-decreasing.
    #[inline]
    pub fn dropped_partition(&self) -> u32 {
        self.dropped_partition
    }

    /// Count of frames that could not be enqueued for delivery because
    /// the queue was full. Non-decreasing.
    #[inline]
    pub fn dropped_queue_full(&self) -> u32 {
        self.dropped_queue_full
    }

    /// Current number of frames queued for delivery. Exposed for test
    /// assertions.
    #[inline]
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Immutable access to the wrapped transport. Useful for tests that
    /// need to inspect the inner state.
    #[inline]
    pub fn inner(&self) -> &T {
        &self.inner
    }

    // ── Deterministic send/recv with injected clock ───────────────────────

    /// Send one frame at logical time `now_us`.
    ///
    /// The frame must match the inner transport's [`FrameForm`]; other
    /// lengths return [`SepTransportError::InvalidFrameLen`] without
    /// advancing the drop counters.
    ///
    /// All other fault outcomes (partition, loss) return `Ok(())` — from
    /// the caller's perspective, a successful send into a link that
    /// later lost the frame is indistinguishable from a send that
    /// arrived. This is the realistic LoRa contract: the radio accepted
    /// the frame; whether it lands is a separate question answered on
    /// the receiver.
    pub fn send_at(&mut self, frame: &[u8], now_us: u64) -> Result<(), SepTransportError> {
        if frame.len() != self.inner.form().bytes() {
            return Err(SepTransportError::InvalidFrameLen);
        }
        if self.partitioned {
            self.dropped_partition = self.dropped_partition.saturating_add(1);
            return Ok(());
        }
        // Advance RNG once for the loss roll.
        let roll = self.next_u32();
        if self.loss_roll_hits(roll) {
            self.dropped_loss = self.dropped_loss.saturating_add(1);
            return Ok(());
        }
        // Survived: hand the frame to the inner transport. Latency and
        // jitter are applied on the receive side by the peer's SimLora,
        // or — if this wrapper is reading back its own traffic (as in
        // self-addressed UDP tests) — on our own recv_at.
        let _ = now_us; // present to stabilise the API surface
        self.inner.send(frame)?;
        Ok(())
    }

    /// Receive one frame at logical time `now_us`.
    ///
    /// Two-phase: first drain any frames the inner transport has ready
    /// (stamping each with its delivery time `now_us + base + jitter`),
    /// then return the oldest queue slot whose `deliver_at_us <=
    /// now_us`.
    ///
    /// Because drain and pop use the same `now_us`, a frame just pulled
    /// from the inner transport with non-zero jitter is never delivered
    /// in the same call that pulled it — callers must call `recv_at` a
    /// second time at a later `now_us` (or split drain/pop via
    /// [`SimLora::drain_at`] and [`SimLora::pop_at`]).
    ///
    /// Returns [`SepTransportError::BufferTooSmall`] if `out` is shorter
    /// than the inner form. Returns [`SepTransportError::WouldBlock`]
    /// if no slot is ready for delivery (either the queue is empty or
    /// every slot's `deliver_at_us` is in the future).
    pub fn recv_at(&mut self, out: &mut [u8], now_us: u64) -> Result<usize, SepTransportError> {
        if out.len() < self.inner.form().bytes() {
            return Err(SepTransportError::BufferTooSmall);
        }
        self.drain_at(now_us);
        self.pop_at(out, now_us)
    }

    /// Drain the inner transport, stamping each recovered frame with
    /// delivery time `arrival_us + base_latency_us + u(0, jitter_us)`.
    ///
    /// Discards frames if the link is currently partitioned (counted in
    /// [`SimLora::dropped_partition`]). Drops frames if the delivery
    /// queue is full (counted in [`SimLora::dropped_queue_full`]).
    ///
    /// Tests that want to control delivery timing precisely typically
    /// call `drain_at(t_arrival)` once and `pop_at(t_delivery)` later.
    pub fn drain_at(&mut self, arrival_us: u64) {
        let mut scratch = [0u8; SEP_ENVELOPE_BYTES];
        let form_bytes = self.inner.form().bytes();
        loop {
            match self.inner.recv(&mut scratch[..form_bytes]) {
                Ok(n) => {
                    if self.partitioned {
                        self.dropped_partition = self.dropped_partition.saturating_add(1);
                        continue;
                    }
                    let jitter = self.draw_jitter();
                    let deliver_at_us = arrival_us
                        .saturating_add(self.base_latency_us)
                        .saturating_add(jitter);
                    let mut slot = Slot {
                        deliver_at_us,
                        len: n as u8,
                        buf: [0u8; SEP_ENVELOPE_BYTES],
                    };
                    slot.buf[..n].copy_from_slice(&scratch[..n]);
                    if self.queue.push(slot).is_err() {
                        self.dropped_queue_full = self.dropped_queue_full.saturating_add(1);
                    }
                }
                Err(SepTransportError::WouldBlock) => break,
                Err(_) => break,
            }
        }
    }

    /// Pop the oldest queue slot whose `deliver_at_us <= now_us`. Does
    /// not drain the inner transport — callers typically pair this with
    /// [`SimLora::drain_at`] for deterministic control.
    pub fn pop_at(&mut self, out: &mut [u8], now_us: u64) -> Result<usize, SepTransportError> {
        if out.len() < self.inner.form().bytes() {
            return Err(SepTransportError::BufferTooSmall);
        }
        let mut oldest: Option<(usize, u64)> = None;
        for (i, slot) in self.queue.iter().enumerate() {
            if slot.deliver_at_us <= now_us {
                match oldest {
                    None => oldest = Some((i, slot.deliver_at_us)),
                    Some((_, t)) if slot.deliver_at_us < t => {
                        oldest = Some((i, slot.deliver_at_us));
                    }
                    _ => {}
                }
            }
        }
        let idx = oldest.ok_or(SepTransportError::WouldBlock)?.0;
        let slot = self.queue.swap_remove(idx);
        let n = slot.len as usize;
        out[..n].copy_from_slice(&slot.buf[..n]);
        Ok(n)
    }

    // ── Internals ─────────────────────────────────────────────────────────

    /// xorshift64 — advances state, returns the low 32 bits.
    fn next_u32(&mut self) -> u32 {
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_state = x;
        x as u32
    }

    /// `true` if the 32-bit roll falls inside the loss region. Scaled
    /// to parts-per-million without floating-point.
    fn loss_roll_hits(&self, roll: u32) -> bool {
        if self.loss_ppm == 0 {
            return false;
        }
        if self.loss_ppm >= MAX_LOSS_PPM {
            return true;
        }
        // roll in [0, 2^32). Threshold = (loss_ppm / 1_000_000) * 2^32.
        // Compute as u64 to avoid overflow.
        let threshold = ((self.loss_ppm as u64) << 32) / (MAX_LOSS_PPM as u64);
        (roll as u64) < threshold
    }

    /// Draw a uniform delay in `[0, jitter_us)` — unbiased-enough for
    /// a test fixture; we don't need cryptographic rejection sampling.
    fn draw_jitter(&mut self) -> u64 {
        if self.jitter_us == 0 {
            return 0;
        }
        let r = self.next_u32() as u64;
        // Simple modulo. Bias is at most 1 part in 2^32 / jitter_us;
        // for jitter_us = 60_000_000 that's about 1.4%, which is well
        // within the realism budget for a test fixture.
        r % self.jitter_us
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use umwelt_sep::SEP_FRAME_BYTES;

    // ── Minimal in-memory test transport ─────────────────────────────────
    //
    // A one-way queue that `SimLora` can wrap. Simpler than the full
    // LoopbackTransport and avoids feature coupling.

    struct FifoTransport {
        queue: std::collections::VecDeque<[u8; SEP_FRAME_BYTES]>,
        form: FrameForm,
    }

    impl FifoTransport {
        fn new() -> Self {
            Self {
                queue: std::collections::VecDeque::new(),
                form: FrameForm::Inner,
            }
        }
    }

    impl SepTransport for FifoTransport {
        fn form(&self) -> FrameForm {
            self.form
        }

        fn send(&mut self, frame: &[u8]) -> Result<(), SepTransportError> {
            if frame.len() != self.form.bytes() {
                return Err(SepTransportError::InvalidFrameLen);
            }
            let mut buf = [0u8; SEP_FRAME_BYTES];
            buf.copy_from_slice(frame);
            self.queue.push_back(buf);
            Ok(())
        }

        fn recv(&mut self, out: &mut [u8]) -> Result<usize, SepTransportError> {
            if out.len() < self.form.bytes() {
                return Err(SepTransportError::BufferTooSmall);
            }
            let buf = self
                .queue
                .pop_front()
                .ok_or(SepTransportError::WouldBlock)?;
            out[..SEP_FRAME_BYTES].copy_from_slice(&buf);
            Ok(SEP_FRAME_BYTES)
        }
    }

    fn sample(byte: u8) -> [u8; SEP_FRAME_BYTES] {
        [byte; SEP_FRAME_BYTES]
    }

    // ── Round-trip / form ─────────────────────────────────────────────────

    #[test]
    fn clean_preset_is_lossless_and_instant() {
        let mut t: SimLora<FifoTransport, 8> = SimLora::clean(FifoTransport::new());
        let frame = sample(0x11);
        t.send_at(&frame, 0).unwrap();
        let mut out = [0u8; SEP_FRAME_BYTES];
        let n = t.recv_at(&mut out, 0).unwrap();
        assert_eq!(n, SEP_FRAME_BYTES);
        assert_eq!(out, frame);
        assert_eq!(t.dropped_loss(), 0);
        assert_eq!(t.dropped_partition(), 0);
        assert_eq!(t.dropped_queue_full(), 0);
    }

    #[test]
    fn form_is_inherited_from_inner() {
        let t: SimLora<FifoTransport, 4> = SimLora::clean(FifoTransport::new());
        assert_eq!(t.form(), FrameForm::Inner);
    }

    #[test]
    fn wrong_length_returns_invalid_frame_len() {
        let mut t: SimLora<FifoTransport, 4> = SimLora::new(FifoTransport::new(), 1);
        let short = [0u8; 10];
        let err = t.send_at(&short, 0);
        assert_eq!(err, Err(SepTransportError::InvalidFrameLen));
    }

    #[test]
    fn recv_small_buffer_returns_buffer_too_small() {
        let mut t: SimLora<FifoTransport, 4> = SimLora::new(FifoTransport::new(), 1);
        let mut small = [0u8; 10];
        let err = t.recv_at(&mut small, 0);
        assert_eq!(err, Err(SepTransportError::BufferTooSmall));
    }

    // ── Loss ──────────────────────────────────────────────────────────────

    #[test]
    fn total_loss_drops_every_frame() {
        let mut t: SimLora<FifoTransport, 4> = SimLora::new(FifoTransport::new(), 1);
        t.set_loss_ppm(MAX_LOSS_PPM);
        let frame = sample(0xAA);
        for _ in 0..10 {
            t.send_at(&frame, 0).unwrap();
        }
        let mut out = [0u8; SEP_FRAME_BYTES];
        assert_eq!(t.recv_at(&mut out, 0), Err(SepTransportError::WouldBlock));
        assert_eq!(t.dropped_loss(), 10);
        assert_eq!(t.dropped_partition(), 0);
    }

    #[test]
    fn zero_loss_never_drops() {
        let mut t: SimLora<FifoTransport, 8> = SimLora::new(FifoTransport::new(), 1);
        let frame = sample(0xAA);
        for _ in 0..5 {
            t.send_at(&frame, 0).unwrap();
        }
        assert_eq!(t.dropped_loss(), 0);
    }

    #[test]
    fn partial_loss_is_bernoulli() {
        // At 50% loss over 1000 trials, expected loss count is ~500
        // with stddev ~16. Assert it's within 4σ = [436, 564] — a
        // non-flaky bound that still catches a broken roll.
        let mut t: SimLora<FifoTransport, 1> = SimLora::new(FifoTransport::new(), 0xDEAD_BEEF);
        t.set_loss_ppm(MAX_LOSS_PPM / 2);
        let frame = sample(0x55);
        for _ in 0..1000 {
            t.send_at(&frame, 0).unwrap();
            // Drain the inner so we don't exhaust the FIFO with
            // successful frames that never get read.
            let mut out = [0u8; SEP_FRAME_BYTES];
            let _ = t.recv_at(&mut out, 0);
        }
        let lost = t.dropped_loss();
        assert!(
            (436..=564).contains(&lost),
            "bernoulli loss count outside 4σ window: {lost}",
        );
    }

    // ── Partition ─────────────────────────────────────────────────────────

    #[test]
    fn partition_drops_all_sends() {
        let mut t: SimLora<FifoTransport, 4> = SimLora::new(FifoTransport::new(), 1);
        t.set_partitioned(true);
        let frame = sample(0xBB);
        for _ in 0..3 {
            t.send_at(&frame, 0).unwrap();
        }
        assert_eq!(t.dropped_partition(), 3);
        assert_eq!(t.dropped_loss(), 0);
        let mut out = [0u8; SEP_FRAME_BYTES];
        assert_eq!(t.recv_at(&mut out, 0), Err(SepTransportError::WouldBlock));
    }

    #[test]
    fn partition_recovery_resumes_traffic() {
        let mut t: SimLora<FifoTransport, 4> = SimLora::new(FifoTransport::new(), 1);
        let frame = sample(0xCC);
        t.set_partitioned(true);
        t.send_at(&frame, 0).unwrap();
        assert_eq!(t.dropped_partition(), 1);
        t.set_partitioned(false);
        t.send_at(&frame, 0).unwrap();
        let mut out = [0u8; SEP_FRAME_BYTES];
        let n = t.recv_at(&mut out, 0).unwrap();
        assert_eq!(n, SEP_FRAME_BYTES);
        assert_eq!(out, frame);
        assert_eq!(t.dropped_partition(), 1);
    }

    #[test]
    fn partition_on_recv_discards_incoming() {
        let mut t: SimLora<FifoTransport, 4> = SimLora::new(FifoTransport::new(), 1);
        let frame = sample(0xDD);
        t.send_at(&frame, 0).unwrap(); // lands in inner
        t.set_partitioned(true);
        let mut out = [0u8; SEP_FRAME_BYTES];
        // drain_inner sees the frame but discards under partition.
        assert_eq!(t.recv_at(&mut out, 0), Err(SepTransportError::WouldBlock));
        assert_eq!(t.dropped_partition(), 1);
    }

    // ── Jitter / latency ──────────────────────────────────────────────────

    #[test]
    fn base_latency_delays_delivery() {
        let mut t: SimLora<FifoTransport, 4> = SimLora::new(FifoTransport::new(), 42);
        t.set_latency(500, 0);
        let frame = sample(0xEE);
        t.send_at(&frame, 0).unwrap();
        let mut out = [0u8; SEP_FRAME_BYTES];
        // First recv at t=0: drain inner (stamps deliver_at = 0 + 500 +
        // 0 = 500), try to pop at t=0 — nothing ready yet.
        assert_eq!(t.recv_at(&mut out, 0), Err(SepTransportError::WouldBlock));
        // Before latency elapses: still not ready.
        assert_eq!(t.recv_at(&mut out, 499), Err(SepTransportError::WouldBlock),);
        // At latency: ready.
        let n = t.recv_at(&mut out, 500).unwrap();
        assert_eq!(n, SEP_FRAME_BYTES);
        assert_eq!(out, frame);
    }

    #[test]
    fn jitter_can_reorder_delivery() {
        // Two frames sent back-to-back with non-zero jitter. The RNG
        // chooses each slot's deliver_at_us independently, so a later
        // send can out-rank an earlier one. We run until a reorder
        // occurs or fail out loud.
        let mut reordered = false;
        for seed in 1u64..=64 {
            let mut t: SimLora<FifoTransport, 8> = SimLora::new(FifoTransport::new(), seed);
            t.set_latency(0, 100);
            let a = sample(0xA1);
            let b = sample(0xB2);
            t.send_at(&a, 0).unwrap();
            t.send_at(&b, 0).unwrap();
            // Drain at t=0 so both slots get deterministic stamps in
            // [0, 100). Then pop at t=big to observe delivery order.
            t.drain_at(0);
            let mut out = [0u8; SEP_FRAME_BYTES];
            let n1 = t.pop_at(&mut out, 1_000_000).unwrap();
            assert_eq!(n1, SEP_FRAME_BYTES);
            let first = out[0];
            let n2 = t.pop_at(&mut out, 1_000_000).unwrap();
            assert_eq!(n2, SEP_FRAME_BYTES);
            let second = out[0];
            if (first, second) == (0xB2, 0xA1) {
                reordered = true;
                break;
            }
        }
        assert!(
            reordered,
            "no seed in 1..=64 produced a reorder — jitter is not randomising",
        );
    }

    #[test]
    fn queue_full_counts_drops() {
        // Q=2. Send 4 frames with non-zero jitter, drain inner all at
        // once, and observe the overflow counter.
        let mut t: SimLora<FifoTransport, 2> = SimLora::new(FifoTransport::new(), 7);
        t.set_latency(1_000_000, 0); // 1 s base — guaranteed not ready
        let frame = sample(0xF0);
        for _ in 0..4 {
            t.send_at(&frame, 0).unwrap();
        }
        // Drain inner; first 2 slots succeed, next 2 overflow.
        let mut out = [0u8; SEP_FRAME_BYTES];
        // recv_at at t=0 drains inner; all 4 frames are pulled. 2 land,
        // 2 are counted as queue-full. None are ready (deliver_at=1s).
        assert_eq!(t.recv_at(&mut out, 0), Err(SepTransportError::WouldBlock));
        assert_eq!(t.queued(), 2);
        assert_eq!(t.dropped_queue_full(), 2);
    }

    // ── Presets ───────────────────────────────────────────────────────────

    #[test]
    fn lossy_preset_loses_roughly_ten_percent() {
        let mut t: SimLora<FifoTransport, 1> = SimLora::lossy(FifoTransport::new(), 0xF00D);
        let frame = sample(0x12);
        for _ in 0..1000 {
            t.send_at(&frame, 0).unwrap();
            let mut out = [0u8; SEP_FRAME_BYTES];
            let _ = t.recv_at(&mut out, 1_000_000);
        }
        let lost = t.dropped_loss();
        // 10% of 1000 = 100, stddev sqrt(1000*0.1*0.9) ≈ 9.5. 4σ = [62, 138].
        assert!(
            (62..=138).contains(&lost),
            "lossy preset count outside 4σ window: {lost}",
        );
    }

    #[test]
    fn fringe_preset_loses_roughly_forty_percent() {
        let mut t: SimLora<FifoTransport, 1> = SimLora::fringe(FifoTransport::new(), 0xCAFE);
        let frame = sample(0x21);
        for _ in 0..1000 {
            t.send_at(&frame, 0).unwrap();
            let mut out = [0u8; SEP_FRAME_BYTES];
            let _ = t.recv_at(&mut out, 1_000_000_000);
        }
        let lost = t.dropped_loss();
        // 40% of 1000 = 400, stddev sqrt(1000*0.4*0.6) ≈ 15.5. 4σ = [338, 462].
        assert!(
            (338..=462).contains(&lost),
            "fringe preset count outside 4σ window: {lost}",
        );
    }

    // ── Parameter clamps ──────────────────────────────────────────────────

    #[test]
    fn setters_clamp_to_maxima() {
        let mut t: SimLora<FifoTransport, 1> = SimLora::new(FifoTransport::new(), 0);
        t.set_loss_ppm(u32::MAX);
        t.set_latency(u64::MAX, u64::MAX);
        // Nothing panics, and send_at with 100% loss confirms the
        // clamp landed on MAX_LOSS_PPM.
        let frame = sample(0x00);
        t.send_at(&frame, 0).unwrap();
        assert_eq!(t.dropped_loss(), 1);
    }
}

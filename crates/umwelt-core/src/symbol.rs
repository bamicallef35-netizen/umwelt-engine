//! Symbol layer — recurrence-driven pair commitment (ADR-0018).
//!
//! Peirce's third-ness: a canonical pair of channels that recurs above
//! a count threshold within a rolling recurrence window becomes a
//! [`SymbolEvent`]. The recogniser consumes `IndexEvent`s from the
//! correlator, maintains one `u8` counter per pair (upper-triangle),
//! and fires via a caller-owned callback the instant the count reaches
//! [`COMMITMENT_THRESHOLD`]. On commitment the counter resets to zero,
//! allowing the same pair to re-commit if it keeps recurring — feeding
//! naturally into ADR-0016's saturating-add reinforcement in the graph.
//!
//! Everything here is `no_std`, allocation-free, and `unsafe`-free. See
//! `docs/decisions.md` § ADR-0018 for the rationale behind count-threshold
//! commitment (vs. EWMA rate-crossing), content-addressed symbol-id
//! assignment, and the parameters below.

use crate::index::IndexEvent;
use crate::{TimestampUs, Q15};

/// Number of recurrences required to commit a symbol. Per ADR-0018.
pub const COMMITMENT_THRESHOLD: u8 = 3;

/// Default recurrence window in milliseconds. Pair counts that age
/// beyond this reset before the next increment is applied.
pub const RECURRENCE_WINDOW_MS: u64 = 10_000;

/// Upper clamp accepted by [`SymbolRecogniser::with_window_ms`]. Beyond
/// a minute, recurrence becomes indistinguishable from background rate.
pub const MAX_RECURRENCE_WINDOW_MS: u64 = 60_000;

/// One emission from the recogniser. Channels are passed through from
/// the triggering [`IndexEvent`] (already canonical ascending per
/// ADR-0017); `formed_at` and `coherence` likewise propagate unchanged,
/// giving downstream graph-insert code everything needed to build a
/// `Sign` of layer `Symbol`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SymbolEvent {
    /// Canonical pair (passes through from the triggering index event).
    pub channels: [u16; 2],
    /// Timestamp of the [`IndexEvent`] that tipped the counter.
    pub formed_at: TimestampUs,
    /// Coherence of the triggering [`IndexEvent`].
    pub coherence: Q15,
}

/// Error returned by [`SymbolRecogniser::observe`] when the slot table
/// is saturated. Atomic: the recogniser's state is untouched on the
/// failure path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ObserveError {
    /// Accepting this event would require registering a new channel,
    /// but the fixed slot table is full.
    ChannelFull,
}

/// Recurrence recogniser over `C` channels.
///
/// Channels are bound to slots on first appearance (same pattern as
/// [`crate::index::IndexCorrelator`]). The `counts` and `first_seen`
/// matrices are upper-triangular: only indices with `lo < hi` are
/// written. Worst-case state is `C + C + C*C + C*C*8 + 16` bytes —
/// `1_184` for `C = 16`.
///
/// Per-observe cost is `O(C)` (two channel lookups). The internal
/// increment / commit path is `O(1)`.
#[derive(Debug, Clone)]
pub struct SymbolRecogniser<const C: usize> {
    /// Channel id at each slot; only `0..occupied` is valid.
    channels: [u16; C],
    /// Number of distinct channels registered so far.
    occupied: usize,
    /// Upper-triangle pair counts, indexed by `(lo_slot, hi_slot)` with
    /// `lo < hi`. Diagonal and lower triangle are unused (writes would
    /// imply a self-pair, which `IndexEvent`'s invariant rules out).
    counts: [[u8; C]; C],
    /// Timestamp of the first still-counting event for each pair; only
    /// read when `counts[lo][hi] > 0`.
    first_seen: [[u64; C]; C],
    /// Recurrence window, in microseconds.
    window_us: u64,
    /// Recurrences required to commit. Default [`COMMITMENT_THRESHOLD`].
    threshold: u8,
}

impl<const C: usize> SymbolRecogniser<C> {
    /// Construct a recogniser with [`RECURRENCE_WINDOW_MS`] and
    /// [`COMMITMENT_THRESHOLD`].
    pub const fn new() -> Self {
        Self {
            channels: [0u16; C],
            occupied: 0,
            counts: [[0u8; C]; C],
            first_seen: [[0u64; C]; C],
            window_us: RECURRENCE_WINDOW_MS * 1_000,
            threshold: COMMITMENT_THRESHOLD,
        }
    }

    /// Construct with a custom window (clamped to
    /// `[0, MAX_RECURRENCE_WINDOW_MS]`) and the default threshold.
    pub const fn with_window_ms(window_ms: u64) -> Self {
        let clamped = if window_ms > MAX_RECURRENCE_WINDOW_MS {
            MAX_RECURRENCE_WINDOW_MS
        } else {
            window_ms
        };
        Self {
            channels: [0u16; C],
            occupied: 0,
            counts: [[0u8; C]; C],
            first_seen: [[0u64; C]; C],
            window_us: clamped * 1_000,
            threshold: COMMITMENT_THRESHOLD,
        }
    }

    /// Override the commitment threshold. `threshold == 0` is coerced
    /// to `1` — a zero-threshold recogniser would fire on every event
    /// and is definitionally not a "recurrence" detector.
    pub const fn with_threshold(mut self, threshold: u8) -> Self {
        self.threshold = if threshold == 0 { 1 } else { threshold };
        self
    }

    /// Current recurrence window, in microseconds.
    #[inline]
    pub const fn window_us(&self) -> u64 {
        self.window_us
    }

    /// Recurrences required to commit.
    #[inline]
    pub const fn threshold(&self) -> u8 {
        self.threshold
    }

    /// Number of distinct channels registered so far.
    #[inline]
    pub const fn occupied(&self) -> usize {
        self.occupied
    }

    /// Compile-time capacity.
    #[inline]
    pub const fn capacity(&self) -> usize {
        C
    }

    /// Feed one [`IndexEvent`] through the recogniser.
    ///
    /// Maps both channels to slot indices (allocating on first sight),
    /// resets any stale accumulation older than `window_us`, increments
    /// the pair's count, and fires `on_commit` if the threshold is
    /// reached — resetting the count to zero on emission so the pair
    /// can re-commit.
    ///
    /// Returns [`ObserveError::ChannelFull`] if accepting this event
    /// would push `occupied` past `C`; state is left unchanged in that
    /// case (both allocations are verified before either is written).
    pub fn observe<F>(&mut self, idx: &IndexEvent, mut on_commit: F) -> Result<(), ObserveError>
    where
        F: FnMut(SymbolEvent),
    {
        let [ch_a, ch_b] = idx.channels;

        // Resolve or reserve slots for both channels atomically.
        let slot_a_existing = self.find_slot(ch_a);
        let slot_b_existing = self.find_slot(ch_b);
        let needed_new = match (slot_a_existing, slot_b_existing) {
            (Some(_), Some(_)) => 0,
            (None, Some(_)) | (Some(_), None) => 1,
            (None, None) => {
                // Distinct channel ids per IndexEvent invariant
                // (channels[0] < channels[1]), so two new slots.
                2
            }
        };
        if self.occupied + needed_new > C {
            return Err(ObserveError::ChannelFull);
        }

        let slot_a = slot_a_existing.unwrap_or_else(|| {
            let i = self.occupied;
            self.channels[i] = ch_a;
            self.occupied += 1;
            i
        });
        let slot_b = slot_b_existing.unwrap_or_else(|| {
            let i = self.occupied;
            self.channels[i] = ch_b;
            self.occupied += 1;
            i
        });

        // Canonical upper-triangle index.
        let (lo, hi) = if slot_a < slot_b {
            (slot_a, slot_b)
        } else {
            (slot_b, slot_a)
        };

        // Stale-window reset. If the running count is non-zero but the
        // window opened too long ago, drop the accumulation and start
        // fresh from this event.
        let count = &mut self.counts[lo][hi];
        let first = &mut self.first_seen[lo][hi];
        if *count > 0 {
            let elapsed = idx.formed_at.saturating_sub(*first);
            if elapsed > self.window_us {
                *count = 0;
            }
        }
        if *count == 0 {
            *first = idx.formed_at;
        }

        *count = count.saturating_add(1);

        if *count >= self.threshold {
            *count = 0;
            on_commit(SymbolEvent {
                channels: idx.channels,
                formed_at: idx.formed_at,
                coherence: idx.coherence,
            });
        }

        Ok(())
    }

    /// Linear scan for the slot holding `channel`, if any. O(`C`).
    #[inline]
    fn find_slot(&self, channel: u16) -> Option<usize> {
        let mut i = 0;
        while i < self.occupied {
            if self.channels[i] == channel {
                return Some(i);
            }
            i += 1;
        }
        None
    }
}

impl<const C: usize> Default for SymbolRecogniser<C> {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn idx_event(a: u16, b: u16, formed_at: TimestampUs, coh_raw: i16) -> IndexEvent {
        let (lo, hi) = if a < b { (a, b) } else { (b, a) };
        IndexEvent {
            channels: [lo, hi],
            delta_us: 0,
            formed_at,
            coherence: Q15::from_raw(coh_raw),
        }
    }

    #[test]
    fn new_starts_empty_with_defaults() {
        let r: SymbolRecogniser<4> = SymbolRecogniser::new();
        assert_eq!(r.occupied(), 0);
        assert_eq!(r.capacity(), 4);
        assert_eq!(r.threshold(), COMMITMENT_THRESHOLD);
        assert_eq!(r.window_us(), RECURRENCE_WINDOW_MS * 1_000);
    }

    #[test]
    fn with_window_ms_clamps_above_max() {
        let r: SymbolRecogniser<4> = SymbolRecogniser::with_window_ms(120_000);
        assert_eq!(r.window_us(), MAX_RECURRENCE_WINDOW_MS * 1_000);
        let r2: SymbolRecogniser<4> = SymbolRecogniser::with_window_ms(1_500);
        assert_eq!(r2.window_us(), 1_500_000);
    }

    #[test]
    fn with_threshold_coerces_zero_to_one() {
        let r: SymbolRecogniser<4> = SymbolRecogniser::new().with_threshold(0);
        assert_eq!(r.threshold(), 1);
        let r2: SymbolRecogniser<4> = SymbolRecogniser::new().with_threshold(5);
        assert_eq!(r2.threshold(), 5);
    }

    #[test]
    fn below_threshold_never_emits() {
        let mut r: SymbolRecogniser<4> = SymbolRecogniser::new();
        let mut count = 0;
        for i in 0..(COMMITMENT_THRESHOLD - 1) {
            r.observe(&idx_event(2, 5, 1_000 + i as u64, 20_000), |_| count += 1)
                .unwrap();
        }
        assert_eq!(count, 0);
    }

    #[test]
    fn exactly_at_threshold_emits_once() {
        let mut r: SymbolRecogniser<4> = SymbolRecogniser::new();
        let mut events = heapless::Vec::<SymbolEvent, 8>::new();
        for i in 0..COMMITMENT_THRESHOLD {
            r.observe(&idx_event(2, 5, 1_000 + i as u64, 20_000), |e| {
                events.push(e).unwrap();
            })
            .unwrap();
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].channels, [2, 5]);
    }

    #[test]
    fn count_resets_after_commitment() {
        let mut r: SymbolRecogniser<4> = SymbolRecogniser::new();
        let mut events = heapless::Vec::<SymbolEvent, 8>::new();
        // Two full threshold runs back-to-back should produce two events.
        for i in 0..(COMMITMENT_THRESHOLD * 2) {
            r.observe(&idx_event(2, 5, 1_000 + i as u64, 20_000), |e| {
                events.push(e).unwrap();
            })
            .unwrap();
        }
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn stale_window_resets_counter() {
        let mut r: SymbolRecogniser<4> = SymbolRecogniser::new();
        let mut events = heapless::Vec::<SymbolEvent, 8>::new();
        let window = r.window_us();
        // One in-window event, then THRESHOLD-1 more after the window
        // elapses; the stale first must be dropped, so only threshold-1
        // events count — no emission.
        r.observe(&idx_event(2, 5, 1_000, 20_000), |e| {
            events.push(e).unwrap();
        })
        .unwrap();
        for i in 0..(COMMITMENT_THRESHOLD - 1) {
            let t = 1_000 + window + 2 + i as u64;
            r.observe(&idx_event(2, 5, t, 20_000), |e| {
                events.push(e).unwrap();
            })
            .unwrap();
        }
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn stale_window_reset_then_fresh_run_emits() {
        let mut r: SymbolRecogniser<4> = SymbolRecogniser::new();
        let mut events = heapless::Vec::<SymbolEvent, 8>::new();
        let window = r.window_us();
        r.observe(&idx_event(2, 5, 1_000, 20_000), |e| {
            events.push(e).unwrap();
        })
        .unwrap();
        let base = 1_000 + window + 10;
        for i in 0..COMMITMENT_THRESHOLD {
            r.observe(&idx_event(2, 5, base + i as u64, 20_000), |e| {
                events.push(e).unwrap();
            })
            .unwrap();
        }
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].formed_at,
            base + (COMMITMENT_THRESHOLD - 1) as u64
        );
    }

    #[test]
    fn channel_full_on_capacity_exceeded() {
        let mut r: SymbolRecogniser<2> = SymbolRecogniser::new();
        r.observe(&idx_event(1, 2, 100, 20_000), |_| {}).unwrap();
        // Adds nothing — both channels already bound.
        r.observe(&idx_event(1, 2, 200, 20_000), |_| {}).unwrap();
        // A fresh channel needs a third slot.
        let err = r.observe(&idx_event(1, 3, 300, 20_000), |_| {});
        assert_eq!(err, Err(ObserveError::ChannelFull));
        // State untouched on failure.
        assert_eq!(r.occupied(), 2);
    }

    #[test]
    fn channel_full_two_new_channels_atomic() {
        // Occupancy 3 of 4, an event introducing TWO new channels
        // cannot fit and must leave state untouched.
        let mut r: SymbolRecogniser<4> = SymbolRecogniser::new();
        r.observe(&idx_event(1, 2, 100, 10_000), |_| {}).unwrap();
        r.observe(&idx_event(2, 3, 200, 10_000), |_| {}).unwrap();
        assert_eq!(r.occupied(), 3);
        let err = r.observe(&idx_event(7, 9, 300, 10_000), |_| {});
        assert_eq!(err, Err(ObserveError::ChannelFull));
        assert_eq!(r.occupied(), 3);
    }

    #[test]
    fn distinct_pairs_are_independent() {
        let mut r: SymbolRecogniser<4> = SymbolRecogniser::new();
        let mut events = heapless::Vec::<SymbolEvent, 8>::new();
        // Interleave two pairs; each should commit independently at
        // their own threshold-count.
        for i in 0..COMMITMENT_THRESHOLD {
            r.observe(&idx_event(2, 5, 1_000 + i as u64, 20_000), |e| {
                events.push(e).unwrap();
            })
            .unwrap();
            r.observe(&idx_event(3, 7, 1_000 + i as u64, 25_000), |e| {
                events.push(e).unwrap();
            })
            .unwrap();
        }
        assert_eq!(events.len(), 2);
        let mut chans: heapless::Vec<[u16; 2], 8> = heapless::Vec::new();
        for e in &events {
            chans.push(e.channels).unwrap();
        }
        assert!(chans.contains(&[2, 5]));
        assert!(chans.contains(&[3, 7]));
    }

    // ── Proptests (ADR-0018 contract) ──────────────────────────────────────

    proptest! {
        #[test]
        fn below_threshold_never_emits_prop(
            n in 0u8..COMMITMENT_THRESHOLD,
            a in 0u16..64,
            b in 0u16..64,
            dt in 1u64..1_000,
        ) {
            prop_assume!(a != b);
            let mut r: SymbolRecogniser<8> = SymbolRecogniser::new();
            let mut fired = 0;
            for i in 0..n {
                r.observe(&idx_event(a, b, 1_000 + i as u64 * dt, 20_000),
                    |_| fired += 1).unwrap();
            }
            prop_assert_eq!(fired, 0);
        }

        #[test]
        fn threshold_exact_emits_once_prop(
            a in 0u16..64,
            b in 0u16..64,
            coh in 0i16..=i16::MAX,
            dt in 1u64..1_000,
        ) {
            prop_assume!(a != b);
            let mut r: SymbolRecogniser<8> = SymbolRecogniser::new();
            let mut events = heapless::Vec::<SymbolEvent, 8>::new();
            for i in 0..COMMITMENT_THRESHOLD {
                r.observe(&idx_event(a, b, 1_000 + i as u64 * dt, coh),
                    |e| events.push(e).unwrap()).unwrap();
            }
            prop_assert_eq!(events.len(), 1);
        }

        #[test]
        fn symbol_event_channels_match_index_event_prop(
            a in 0u16..64,
            b in 0u16..64,
        ) {
            prop_assume!(a != b);
            let mut r: SymbolRecogniser<8> = SymbolRecogniser::new();
            let mut captured: Option<SymbolEvent> = None;
            for i in 0..COMMITMENT_THRESHOLD {
                r.observe(&idx_event(a, b, 1_000 + i as u64, 20_000),
                    |e| captured = Some(e)).unwrap();
            }
            let e = captured.expect("should have emitted");
            let (lo, hi) = if a < b { (a, b) } else { (b, a) };
            prop_assert_eq!(e.channels, [lo, hi]);
        }

        #[test]
        fn coherence_and_formed_at_pass_through_prop(
            a in 0u16..64,
            b in 0u16..64,
            coh in 0i16..=i16::MAX,
            t_last in 1_000u64..10_000,
        ) {
            prop_assume!(a != b);
            // Drive up to just-before-threshold with filler, then the
            // final event carries the coherence/timestamp we assert.
            let mut r: SymbolRecogniser<8> = SymbolRecogniser::new();
            let mut captured: Option<SymbolEvent> = None;
            for i in 0..(COMMITMENT_THRESHOLD - 1) {
                r.observe(&idx_event(a, b, 100 + i as u64, 10_000),
                    |_| {}).unwrap();
            }
            r.observe(&idx_event(a, b, t_last, coh),
                |e| captured = Some(e)).unwrap();
            let e = captured.expect("should have emitted");
            prop_assert_eq!(e.formed_at, t_last);
            prop_assert_eq!(e.coherence.raw(), coh);
        }

        #[test]
        fn count_resets_then_second_run_emits_prop(
            a in 0u16..64,
            b in 0u16..64,
        ) {
            prop_assume!(a != b);
            let mut r: SymbolRecogniser<8> = SymbolRecogniser::new();
            let mut events = heapless::Vec::<SymbolEvent, 16>::new();
            for i in 0..(COMMITMENT_THRESHOLD as u64 * 2) {
                r.observe(&idx_event(a, b, 1_000 + i, 20_000),
                    |e| events.push(e).unwrap()).unwrap();
            }
            prop_assert_eq!(events.len(), 2);
        }

        #[test]
        fn channel_full_leaves_state_untouched_prop(
            seed in 0u16..200,
        ) {
            // Fill a cap-2 recogniser, then probe with a third channel.
            let mut r: SymbolRecogniser<2> = SymbolRecogniser::new();
            let a = seed;
            let b = seed.wrapping_add(1).max(seed + 1);
            prop_assume!(a != b);
            r.observe(&idx_event(a, b, 100, 10_000), |_| {}).unwrap();
            let before = r.occupied();
            let c = seed.wrapping_add(2).max(seed + 2);
            prop_assume!(c != a && c != b);
            let err = r.observe(&idx_event(a, c, 200, 10_000), |_| {});
            prop_assert_eq!(err, Err(ObserveError::ChannelFull));
            prop_assert_eq!(r.occupied(), before);
        }
    }
}

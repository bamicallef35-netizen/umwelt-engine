//! Index layer — pairwise temporal correlator (ADR-0017).
//!
//! Peirce's second-ness: two distinct-channel icons that fire within a
//! short sliding window become an [`IndexEvent`]. The correlator holds
//! the most-recent icon per channel and, on each new icon, enumerates
//! every other occupied slot whose timestamp still lies inside the
//! window, emitting one event per qualifying pair via a caller-owned
//! callback.
//!
//! Everything here is `no_std`, allocation-free, and `unsafe`-free. See
//! `docs/decisions.md` § ADR-0017 for the full rationale, including why the
//! coherence scalar is the geometric mean of the two magnitudes and why
//! the pair enumeration is a flat O(C) scan rather than an event-driven
//! heap.

use crate::icon::{sqrt_u32, IconEvent};
use crate::{TimestampUs, Q15};

/// Default co-occurrence window, in milliseconds. Five ticks at the
/// ADR-0015 reference rate (10 Hz).
pub const DEFAULT_WINDOW_MS: u64 = 500;

/// Upper bound accepted by [`IndexCorrelator::with_window_ms`]. Values
/// above this clamp down to `MAX_WINDOW_MS` — beyond 3 s the ADR-0015
/// decay curve has erased the signal the Index layer is looking for.
pub const MAX_WINDOW_MS: u64 = 3_000;

/// One emission from the correlator. The `channels` array is always
/// sorted ascending (`channels[0] < channels[1]`). `formed_at` is the
/// *incoming* icon's timestamp so downstream consumers see events in
/// monotonic order; `delta_us` is the absolute separation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct IndexEvent {
    /// Canonical pair of channels: `channels[0] < channels[1]`.
    pub channels: [u16; 2],
    /// `|t_new − t_old|`, always ≤ the correlator's window.
    pub delta_us: u64,
    /// Timestamp of the icon that triggered this emission.
    pub formed_at: TimestampUs,
    /// Geometric mean of the two icons' magnitudes, in Q15.
    /// Always in `[0, Q15::MAX]`; zero iff either magnitude was zero.
    pub coherence: Q15,
}

/// Error returned by [`IndexCorrelator::observe`] when the correlator's
/// fixed channel table is full. Mirrors [`ReplayGuard`]'s `UnknownPeer`
/// pattern from the ADR-0011 family: an explicit refusal, not a silent
/// drop.
///
/// [`ReplayGuard`]: crate::graph::HeaplessGraph
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ObserveError {
    /// The correlator already has `C` distinct channels registered and
    /// the incoming icon's channel is new.
    ChannelFull,
}

/// Most recent icon in a given slot.
#[derive(Debug, Clone, Copy)]
struct LastIcon {
    formed_at: TimestampUs,
    magnitude: Q15,
}

/// Pairwise icon correlator over `C` channels.
///
/// Channels are bound to slots on first appearance; the mapping is
/// stable for the lifetime of the correlator. Per-observe cost is
/// `O(C)` — a single scan of the slot table — with zero heap
/// allocation.
#[derive(Debug, Clone)]
pub struct IndexCorrelator<const C: usize> {
    /// Most-recent icon per slot, `None` until a channel fires.
    slots: [Option<LastIcon>; C],
    /// Channel id at each slot. Only indices `0..occupied` are valid.
    channels: [u16; C],
    /// Number of distinct channels registered so far, `≤ C`.
    occupied: usize,
    /// Window, in microseconds. `window_ms * 1000`.
    window_us: u64,
}

impl<const C: usize> IndexCorrelator<C> {
    /// Construct a correlator with [`DEFAULT_WINDOW_MS`].
    pub const fn new() -> Self {
        Self {
            slots: [None; C],
            channels: [0u16; C],
            occupied: 0,
            window_us: DEFAULT_WINDOW_MS * 1_000,
        }
    }

    /// Construct a correlator with a custom window, clamped to
    /// `[0, MAX_WINDOW_MS]`.
    pub const fn with_window_ms(window_ms: u64) -> Self {
        let clamped = if window_ms > MAX_WINDOW_MS {
            MAX_WINDOW_MS
        } else {
            window_ms
        };
        Self {
            slots: [None; C],
            channels: [0u16; C],
            occupied: 0,
            window_us: clamped * 1_000,
        }
    }

    /// Window, in microseconds.
    #[inline]
    pub const fn window_us(&self) -> u64 {
        self.window_us
    }

    /// Number of distinct channels registered so far.
    #[inline]
    pub const fn occupied(&self) -> usize {
        self.occupied
    }

    /// Capacity (compile-time `C`).
    #[inline]
    pub const fn capacity(&self) -> usize {
        C
    }

    /// Feed one icon through the correlator.
    ///
    /// For every other slot whose most-recent icon lies inside the
    /// window, emits one [`IndexEvent`] via `on_pair`. Then updates the
    /// slot for `icon.channel` to reflect this as the new most-recent.
    ///
    /// Returns [`ObserveError::ChannelFull`] if `icon.channel` is new
    /// and the slot table is already saturated; the internal state is
    /// left unchanged in that case.
    pub fn observe<F>(&mut self, icon: &IconEvent, mut on_pair: F) -> Result<(), ObserveError>
    where
        F: FnMut(IndexEvent),
    {
        // Resolve (or allocate) the slot for the incoming channel.
        let my_slot = match self.find_slot(icon.channel) {
            Some(i) => i,
            None => {
                if self.occupied >= C {
                    return Err(ObserveError::ChannelFull);
                }
                let i = self.occupied;
                self.channels[i] = icon.channel;
                self.occupied += 1;
                i
            }
        };

        // Enumerate every *other* occupied slot and emit on within-window
        // pairs. Out-of-order icons (new.formed_at < old.formed_at) are
        // skipped — v0.1 assumes per-agent monotone time.
        let new_t = icon.formed_at;
        for i in 0..self.occupied {
            if i == my_slot {
                continue;
            }
            let Some(last) = self.slots[i] else {
                continue;
            };
            if new_t < last.formed_at {
                continue;
            }
            let delta_us = new_t - last.formed_at;
            if delta_us > self.window_us {
                continue;
            }
            let other_chan = self.channels[i];
            let (c_lo, c_hi) = if icon.channel < other_chan {
                (icon.channel, other_chan)
            } else {
                (other_chan, icon.channel)
            };
            // Geometric-mean coherence. `magnitude` is non-negative Q15
            // by IconEvent's invariant; `unsigned_abs` is defensive in
            // case a future caller ever hands us a signed value.
            let m_a = (icon.magnitude.raw() as i32).unsigned_abs();
            let m_b = (last.magnitude.raw() as i32).unsigned_abs();
            let product = m_a * m_b; // ≤ 32767² ≈ 1.07e9 < 2³²
            let coh_raw = sqrt_u32(product) as i16; // ≤ 32767, fits i16
            on_pair(IndexEvent {
                channels: [c_lo, c_hi],
                delta_us,
                formed_at: new_t,
                coherence: Q15::from_raw(coh_raw),
            });
        }

        // Commit the new most-recent icon for this channel.
        self.slots[my_slot] = Some(LastIcon {
            formed_at: new_t,
            magnitude: icon.magnitude,
        });
        Ok(())
    }

    /// Linear scan for the slot holding `channel`, if any.
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

impl<const C: usize> Default for IndexCorrelator<C> {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn icon(channel: u16, formed_at: TimestampUs, magnitude_raw: i16) -> IconEvent {
        IconEvent {
            channel,
            formed_at,
            entropy: Q15::ZERO,
            magnitude: Q15::from_raw(magnitude_raw),
        }
    }

    #[test]
    fn new_starts_empty_with_default_window() {
        let c: IndexCorrelator<4> = IndexCorrelator::new();
        assert_eq!(c.occupied(), 0);
        assert_eq!(c.capacity(), 4);
        assert_eq!(c.window_us(), DEFAULT_WINDOW_MS * 1_000);
    }

    #[test]
    fn with_window_ms_clamps_above_max() {
        let c: IndexCorrelator<4> = IndexCorrelator::with_window_ms(10_000);
        assert_eq!(c.window_us(), MAX_WINDOW_MS * 1_000);
        let c2: IndexCorrelator<4> = IndexCorrelator::with_window_ms(250);
        assert_eq!(c2.window_us(), 250_000);
    }

    #[test]
    fn first_icon_registers_slot_no_emission() {
        let mut c: IndexCorrelator<4> = IndexCorrelator::new();
        let mut count = 0;
        c.observe(&icon(7, 1_000, 20_000), |_| count += 1).unwrap();
        assert_eq!(count, 0);
        assert_eq!(c.occupied(), 1);
    }

    #[test]
    fn same_channel_never_emits() {
        let mut c: IndexCorrelator<4> = IndexCorrelator::new();
        let mut events: heapless::Vec<IndexEvent, 8> = heapless::Vec::new();
        c.observe(&icon(3, 1_000, 20_000), |e| events.push(e).unwrap())
            .unwrap();
        c.observe(&icon(3, 1_100, 30_000), |e| events.push(e).unwrap())
            .unwrap();
        c.observe(&icon(3, 1_200, 25_000), |e| events.push(e).unwrap())
            .unwrap();
        assert_eq!(events.len(), 0);
        assert_eq!(c.occupied(), 1);
    }

    #[test]
    fn two_channels_within_window_emit_once_sorted() {
        let mut c: IndexCorrelator<4> = IndexCorrelator::with_window_ms(500);
        let mut events: heapless::Vec<IndexEvent, 8> = heapless::Vec::new();
        c.observe(&icon(9, 1_000_000, 20_000), |e| events.push(e).unwrap())
            .unwrap();
        c.observe(&icon(3, 1_400_000, 30_000), |e| events.push(e).unwrap())
            .unwrap();
        assert_eq!(events.len(), 1);
        let ev = events[0];
        assert_eq!(ev.channels, [3, 9]); // sorted ascending
        assert_eq!(ev.delta_us, 400_000);
        assert_eq!(ev.formed_at, 1_400_000);
        // geomean(20000, 30000) = sqrt(6e8) ≈ 24495.
        let approx = (20_000u32 * 30_000u32) as f64;
        let expected = approx.sqrt() as i16;
        let delta = (ev.coherence.raw() - expected).abs();
        assert!(
            delta <= 1,
            "coherence {} off by {}",
            ev.coherence.raw(),
            delta
        );
    }

    #[test]
    fn outside_window_no_emit() {
        let mut c: IndexCorrelator<4> = IndexCorrelator::with_window_ms(100);
        let mut events: heapless::Vec<IndexEvent, 8> = heapless::Vec::new();
        c.observe(&icon(1, 1_000_000, 20_000), |e| events.push(e).unwrap())
            .unwrap();
        // 200 ms later — outside 100 ms window.
        c.observe(&icon(2, 1_200_000, 20_000), |e| events.push(e).unwrap())
            .unwrap();
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn three_channels_within_window_emit_two_events() {
        let mut c: IndexCorrelator<4> = IndexCorrelator::with_window_ms(500);
        let mut events: heapless::Vec<IndexEvent, 8> = heapless::Vec::new();
        c.observe(&icon(5, 1_000_000, 10_000), |e| events.push(e).unwrap())
            .unwrap();
        c.observe(&icon(2, 1_100_000, 20_000), |e| events.push(e).unwrap())
            .unwrap();
        c.observe(&icon(8, 1_200_000, 30_000), |e| events.push(e).unwrap())
            .unwrap();
        // Third icon pairs with slot-0 (ch=5) and slot-1 (ch=2).
        assert_eq!(events.len(), 3); // 1 from 2nd observe + 2 from 3rd
                                     // Events from the third observe:
        assert_eq!(events[1].channels, [5, 8]);
        assert_eq!(events[2].channels, [2, 8]);
    }

    #[test]
    fn channel_full_returns_error() {
        let mut c: IndexCorrelator<2> = IndexCorrelator::new();
        c.observe(&icon(1, 100, 10_000), |_| {}).unwrap();
        c.observe(&icon(2, 200, 10_000), |_| {}).unwrap();
        let err = c.observe(&icon(3, 300, 10_000), |_| {}).unwrap_err();
        assert_eq!(err, ObserveError::ChannelFull);
        // Existing channel still works.
        c.observe(&icon(1, 400, 10_000), |_| {}).unwrap();
        assert_eq!(c.occupied(), 2);
    }

    #[test]
    fn out_of_order_timestamps_skip() {
        let mut c: IndexCorrelator<4> = IndexCorrelator::with_window_ms(500);
        let mut events: heapless::Vec<IndexEvent, 8> = heapless::Vec::new();
        c.observe(&icon(1, 1_000_000, 20_000), |e| events.push(e).unwrap())
            .unwrap();
        // Earlier than slot-0's timestamp: skip.
        c.observe(&icon(2, 900_000, 20_000), |e| events.push(e).unwrap())
            .unwrap();
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn zero_magnitude_gives_zero_coherence() {
        let mut c: IndexCorrelator<4> = IndexCorrelator::with_window_ms(500);
        let mut events: heapless::Vec<IndexEvent, 8> = heapless::Vec::new();
        c.observe(&icon(1, 1_000_000, 0), |e| events.push(e).unwrap())
            .unwrap();
        c.observe(&icon(2, 1_100_000, 30_000), |e| events.push(e).unwrap())
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].coherence, Q15::ZERO);
    }

    #[test]
    fn boundary_exactly_at_window_emits() {
        let mut c: IndexCorrelator<4> = IndexCorrelator::with_window_ms(500);
        let mut events: heapless::Vec<IndexEvent, 8> = heapless::Vec::new();
        c.observe(&icon(1, 1_000_000, 20_000), |e| events.push(e).unwrap())
            .unwrap();
        // Exactly 500_000 µs later.
        c.observe(&icon(2, 1_500_000, 20_000), |e| events.push(e).unwrap())
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].delta_us, 500_000);
    }

    // ── Proptests per ADR-0017 contract ─────────────────────────────────────

    fn small_mag() -> impl Strategy<Value = i16> {
        0i16..=32_767
    }

    proptest! {
        /// Same channel, repeated: never emits.
        #[test]
        fn prop_same_channel_never_emits(
            ch in 0u16..16,
            mags in proptest::collection::vec(small_mag(), 1..32),
        ) {
            let mut c: IndexCorrelator<4> = IndexCorrelator::new();
            let mut count = 0usize;
            let mut t = 0u64;
            for m in mags {
                t += 1_000;
                c.observe(&icon(ch, t, m), |_| count += 1).unwrap();
            }
            prop_assert_eq!(count, 0);
        }

        /// Two distinct channels within the window emit exactly one
        /// event with sorted channels.
        #[test]
        fn prop_two_channels_within_window_emit_once(
            (c_a, c_b) in (0u16..100, 0u16..100).prop_filter("distinct", |(a, b)| a != b),
            m_a in small_mag(),
            m_b in small_mag(),
            gap_us in 0u64..=500_000,
        ) {
            let mut c: IndexCorrelator<8> = IndexCorrelator::with_window_ms(500);
            let mut events: heapless::Vec<IndexEvent, 4> = heapless::Vec::new();
            c.observe(&icon(c_a, 1_000_000, m_a), |e| events.push(e).unwrap()).unwrap();
            c.observe(&icon(c_b, 1_000_000 + gap_us, m_b), |e| events.push(e).unwrap()).unwrap();
            prop_assert_eq!(events.len(), 1);
            let ev = events[0];
            prop_assert!(ev.channels[0] < ev.channels[1]);
            let (lo, hi) = if c_a < c_b { (c_a, c_b) } else { (c_b, c_a) };
            prop_assert_eq!(ev.channels, [lo, hi]);
            prop_assert_eq!(ev.delta_us, gap_us);
        }

        /// Two distinct channels outside the window never emit.
        #[test]
        fn prop_outside_window_never_emits(
            (c_a, c_b) in (0u16..100, 0u16..100).prop_filter("distinct", |(a, b)| a != b),
            m_a in small_mag(),
            m_b in small_mag(),
            gap_us in 500_001u64..=10_000_000,
        ) {
            let mut c: IndexCorrelator<8> = IndexCorrelator::with_window_ms(500);
            let mut count = 0usize;
            c.observe(&icon(c_a, 1_000_000, m_a), |_| count += 1).unwrap();
            c.observe(&icon(c_b, 1_000_000 + gap_us, m_b), |_| count += 1).unwrap();
            prop_assert_eq!(count, 0);
        }

        /// `geomean(0, _) = 0`: zero magnitude on either side ⇒ zero coherence.
        #[test]
        fn prop_zero_magnitude_zero_coherence(m in small_mag()) {
            let mut c: IndexCorrelator<4> = IndexCorrelator::with_window_ms(500);
            let mut events: heapless::Vec<IndexEvent, 4> = heapless::Vec::new();
            c.observe(&icon(1, 1_000_000, 0), |e| events.push(e).unwrap()).unwrap();
            c.observe(&icon(2, 1_100_000, m), |e| events.push(e).unwrap()).unwrap();
            prop_assert_eq!(events.len(), 1);
            prop_assert_eq!(events[0].coherence, Q15::ZERO);
        }

        /// Coherence is monotone in each magnitude: strengthening one
        /// icon never weakens the pair's coherence.
        #[test]
        fn prop_coherence_monotone_in_magnitude(
            m_a in 1i16..=32_767,
            m_b in 1i16..=32_767,
            bump_b in 0i16..=100,
        ) {
            let mut c1: IndexCorrelator<4> = IndexCorrelator::with_window_ms(500);
            let mut e1: heapless::Vec<IndexEvent, 4> = heapless::Vec::new();
            c1.observe(&icon(1, 1_000_000, m_a), |e| e1.push(e).unwrap()).unwrap();
            c1.observe(&icon(2, 1_100_000, m_b), |e| e1.push(e).unwrap()).unwrap();

            let m_b2 = m_b.saturating_add(bump_b);
            let mut c2: IndexCorrelator<4> = IndexCorrelator::with_window_ms(500);
            let mut e2: heapless::Vec<IndexEvent, 4> = heapless::Vec::new();
            c2.observe(&icon(1, 1_000_000, m_a), |e| e2.push(e).unwrap()).unwrap();
            c2.observe(&icon(2, 1_100_000, m_b2), |e| e2.push(e).unwrap()).unwrap();

            prop_assert_eq!(e1.len(), 1);
            prop_assert_eq!(e2.len(), 1);
            prop_assert!(e2[0].coherence.raw() >= e1[0].coherence.raw());
        }

        /// Emission count per observe ≤ occupied − 1 (never correlates
        /// against its own slot, never revisits).
        #[test]
        fn prop_emission_bounded_by_occupied(
            channels in proptest::collection::vec(0u16..8, 1..32),
        ) {
            let mut c: IndexCorrelator<8> = IndexCorrelator::with_window_ms(500);
            let mut t = 1_000_000u64;
            for ch in channels {
                t += 10_000;
                let mut count = 0usize;
                let occupied_before = c.occupied();
                let known = (0..occupied_before).any(|i| c.channels[i] == ch);
                let _ = c.observe(&icon(ch, t, 10_000), |_| count += 1);
                // Bound: at most (occupied_before) partners if channel
                // was new, else (occupied_before − 1).
                let bound = if known { occupied_before.saturating_sub(1) } else { occupied_before };
                prop_assert!(count <= bound, "count={} bound={}", count, bound);
            }
        }
    }
}

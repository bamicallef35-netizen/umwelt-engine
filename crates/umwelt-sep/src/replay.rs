//! Anti-replay window (P2.5).
//!
//! Builds on P2.3's [`SeenSet`](crate::relay::SeenSet) to reject two
//! classes of replayed frames:
//!
//! 1. **Literal duplicates** — the `(agent_id, timestamp_ms)` tuple
//!    is already present in the recent-seen ring. Handled by the
//!    existing `SeenSet`.
//! 2. **Too-old frames** — even if the tuple isn't in the ring, a
//!    frame whose `timestamp_ms` is older than the highest we've
//!    accepted from that peer by more than `window_ms` is rejected
//!    outright. Prevents an attacker from replaying a frame that
//!    fell off the end of the ring due to unrelated traffic.
//!
//! Everything is `no_std`, allocation-free, and fixed-capacity.
//! Peer tracking uses [`heapless::FnvIndexMap`] with a compile-time
//! cap `M_PEERS`; attempting to register the `M_PEERS + 1`-th peer
//! returns [`ReplayVerdict::UnknownPeer`] so the operator is forced
//! to pick an explicit cap rather than silently falling over.
//!
//! ## Wrap-around
//!
//! `timestamp_ms` is `u32` and wraps every ~49.7 days. The window
//! check uses signed interpretation of `wrapping_sub`: if
//! `ts.wrapping_sub(highest)` reinterpreted as `i32` is non-negative
//! the frame is "same or newer"; if in `[-window, -1]` it is a
//! tolerated out-of-order late arrival; below `-window` it is
//! rejected as `TooOld`. This handles the u32 wrap point
//! transparently as long as `window_ms < 2^31`, which is
//! ~24.8 days — comfortably beyond any deployment's replay window.

use crate::relay::{Seen, SeenKey, SeenSet};
use crate::SepFrame;

// ── Verdict ────────────────────────────────────────────────────────────

/// Outcome of submitting a frame to a [`ReplayGuard`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayVerdict {
    /// Accept the frame. Its `(agent_id, timestamp_ms)` has been
    /// recorded and, if newer, the peer's high-water timestamp has
    /// been updated.
    Fresh,
    /// `(agent_id, timestamp_ms)` is already in the recent-seen ring
    /// — a literal duplicate. The guard state is unchanged.
    Duplicate,
    /// The frame's timestamp is older than the peer's high-water
    /// mark by more than `window_ms`. Guard state is unchanged.
    TooOld,
    /// The peer-tracking map is full and this is a new peer. The
    /// guard refuses to evict an existing peer silently; the
    /// operator must resize `M_PEERS` (or trigger some out-of-band
    /// purge policy). Guard state is unchanged.
    UnknownPeer,
}

// ── ReplayGuard ────────────────────────────────────────────────────────

/// Per-peer anti-replay state.
///
/// * `N_SEEN` — size of the literal-duplicate ring (`SeenSet` FIFO).
///   Power of 2 *not* required.
/// * `M_PEERS` — maximum number of distinct peers tracked. **Must
///   be a power of 2** (heapless `FnvIndexMap` requirement).
///
/// Typical v0.1 deployment: `N_SEEN = 64`, `M_PEERS = 32` for a
/// small swarm; `M_PEERS = 128` or `256` for a fleet.
#[derive(Debug)]
pub struct ReplayGuard<const N_SEEN: usize, const M_PEERS: usize> {
    seen: SeenSet<N_SEEN>,
    highest_ts: heapless::FnvIndexMap<u32, u32, M_PEERS>,
    window_ms: u32,
}

impl<const N_SEEN: usize, const M_PEERS: usize> ReplayGuard<N_SEEN, M_PEERS> {
    /// Construct a new guard with `window_ms` lateness tolerance.
    ///
    /// `window_ms` must satisfy `window_ms < 2^31` so the signed
    /// wrap-around math in [`check`][Self::check] stays unambiguous.
    /// Larger values are silently clamped to `i32::MAX as u32`.
    #[inline]
    pub const fn new(window_ms: u32) -> Self {
        let w = if window_ms > i32::MAX as u32 {
            i32::MAX as u32
        } else {
            window_ms
        };
        Self {
            seen: SeenSet::new(),
            highest_ts: heapless::FnvIndexMap::new(),
            window_ms: w,
        }
    }

    /// The configured lateness window.
    #[inline]
    pub const fn window_ms(&self) -> u32 {
        self.window_ms
    }

    /// Number of peers currently tracked (0..=M_PEERS).
    #[inline]
    pub fn tracked_peers(&self) -> usize {
        self.highest_ts.len()
    }

    /// Retrieve the current high-water timestamp for `agent_id`, if
    /// we've ever accepted a frame from it.
    #[inline]
    pub fn highest_for(&self, agent_id: u32) -> Option<u32> {
        self.highest_ts.get(&agent_id).copied()
    }

    /// Submit a frame to the guard. See [`ReplayVerdict`] for the
    /// possible outcomes.
    ///
    /// `check` is *not* idempotent on the `Fresh` path — it updates
    /// the seen-set and (possibly) the peer's high-water mark. Call
    /// exactly once per received frame.
    pub fn check(&mut self, frame: &SepFrame) -> ReplayVerdict {
        let key: SeenKey = (frame.agent_id, frame.timestamp_ms);

        // ── Literal duplicate? Cheapest check; bail before we touch
        //    the peer map.
        if self.seen.contains(&key) {
            return ReplayVerdict::Duplicate;
        }

        // ── Peer registered? Age-check against the high-water mark.
        if let Some(&highest) = self.highest_ts.get(&frame.agent_id) {
            // Signed diff of wrapping_sub: positive means `ts` is at
            // or ahead of the high-water mark (possibly via wrap);
            // negative means `ts` is in the past relative to it.
            let diff = frame.timestamp_ms.wrapping_sub(highest) as i32;
            if diff >= 0 {
                // Same or newer → accept, bump the high-water mark
                // on strictly-greater (stay stable on equal which
                // can only happen if ts came back via the ring once
                // it had dropped out — still fresh, don't regress).
                if diff > 0 {
                    let _ = self.highest_ts.insert(frame.agent_id, frame.timestamp_ms);
                }
                let verdict = self.seen.observe(key);
                debug_assert_eq!(verdict, Seen::Fresh);
                return ReplayVerdict::Fresh;
            }
            // `ts` is older than highest. Allow if within window,
            // otherwise reject outright.
            let behind = (-diff) as u32; // safe: diff < 0 so -diff > 0
            if behind <= self.window_ms {
                let verdict = self.seen.observe(key);
                debug_assert_eq!(verdict, Seen::Fresh);
                return ReplayVerdict::Fresh;
            }
            return ReplayVerdict::TooOld;
        }

        // ── New peer. Register or bail.
        if self.highest_ts.len() == M_PEERS {
            return ReplayVerdict::UnknownPeer;
        }
        // Unwrap is infallible: we just verified len < capacity.
        self.highest_ts
            .insert(frame.agent_id, frame.timestamp_ms)
            .ok();
        let verdict = self.seen.observe(key);
        debug_assert_eq!(verdict, Seen::Fresh);
        ReplayVerdict::Fresh
    }
}

impl<const N_SEEN: usize, const M_PEERS: usize> Default for ReplayGuard<N_SEEN, M_PEERS> {
    /// Default window: 60 seconds. A reasonable starting point for
    /// 10 Hz mesh traffic — overridable via [`ReplayGuard::new`].
    #[inline]
    fn default() -> Self {
        Self::new(60_000)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn f(agent_id: u32, ts: u32) -> SepFrame {
        SepFrame {
            agent_id,
            timestamp_ms: ts,
            ..SepFrame::ZERO
        }
    }

    // ── Single-peer, monotonic ──────────────────────────────────────

    #[test]
    fn first_frame_from_peer_is_fresh() {
        let mut g = ReplayGuard::<64, 16>::new(1_000);
        assert_eq!(g.check(&f(1, 100)), ReplayVerdict::Fresh);
        assert_eq!(g.tracked_peers(), 1);
        assert_eq!(g.highest_for(1), Some(100));
    }

    #[test]
    fn monotonic_increasing_all_fresh() {
        let mut g = ReplayGuard::<64, 16>::new(1_000);
        for ts in [10u32, 20, 30, 40, 50] {
            assert_eq!(g.check(&f(1, ts)), ReplayVerdict::Fresh);
        }
        assert_eq!(g.highest_for(1), Some(50));
    }

    #[test]
    fn literal_duplicate_in_ring_rejected() {
        let mut g = ReplayGuard::<64, 16>::new(1_000);
        assert_eq!(g.check(&f(1, 100)), ReplayVerdict::Fresh);
        assert_eq!(g.check(&f(1, 100)), ReplayVerdict::Duplicate);
        // Highest is unchanged.
        assert_eq!(g.highest_for(1), Some(100));
    }

    #[test]
    fn late_arrival_within_window_is_fresh() {
        let mut g = ReplayGuard::<64, 16>::new(1_000);
        g.check(&f(1, 5_000));
        // 500 ms late: within window → Fresh. Highest should NOT
        // regress (it stays at 5_000).
        assert_eq!(g.check(&f(1, 4_500)), ReplayVerdict::Fresh);
        assert_eq!(g.highest_for(1), Some(5_000));
    }

    #[test]
    fn late_arrival_past_window_is_too_old() {
        let mut g = ReplayGuard::<64, 16>::new(1_000);
        g.check(&f(1, 5_000));
        // 1_500 ms late: past window → TooOld. State unchanged.
        assert_eq!(g.check(&f(1, 3_500)), ReplayVerdict::TooOld);
        assert_eq!(g.highest_for(1), Some(5_000));
    }

    #[test]
    fn window_boundary_is_inclusive() {
        // Exactly `window_ms` behind the high-water mark → still
        // Fresh (inclusive boundary).
        let mut g = ReplayGuard::<64, 16>::new(1_000);
        g.check(&f(1, 5_000));
        assert_eq!(g.check(&f(1, 4_000)), ReplayVerdict::Fresh);
        assert_eq!(g.check(&f(1, 3_999)), ReplayVerdict::TooOld);
    }

    // ── Multi-peer ─────────────────────────────────────────────────

    #[test]
    fn independent_peers_each_track_their_own_high_water() {
        let mut g = ReplayGuard::<64, 16>::new(100);
        g.check(&f(1, 1_000));
        g.check(&f(2, 5_000));
        // Peer 1 at 900 ms: within window of its own high-water
        // (1_000) → Fresh. Peer 2 untouched.
        assert_eq!(g.check(&f(1, 900)), ReplayVerdict::Fresh);
        assert_eq!(g.highest_for(1), Some(1_000));
        assert_eq!(g.highest_for(2), Some(5_000));
    }

    #[test]
    fn full_peer_map_returns_unknown_peer() {
        // M_PEERS = 4 so we can fill it cheaply.
        let mut g = ReplayGuard::<64, 4>::new(100);
        for agent_id in [10u32, 11, 12, 13] {
            assert_eq!(g.check(&f(agent_id, 0)), ReplayVerdict::Fresh);
        }
        assert_eq!(g.tracked_peers(), 4);
        assert_eq!(g.check(&f(99, 0)), ReplayVerdict::UnknownPeer);
        // State unchanged.
        assert_eq!(g.tracked_peers(), 4);
        assert_eq!(g.highest_for(99), None);
    }

    // ── u32 wrap-around ────────────────────────────────────────────

    #[test]
    fn wrap_around_past_u32_max_is_fresh() {
        let mut g = ReplayGuard::<64, 16>::new(1_000);
        // Peer sends a frame very close to the u32::MAX ceiling.
        g.check(&f(1, u32::MAX - 10));
        // Clock wraps: next frame is ts = 5. Using signed
        // wrapping_sub, 5 - (u32::MAX - 10) = 16 (i32) — newer.
        assert_eq!(g.check(&f(1, 5)), ReplayVerdict::Fresh);
        assert_eq!(g.highest_for(1), Some(5));
    }

    #[test]
    fn wrap_around_late_arrival_still_respects_window() {
        let mut g = ReplayGuard::<64, 16>::new(1_000);
        g.check(&f(1, 500)); // recent-ish after a wrap
                             // Now a "late" frame from just before the wrap: ts = u32::MAX - 100.
                             // diff = (u32::MAX - 100).wrapping_sub(500) as i32
                             //      = wrapping_sub(u32::MAX - 100, 500)
                             //      = u32::MAX - 600
                             // as i32 = -601. behind = 601 ≤ 1000 → Fresh.
        assert_eq!(g.check(&f(1, u32::MAX - 100)), ReplayVerdict::Fresh);
        // High-water doesn't regress.
        assert_eq!(g.highest_for(1), Some(500));
    }

    // ── Constructor clamps ─────────────────────────────────────────

    #[test]
    fn oversize_window_is_clamped() {
        let g = ReplayGuard::<64, 16>::new(u32::MAX);
        assert_eq!(g.window_ms(), i32::MAX as u32);
    }

    // ── Proptest invariants ────────────────────────────────────────

    use proptest::prelude::*;

    proptest! {
        /// Invariant: for a strictly-monotonic sequence of
        /// timestamps from one peer, every frame is Fresh and the
        /// high-water mark tracks the latest.
        #[test]
        fn monotonic_sequence_all_fresh(
            start in 0u32..1_000_000,
            gaps in proptest::collection::vec(1u32..1_000, 1..16),
        ) {
            let mut g = ReplayGuard::<64, 16>::new(100);
            let mut ts = start;
            for gap in gaps {
                // Saturate to avoid wrap — this test is about the
                // monotonic path, not wrap semantics.
                ts = ts.saturating_add(gap);
                prop_assert_eq!(g.check(&f(42, ts)), ReplayVerdict::Fresh);
            }
            prop_assert_eq!(g.highest_for(42), Some(ts));
        }

        /// Invariant: re-submitting the *exact* last-fresh frame is
        /// always Duplicate (SeenSet picks it up).
        #[test]
        fn exact_duplicate_always_rejected(
            agent_id in any::<u32>(),
            ts in any::<u32>(),
        ) {
            let mut g = ReplayGuard::<64, 16>::new(1_000);
            prop_assert_eq!(g.check(&f(agent_id, ts)), ReplayVerdict::Fresh);
            prop_assert_eq!(g.check(&f(agent_id, ts)), ReplayVerdict::Duplicate);
        }

        /// Invariant: for a single peer, `TooOld` is stable — if a
        /// given `ts` is TooOld now, submitting it again is still
        /// TooOld (the high-water only moves forward, so the
        /// distance behind it cannot shrink). Shuffling arrivals of
        /// too-old candidates doesn't change the verdict.
        #[test]
        fn too_old_is_stable(
            high in 10_000u32..100_000,
            window in 100u32..1_000,
            lates in proptest::collection::vec(1u32..5_000, 1..8),
        ) {
            let mut g = ReplayGuard::<64, 16>::new(window);
            g.check(&f(1, high));
            for late in lates {
                // Late = how far behind the high-water mark.
                let ts = high.saturating_sub(late + window + 1);
                // Guaranteed behind `high` by > window → TooOld.
                prop_assert_eq!(g.check(&f(1, ts)), ReplayVerdict::TooOld);
                // Re-submission is still TooOld (state unchanged).
                prop_assert_eq!(g.check(&f(1, ts)), ReplayVerdict::TooOld);
            }
            // High-water hasn't regressed.
            prop_assert_eq!(g.highest_for(1), Some(high));
        }

        /// Invariant: Fresh-within-window marks the frame as
        /// contained in the seen ring (next identical submission is
        /// Duplicate, not Fresh again).
        #[test]
        fn within_window_late_then_duplicate(
            high in 10_000u32..100_000,
            window in 100u32..1_000,
            behind in 1u32..100,
        ) {
            let mut g = ReplayGuard::<64, 16>::new(window);
            g.check(&f(1, high));
            let ts = high - behind; // strictly within window
            prop_assert_eq!(g.check(&f(1, ts)), ReplayVerdict::Fresh);
            prop_assert_eq!(g.check(&f(1, ts)), ReplayVerdict::Duplicate);
        }
    }
}

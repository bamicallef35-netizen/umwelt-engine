//! Relay path: hop-limit enforcement + duplicate suppression (P2.3).
//!
//! Two concerns on the forwarding hot path, neither of which needs
//! to touch the codec:
//!
//! 1. **Hop limit.** Per ADR-0011, each frame carries a 4-bit
//!    `ttl` in the high nibble of byte 6. A relay decrements before
//!    forwarding and drops the frame when `ttl == 0` — so senders
//!    can budget reach deterministically.
//!
//! 2. **Duplicate suppression.** In a partition-tolerant mesh the
//!    same frame can reach a node via several paths; forwarding
//!    every copy is a broadcast storm. A small per-node seen-set
//!    keyed on `(agent_id, timestamp_ms)` (both globally unique
//!    for a given sign — `agent_id` is CRC32 of MAC, timestamp is
//!    agent-local monotonic ms) rejects recent duplicates.
//!
//! Everything here is `no_std`, allocation-free, and fixed-capacity
//! via a compile-time `N`. The seen-set is a FIFO ring that evicts
//! the oldest entry when full. `O(N)` membership; `N = 64` gives a
//! comfortable window for 10 Hz swarm chatter at sub-µs cost.

use crate::SepFrame;

// ── Hop limit ──────────────────────────────────────────────────────────

/// Outcome of a relay decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayAction {
    /// Frame should be re-encoded and forwarded. Its `ttl` has been
    /// decremented in place.
    Forward,
    /// Frame must not be forwarded — either its TTL expired, or the
    /// seen-set found it to be a recent duplicate.
    Drop,
}

/// Apply the hop-limit rule to `frame` per ADR-0011.
///
/// * If `ttl == 0` the frame is already at the end of its reach and
///   must be dropped unchanged (no decrement below zero).
/// * Otherwise `ttl` is decremented in place and the frame is
///   approved for forwarding.
///
/// Decrement-before-relay matches the ADR-0011 wording: a frame with
/// `ttl = 1` means "this is the last hop", so it should be processed
/// locally but not re-broadcast.
#[inline]
pub fn relay(frame: &mut SepFrame) -> RelayAction {
    if frame.ttl == 0 {
        return RelayAction::Drop;
    }
    frame.ttl -= 1;
    if frame.ttl == 0 {
        // Reached the end of its reach after this decrement —
        // process locally, don't re-broadcast.
        RelayAction::Drop
    } else {
        RelayAction::Forward
    }
}

// ── Duplicate suppression ──────────────────────────────────────────────

/// Key used to uniquify a sign across the mesh.
///
/// `(agent_id, timestamp_ms)` is sufficient because `agent_id` is a
/// per-node CRC32 of MAC and `timestamp_ms` is the emitting agent's
/// monotonic clock — the same sign re-broadcast by multiple relays
/// retains the original emitter's fields.
pub type SeenKey = (u32, u32);

/// Observation verdict returned by [`SeenSet::observe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    /// Not in the window. The key has been recorded; caller may
    /// forward the frame.
    Fresh,
    /// Already in the window. The caller must drop the frame; the
    /// window is unchanged.
    Duplicate,
}

/// Fixed-capacity FIFO ring of recently-seen `(agent_id, timestamp)`
/// pairs.
///
/// When full, a new insertion evicts the oldest entry — bounded
/// memory, bounded time, no allocation. `N` is compile-time.
///
/// Membership check is O(N) linear scan; at `N = 64` and the
/// 10 Hz–100 Hz frame rates we plan for, this is 640–6 400
/// comparisons/second per neighbour — negligible even on Cortex-M0.
#[derive(Debug)]
pub struct SeenSet<const N: usize> {
    ring: heapless::Deque<SeenKey, N>,
}

impl<const N: usize> Default for SeenSet<N> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> SeenSet<N> {
    /// Construct an empty ring. `const` so it can live in a `static`.
    #[inline]
    pub const fn new() -> Self {
        Self {
            ring: heapless::Deque::new(),
        }
    }

    /// Current entry count (0..=N).
    #[inline]
    pub fn len(&self) -> usize {
        self.ring.len()
    }

    /// `true` if the ring holds nothing.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    /// Maximum entry count — compile-time `N`.
    #[inline]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// `true` if `key` is somewhere in the window.
    ///
    /// Pure query — does not mutate the ring.
    pub fn contains(&self, key: &SeenKey) -> bool {
        self.ring.iter().any(|k| k == key)
    }

    /// Observe `key`: if it is already in the window, returns
    /// [`Seen::Duplicate`] and leaves the window unchanged; otherwise
    /// records the key (evicting the oldest entry if at capacity) and
    /// returns [`Seen::Fresh`].
    ///
    /// This is the hot-path API — checking and recording in one pass
    /// keeps the ring consistent without exposing the eviction point
    /// to callers.
    pub fn observe(&mut self, key: SeenKey) -> Seen {
        if self.contains(&key) {
            return Seen::Duplicate;
        }
        // At capacity: evict the oldest entry to make room.
        if self.ring.len() == N {
            // Unwrap is safe: len == N > 0 because N = 0 would make
            // contains() vacuously false and every key Fresh; we'd
            // never reach this branch. For N = 0 observe() just
            // always returns Fresh and records nothing (push_back
            // fails silently below).
            let _ = self.ring.pop_front();
        }
        // push_back fails only if the deque is full — we just made
        // room above, except in the N = 0 degenerate case where we
        // quietly drop the record. That matches the user's intent
        // ("no window, nothing remembered").
        let _ = self.ring.push_back(key);
        Seen::Fresh
    }
}

/// Convenience: extract the [`SeenKey`] for a frame.
#[inline]
pub const fn seen_key(frame: &SepFrame) -> SeenKey {
    (frame.agent_id, frame.timestamp_ms)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SepFrame;

    fn frame_with(agent_id: u32, timestamp_ms: u32, ttl: u8) -> SepFrame {
        SepFrame {
            agent_id,
            timestamp_ms,
            ttl,
            ..SepFrame::ZERO
        }
    }

    // ── relay() ────────────────────────────────────────────────────────

    #[test]
    fn relay_drops_ttl_zero_without_underflow() {
        let mut f = frame_with(1, 1, 0);
        assert_eq!(relay(&mut f), RelayAction::Drop);
        // ttl untouched: no underflow into 0xFF.
        assert_eq!(f.ttl, 0);
    }

    #[test]
    fn relay_decrements_and_forwards_middle_ttl() {
        let mut f = frame_with(1, 1, 5);
        assert_eq!(relay(&mut f), RelayAction::Forward);
        assert_eq!(f.ttl, 4);
    }

    #[test]
    fn relay_decrements_ttl_one_to_zero_and_drops() {
        // ttl == 1 means "this is the last hop"; process locally,
        // don't re-broadcast.
        let mut f = frame_with(1, 1, 1);
        assert_eq!(relay(&mut f), RelayAction::Drop);
        assert_eq!(f.ttl, 0);
    }

    #[test]
    fn relay_preserves_all_other_fields() {
        let mut f = SepFrame {
            agent_id: 0xCAFEBABE,
            sign_type: 7,
            magnitude: 128,
            ttl: 3,
            flags: 0b0011,
            spatial: [1, 2, 3],
            timestamp_ms: 42,
            confidence: 200,
            priority: 50,
        };
        let before = f;
        assert_eq!(relay(&mut f), RelayAction::Forward);
        assert_eq!(f.agent_id, before.agent_id);
        assert_eq!(f.sign_type, before.sign_type);
        assert_eq!(f.magnitude, before.magnitude);
        assert_eq!(f.flags, before.flags);
        assert_eq!(f.spatial, before.spatial);
        assert_eq!(f.timestamp_ms, before.timestamp_ms);
        assert_eq!(f.confidence, before.confidence);
        assert_eq!(f.priority, before.priority);
        assert_eq!(f.ttl, before.ttl - 1);
    }

    #[test]
    fn relay_max_ttl_decrements_normally() {
        let mut f = frame_with(1, 1, 15);
        assert_eq!(relay(&mut f), RelayAction::Forward);
        assert_eq!(f.ttl, 14);
    }

    // ── SeenSet ────────────────────────────────────────────────────────

    #[test]
    fn seenset_fresh_then_duplicate() {
        let mut s = SeenSet::<8>::new();
        assert_eq!(s.observe((1, 100)), Seen::Fresh);
        assert_eq!(s.observe((1, 100)), Seen::Duplicate);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn seenset_distinct_keys_are_each_fresh() {
        let mut s = SeenSet::<8>::new();
        assert_eq!(s.observe((1, 100)), Seen::Fresh);
        assert_eq!(s.observe((1, 101)), Seen::Fresh);
        assert_eq!(s.observe((2, 100)), Seen::Fresh);
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn seenset_wraps_and_evicts_oldest() {
        let mut s = SeenSet::<3>::new();
        s.observe((1, 1));
        s.observe((1, 2));
        s.observe((1, 3));
        assert_eq!(s.len(), 3);
        // Fourth insert evicts (1,1).
        assert_eq!(s.observe((1, 4)), Seen::Fresh);
        assert!(!s.contains(&(1, 1)), "oldest must have been evicted");
        assert!(s.contains(&(1, 2)));
        assert!(s.contains(&(1, 3)));
        assert!(s.contains(&(1, 4)));
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn seenset_evicted_key_is_fresh_again() {
        let mut s = SeenSet::<2>::new();
        s.observe((1, 1));
        s.observe((1, 2));
        s.observe((1, 3)); // evicts (1,1)
                           // (1,1) is now outside the window → treated as fresh.
        assert_eq!(s.observe((1, 1)), Seen::Fresh);
    }

    #[test]
    fn seenset_contains_does_not_mutate() {
        let mut s = SeenSet::<4>::new();
        s.observe((1, 1));
        let len_before = s.len();
        assert!(s.contains(&(1, 1)));
        assert!(!s.contains(&(9, 9)));
        assert_eq!(s.len(), len_before);
    }

    #[test]
    fn seenset_capacity_and_is_empty() {
        let s = SeenSet::<16>::new();
        assert_eq!(s.capacity(), 16);
        assert!(s.is_empty());
    }

    #[test]
    fn seen_key_extracts_agent_and_timestamp() {
        let f = frame_with(42, 1234, 5);
        assert_eq!(seen_key(&f), (42, 1234));
    }

    // ── Proptest invariants ────────────────────────────────────────────

    use proptest::prelude::*;

    proptest! {
        /// Invariant: calling `relay` on a `ttl=0` frame is idempotent
        /// — the frame cannot escape the drop state, and the ttl
        /// field never underflows.
        #[test]
        fn relay_is_idempotent_on_ttl_zero(
            agent_id in any::<u32>(),
            timestamp_ms in any::<u32>(),
        ) {
            let mut f = frame_with(agent_id, timestamp_ms, 0);
            for _ in 0..8 {
                prop_assert_eq!(relay(&mut f), RelayAction::Drop);
                prop_assert_eq!(f.ttl, 0);
            }
        }

        /// Invariant: a frame entering with `ttl = n > 0` takes
        /// exactly `n-1` successful Forward decisions before a
        /// terminal Drop on the n-th call.
        #[test]
        fn relay_walks_ttl_down_to_zero_in_exact_steps(
            ttl in 1u8..=15,
        ) {
            let mut f = frame_with(0, 0, ttl);
            for expected in (1..ttl).rev() {
                prop_assert_eq!(relay(&mut f), RelayAction::Forward);
                prop_assert_eq!(f.ttl, expected);
            }
            // Final step takes ttl 1 → 0 and returns Drop.
            prop_assert_eq!(relay(&mut f), RelayAction::Drop);
            prop_assert_eq!(f.ttl, 0);
        }

        /// Invariant: set-level membership is insensitive to the
        /// order in which distinct keys are inserted — as long as
        /// the number of distinct keys is within capacity, a shuffled
        /// insertion yields the same `contains` profile.
        #[test]
        fn seenset_membership_is_order_insensitive(
            // Up to 8 distinct keys, insertion order permuted by `rot`.
            base in proptest::collection::vec(
                (0u32..32, 0u32..32), 1..=8
            ).prop_filter("keys must be distinct", |v| {
                let mut seen = heapless::Vec::<(u32, u32), 8>::new();
                for &k in v.iter() {
                    if seen.iter().any(|x| x == &k) { return false; }
                    let _ = seen.push(k);
                }
                true
            }),
            rot in 0usize..8,
        ) {
            let mut a = SeenSet::<16>::new();
            let mut b = SeenSet::<16>::new();
            for &k in &base {
                a.observe(k);
            }
            let n = base.len();
            let r = if n == 0 { 0 } else { rot % n };
            for i in 0..n {
                b.observe(base[(i + r) % n]);
            }
            for &k in &base {
                prop_assert_eq!(a.contains(&k), b.contains(&k));
            }
            prop_assert_eq!(a.len(), b.len());
        }

        /// Invariant: a freshly-inserted key is always contained
        /// immediately after `observe` returns `Fresh`.
        #[test]
        fn seenset_fresh_key_is_contained_after_insert(
            k in any::<(u32, u32)>(),
        ) {
            let mut s = SeenSet::<4>::new();
            prop_assert_eq!(s.observe(k), Seen::Fresh);
            prop_assert!(s.contains(&k));
        }
    }
}

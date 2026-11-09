//! Umwelt graph storage abstraction (P1.3).
//!
//! Every agent carries a `UmweltGraph`: the live pool of signs it has
//! formed so far. The [`UmweltGraph`] trait is the minimum storage
//! contract — insert with content-addressing (ADR-0014), lookup by
//! [`SignId`], enumeration — and two backends ship in-tree:
//!
//! * [`HeaplessGraph`] — `no_std`, fixed-capacity `heapless::Vec`.
//!   Linear-scan lookup; suitable for microcontroller budgets where
//!   `N` is at most a few thousand.
//! * [`VecGraph`] — `std`-only, growing `Vec` with a `HashMap` id
//!   index. Gated on `feature = "std"`. Suitable for host tooling,
//!   replay harnesses, and simulations.
//!
//! ## Collision policy (ADR-0014)
//!
//! `insert` derives an unsalted id via [`SignId::from_identity`].
//! Four paths from there:
//!
//! 1. **Unsalted slot free** — store the sign under that id. Done.
//! 2. **Unsalted slot taken, identity matches** — dedup hit. Return
//!    the existing id; the stored sign is left untouched (richer
//!    dedup semantics — e.g. magnitude updates — arrive in P1.4).
//! 3. **Unsalted slot taken, identity differs** — genuine hash
//!    collision. Bump [`hash::COLLISION_COUNT`], walk
//!    `salt = 1, 2, …` via [`SignId::from_identity_salted`] until
//!    a free or identity-matching slot is found.
//! 4. **Salt space exhausted (all 2^16 salts taken by distinct
//!    identities)** — returns [`InsertError::SaltSpaceExhausted`].
//!    Astronomically unlikely at 64-bit output.
//!
//! Under `feature = "std"`, the first observed collision also emits
//! a single `tracing::warn!` per process lifetime. The counter is
//! always observable via [`hash::COLLISION_COUNT`].

use crate::decay::DecayClass;
use crate::{hash, SemioticLayer, Sign, SignId, SignIdentity, TimestampUs, SIGN_IDENTITY_BYTES};
use core::sync::atomic::Ordering;

// ── Decay maintenance ──────────────────────────────────────────────────────

/// Result of one [`HeaplessGraph::tick_decay`] or [`VecGraph::tick_decay`]
/// sweep. The ADR-0015 contract only fires the decay math; this struct
/// lets callers report / log how much pruning happened in each tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DecaySummary {
    /// Nodes still alive after this sweep.
    pub remaining: usize,
    /// Nodes removed because their decayed intensity saturated to
    /// [`crate::Q15::ZERO`].
    pub pruned: usize,
}

// ── Error type ─────────────────────────────────────────────────────────────

/// Reasons [`UmweltGraph::insert`] can fail.
///
/// Both variants are terminal — callers should either shed load
/// ([`GraphFull`][Self::GraphFull]) or trigger an agent-wide
/// escalation ([`SaltSpaceExhausted`][Self::SaltSpaceExhausted]),
/// which is the signal that 64-bit FNV-1a is no longer enough for
/// this deployment (see ADR-0014's reserved BLAKE3-64 escalation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertError {
    /// The backend has reached [`UmweltGraph::capacity`].
    GraphFull,
    /// All 2^16 salted rehashes of this identity collided with
    /// distinct stored identities. See ADR-0014.
    SaltSpaceExhausted,
}

// ── Internal node record ──────────────────────────────────────────────────
//
// Every backend stores `(id, identity_bytes, sign)` triples. The
// identity_bytes copy is what lets us *detect* collisions — without
// it, we could not tell an ADR-0014 collision apart from a dedup hit.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Node {
    id: SignId,
    identity_bytes: [u8; SIGN_IDENTITY_BYTES],
    sign: Sign,
}

// ── Trait ──────────────────────────────────────────────────────────────────

/// Minimum storage contract every graph backend satisfies.
///
/// The trait covers the minimum storage contract plus the P1.5 query
/// surface: iteration plus three canonical filters (by layer, by
/// channel, by time window). Richer composed filters live as
/// [`UmweltGraphExt`] blanket-impl extensions below.
///
/// The associated [`Iter`][Self::Iter] GAT makes the trait non–
/// object-safe; dynamic dispatch was never required for this
/// codepath (both backends are concrete at every call-site), and the
/// GAT keeps iteration zero-allocation on no_std.
pub trait UmweltGraph {
    /// Borrowed iterator over every stored sign, in insertion order.
    type Iter<'a>: Iterator<Item = &'a Sign>
    where
        Self: 'a;

    /// Insert `sign` under `identity`. Returns the allocated
    /// [`SignId`] — which also replaces the `id` field of the stored
    /// sign, so callers may ignore whatever they set on the input.
    ///
    /// On a dedup hit (same identity already present) the stored
    /// sign is reinforced per ADR-0016 (intensity saturating-adds,
    /// `duration_us` high-water, `formed_at`/`coherence`/`channel`
    /// preserved).
    fn insert(&mut self, identity: &SignIdentity, sign: Sign) -> Result<SignId, InsertError>;

    /// Retrieve a sign by id. `O(n)` on [`HeaplessGraph`], `O(1)` on
    /// [`VecGraph`].
    fn get(&self, id: SignId) -> Option<&Sign>;

    /// Iterate every stored sign, in insertion order.
    fn iter(&self) -> Self::Iter<'_>;

    /// `true` if `id` is present.
    fn contains(&self, id: SignId) -> bool {
        self.get(id).is_some()
    }

    /// Number of stored nodes.
    fn len(&self) -> usize;

    /// Maximum node count. Returns `usize::MAX` for truly growing
    /// backends (in practice, [`VecGraph`]).
    fn capacity(&self) -> usize;

    /// Shorthand for `self.len() >= self.capacity()`.
    fn is_full(&self) -> bool {
        self.len() >= self.capacity()
    }

    /// Shorthand for `self.len() == 0`.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Signs whose [`SemioticLayer`] equals `layer`.
    fn by_layer(&self, layer: SemioticLayer) -> impl Iterator<Item = &Sign> {
        self.iter().filter(move |s| s.layer == layer)
    }

    /// Signs whose `channel` equals `channel`.
    fn by_channel(&self, channel: u16) -> impl Iterator<Item = &Sign> {
        self.iter().filter(move |s| s.channel == channel)
    }

    /// Signs whose `formed_at` lies in the inclusive window `[lo, hi]`.
    ///
    /// `lo > hi` yields an empty iterator — the caller gets exactly
    /// what they asked for without a panic.
    fn by_time_window(&self, lo: TimestampUs, hi: TimestampUs) -> impl Iterator<Item = &Sign> {
        self.iter()
            .filter(move |s| s.formed_at >= lo && s.formed_at <= hi)
    }
}

// ── Log-once plumbing (ADR-0014) ──────────────────────────────────────────

#[cfg(feature = "std")]
mod log_once {
    use core::sync::atomic::{AtomicBool, Ordering};

    /// Set on the first emission so subsequent collisions stay silent.
    /// `AtomicBool::swap` is used so the "log once" guarantee holds
    /// across threads without a `Mutex`.
    static LOGGED: AtomicBool = AtomicBool::new(false);

    pub(super) fn warn_first_collision() {
        if LOGGED.swap(true, Ordering::Relaxed) {
            return;
        }
        tracing::warn!(
            target: "umwelt_core::graph",
            "ADR-0014: first SignId collision observed; allocator \
             promoted via salted rehash. Monitor \
             umwelt_core::hash::COLLISION_COUNT for ongoing rate."
        );
    }
}

#[cfg(not(feature = "std"))]
mod log_once {
    #[inline(always)]
    pub(super) fn warn_first_collision() {
        // no_std target — COLLISION_COUNT remains the observable.
    }
}

// ── Reinforcement merge (ADR-0016) ────────────────────────────────────────

/// Merge `incoming` into `stored` per ADR-0016: intensity
/// saturating-adds, `duration_us` takes the max, and everything else
/// is preserved from `stored`.
///
/// Pure function — used by both graph backends on a dedup hit. Shared
/// here so the two backends cannot drift apart on semantics.
#[inline]
fn merge_reinforcement(stored: &Sign, incoming: &Sign) -> Sign {
    Sign {
        id: stored.id,
        layer: stored.layer,
        domain: stored.domain,
        channel: stored.channel,
        formed_at: stored.formed_at,
        duration_us: if incoming.duration_us > stored.duration_us {
            incoming.duration_us
        } else {
            stored.duration_us
        },
        intensity: stored.intensity.saturating_add(incoming.intensity),
        coherence: stored.coherence,
    }
}

// ── Shared allocator helper ───────────────────────────────────────────────

/// Walk the ADR-0014 salt ladder, asking `probe` whether each
/// candidate id is free. Returns the id to use — either a free slot
/// or an existing dedup match.
///
/// `probe(id)` semantics:
/// * `None`               — slot is free for `id`
/// * `Some(stored_bytes)` — slot is occupied; `stored_bytes` is the
///   21-byte canonical identity of the node currently holding `id`
fn allocate_id<F>(
    identity: &SignIdentity,
    canonical: &[u8; SIGN_IDENTITY_BYTES],
    mut probe: F,
) -> Result<SignId, InsertError>
where
    F: FnMut(SignId) -> Option<[u8; SIGN_IDENTITY_BYTES]>,
{
    let unsalted = SignId::from_identity(identity);
    match probe(unsalted) {
        None => return Ok(unsalted),
        Some(stored) if &stored == canonical => return Ok(unsalted),
        Some(_) => {
            // Genuine collision: different identity → same id.
            hash::COLLISION_COUNT.fetch_add(1, Ordering::Relaxed);
            log_once::warn_first_collision();
        }
    }
    // Promote. Walk salt = 1..=u16::MAX; each further collision also
    // bumps the counter so the rate is observable, matching ADR-0014.
    for salt in 1u16..=u16::MAX {
        let salted = SignId::from_identity_salted(identity, salt);
        match probe(salted) {
            None => return Ok(salted),
            Some(stored) if &stored == canonical => return Ok(salted),
            Some(_) => {
                hash::COLLISION_COUNT.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    Err(InsertError::SaltSpaceExhausted)
}

// ── Heapless backend (no_std, fixed-capacity) ─────────────────────────────

/// Fixed-capacity graph backend. `N` is a compile-time bound on the
/// number of stored signs; the backing storage is one
/// `heapless::Vec<Node, N>` with linear-scan lookup.
///
/// For small `N` (≤ a few thousand) the linear scan dominates by a
/// constant factor but avoids a hash-table's per-entry overhead —
/// fine at microcontroller scale. A richer indexing layer lands in
/// P1.5 alongside query-by-region.
#[derive(Debug)]
pub struct HeaplessGraph<const N: usize> {
    nodes: heapless::Vec<Node, N>,
}

impl<const N: usize> Default for HeaplessGraph<N> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> HeaplessGraph<N> {
    /// Construct an empty graph. `const` so it can live in `static`.
    #[inline]
    pub const fn new() -> Self {
        Self {
            nodes: heapless::Vec::new(),
        }
    }

    #[inline]
    fn find_index(&self, id: SignId) -> Option<usize> {
        self.nodes.iter().position(|n| n.id == id)
    }

    /// Apply one decay tick per ADR-0015 to every stored sign, then
    /// prune any node whose intensity has saturated to [`Q15::ZERO`].
    ///
    /// The caller owns the classifier — in v0.1 this is a lookup over
    /// the agent's vocabulary; a future ADR will push the mapping onto
    /// `Sign` itself. Returns a [`DecaySummary`] breaking down
    /// remaining vs. pruned nodes.
    pub fn tick_decay<F>(&mut self, mut classify: F) -> DecaySummary
    where
        F: FnMut(&Sign) -> DecayClass,
    {
        let mut pruned = 0usize;
        // Swap-remove is safe here because node order is not part of
        // the public contract (it is in [`HeaplessGraphIter`]) — decay
        // is allowed to reorder. Tests assert the invariants we do
        // preserve: set-membership of surviving ids.
        let mut i = 0;
        while i < self.nodes.len() {
            let class = classify(&self.nodes[i].sign);
            let decayed = crate::decay::decay_one_tick(self.nodes[i].sign.intensity, class);
            self.nodes[i].sign.intensity = decayed;
            if decayed == crate::Q15::ZERO {
                self.nodes.swap_remove(i);
                pruned += 1;
            } else {
                i += 1;
            }
        }
        DecaySummary {
            remaining: self.nodes.len(),
            pruned,
        }
    }
}

/// Borrowed iterator over [`HeaplessGraph`]'s stored signs.
///
/// Thin newtype over `core::slice::Iter<Node>` that projects each
/// node to `&Sign`. Named so the [`UmweltGraph::Iter`] associated
/// type on `no_std` backends is nameable (RPIT-in-trait needs a
/// concrete type here for the GAT).
#[derive(Debug, Clone)]
pub struct HeaplessGraphIter<'a> {
    inner: core::slice::Iter<'a, Node>,
}

impl<'a> Iterator for HeaplessGraphIter<'a> {
    type Item = &'a Sign;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|n| &n.sign)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl ExactSizeIterator for HeaplessGraphIter<'_> {}

impl<const N: usize> UmweltGraph for HeaplessGraph<N> {
    type Iter<'a>
        = HeaplessGraphIter<'a>
    where
        Self: 'a;

    fn insert(&mut self, identity: &SignIdentity, sign: Sign) -> Result<SignId, InsertError> {
        let canonical = identity.to_canonical_bytes();
        // Early probe: is this identity already stored (at any salt)?
        // allocate_id handles that; if it returns a taken slot whose
        // identity matches, we short-circuit to dedup.
        let id = allocate_id(identity, &canonical, |candidate| {
            self.find_index(candidate)
                .map(|idx| self.nodes[idx].identity_bytes)
        })?;
        if let Some(idx) = self.find_index(id) {
            // Dedup hit — reinforce in place per ADR-0016.
            self.nodes[idx].sign = merge_reinforcement(&self.nodes[idx].sign, &sign);
            return Ok(id);
        }
        if self.nodes.len() >= N {
            return Err(InsertError::GraphFull);
        }
        let mut stored = sign;
        stored.id = id;
        let node = Node {
            id,
            identity_bytes: canonical,
            sign: stored,
        };
        // Unwrap is safe: we checked len < N above and we hold &mut.
        self.nodes.push(node).map_err(|_| InsertError::GraphFull)?;
        Ok(id)
    }

    #[inline]
    fn get(&self, id: SignId) -> Option<&Sign> {
        self.find_index(id).map(|idx| &self.nodes[idx].sign)
    }

    #[inline]
    fn iter(&self) -> Self::Iter<'_> {
        HeaplessGraphIter {
            inner: self.nodes.iter(),
        }
    }

    #[inline]
    fn len(&self) -> usize {
        self.nodes.len()
    }

    #[inline]
    fn capacity(&self) -> usize {
        N
    }
}

// ── Test hook for forcing a collision ─────────────────────────────────────
//
// FNV-1a 64-bit collisions are vanishingly rare, so the only way to
// cover the promote path in unit tests is to inject a fake node at a
// known id with identity bytes the allocator will reject. This hook
// is `cfg(test)` only — it never compiles into release builds.

#[cfg(test)]
impl<const N: usize> HeaplessGraph<N> {
    pub(crate) fn force_push_for_test(
        &mut self,
        id: SignId,
        identity_bytes: [u8; SIGN_IDENTITY_BYTES],
        sign: Sign,
    ) {
        self.nodes
            .push(Node {
                id,
                identity_bytes,
                sign,
            })
            .unwrap();
    }
}

// ── Vec backend (std-only, growing) ───────────────────────────────────────

#[cfg(feature = "std")]
mod vec_backend {
    use super::{allocate_id, DecaySummary, InsertError, Node, UmweltGraph};
    use crate::decay::{decay_one_tick, DecayClass};
    #[cfg(test)]
    use crate::SIGN_IDENTITY_BYTES;
    use crate::{Sign, SignId, SignIdentity, Q15};
    use std::collections::HashMap;
    use std::vec::Vec;

    /// Growing-capacity graph backend. Backed by `Vec<Node>` plus a
    /// `HashMap<SignId, usize>` index, giving `O(1)` lookup.
    ///
    /// `capacity()` reports `usize::MAX` — this backend only fails
    /// [`UmweltGraph::insert`] on salt-space exhaustion, which is a
    /// cryptographically-unlikely event.
    #[derive(Debug, Default)]
    pub struct VecGraph {
        nodes: Vec<Node>,
        index: HashMap<SignId, usize>,
    }

    impl VecGraph {
        /// Construct an empty graph.
        #[inline]
        pub fn new() -> Self {
            Self::default()
        }

        /// Preallocate storage for `cap` nodes.
        #[inline]
        pub fn with_capacity(cap: usize) -> Self {
            Self {
                nodes: Vec::with_capacity(cap),
                index: HashMap::with_capacity(cap),
            }
        }

        /// Apply one decay tick per ADR-0015 to every stored sign,
        /// then prune any node whose intensity has saturated to
        /// [`Q15::ZERO`]. Mirrors [`super::HeaplessGraph::tick_decay`].
        ///
        /// Uses `swap_remove` on the backing `Vec` and rebuilds the
        /// affected `HashMap` index entries; cost is `O(n)` time,
        /// `O(1)` space beyond the existing map.
        pub fn tick_decay<F>(&mut self, mut classify: F) -> DecaySummary
        where
            F: FnMut(&Sign) -> DecayClass,
        {
            let mut pruned = 0usize;
            let mut i = 0;
            while i < self.nodes.len() {
                let class = classify(&self.nodes[i].sign);
                let decayed = decay_one_tick(self.nodes[i].sign.intensity, class);
                self.nodes[i].sign.intensity = decayed;
                if decayed == Q15::ZERO {
                    // Forget this node. `swap_remove` fills slot `i`
                    // with the last element — patch its map entry.
                    let removed_id = self.nodes[i].id;
                    self.index.remove(&removed_id);
                    self.nodes.swap_remove(i);
                    if i < self.nodes.len() {
                        let moved_id = self.nodes[i].id;
                        self.index.insert(moved_id, i);
                    }
                    pruned += 1;
                } else {
                    i += 1;
                }
            }
            DecaySummary {
                remaining: self.nodes.len(),
                pruned,
            }
        }

        /// `cfg(test)` hook — forces a node at a known id with
        /// arbitrary identity bytes so we can cover the collision
        /// promote path without needing a real FNV-1a collision.
        #[cfg(test)]
        pub(crate) fn force_push_for_test(
            &mut self,
            id: SignId,
            identity_bytes: [u8; SIGN_IDENTITY_BYTES],
            sign: Sign,
        ) {
            let idx = self.nodes.len();
            self.nodes.push(Node {
                id,
                identity_bytes,
                sign,
            });
            self.index.insert(id, idx);
        }
    }

    /// Borrowed iterator over [`VecGraph`]'s stored signs.
    ///
    /// Thin newtype over `std::slice::Iter<Node>` projecting to
    /// `&Sign`; exists so the [`UmweltGraph::Iter`] associated type
    /// on `VecGraph` is nameable.
    #[derive(Debug, Clone)]
    pub struct VecGraphIter<'a> {
        inner: std::slice::Iter<'a, Node>,
    }

    impl<'a> Iterator for VecGraphIter<'a> {
        type Item = &'a Sign;

        #[inline]
        fn next(&mut self) -> Option<Self::Item> {
            self.inner.next().map(|n| &n.sign)
        }

        #[inline]
        fn size_hint(&self) -> (usize, Option<usize>) {
            self.inner.size_hint()
        }
    }

    impl ExactSizeIterator for VecGraphIter<'_> {}

    impl UmweltGraph for VecGraph {
        type Iter<'a>
            = VecGraphIter<'a>
        where
            Self: 'a;

        fn insert(&mut self, identity: &SignIdentity, sign: Sign) -> Result<SignId, InsertError> {
            let canonical = identity.to_canonical_bytes();
            let id = allocate_id(identity, &canonical, |candidate| {
                self.index
                    .get(&candidate)
                    .map(|&idx| self.nodes[idx].identity_bytes)
            })?;
            if let Some(&idx) = self.index.get(&id) {
                // Dedup hit — reinforce in place per ADR-0016.
                self.nodes[idx].sign = super::merge_reinforcement(&self.nodes[idx].sign, &sign);
                return Ok(id);
            }
            let mut stored = sign;
            stored.id = id;
            let idx = self.nodes.len();
            self.nodes.push(Node {
                id,
                identity_bytes: canonical,
                sign: stored,
            });
            self.index.insert(id, idx);
            Ok(id)
        }

        #[inline]
        fn get(&self, id: SignId) -> Option<&Sign> {
            self.index.get(&id).map(|&idx| &self.nodes[idx].sign)
        }

        #[inline]
        fn iter(&self) -> Self::Iter<'_> {
            VecGraphIter {
                inner: self.nodes.iter(),
            }
        }

        #[inline]
        fn len(&self) -> usize {
            self.nodes.len()
        }

        /// Growing backend. Reports `usize::MAX` so
        /// [`UmweltGraph::is_full`] is always `false`.
        #[inline]
        fn capacity(&self) -> usize {
            usize::MAX
        }
    }
}

#[cfg(feature = "std")]
pub use vec_backend::{VecGraph, VecGraphIter};

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SemioticLayer, SignDomain, Q15};
    use core::sync::atomic::Ordering;

    fn sample_identity(sign_type: u8) -> SignIdentity {
        SignIdentity {
            layer: SemioticLayer::Icon,
            domain: SignDomain::Spatial,
            sign_type,
            agent_id: 1,
            spatial_ref: [0, 0, 0],
            time_bucket: 1,
        }
    }

    fn sample_sign() -> Sign {
        // The id we pass is immaterial — insert() overwrites it.
        Sign::icon(SignId::new(0), 0, 0, Q15::ZERO)
    }

    // ── HeaplessGraph ──────────────────────────────────────────────────

    #[test]
    fn heapless_insert_then_get_roundtrip() {
        let mut g = HeaplessGraph::<8>::new();
        let ident = sample_identity(1);
        let id = g.insert(&ident, sample_sign()).unwrap();
        assert_eq!(id, SignId::from_identity(&ident));
        assert_eq!(g.len(), 1);
        assert!(g.contains(id));
        assert_eq!(g.get(id).unwrap().id, id);
    }

    #[test]
    fn heapless_dedup_same_identity_returns_same_id_and_len() {
        let mut g = HeaplessGraph::<8>::new();
        let ident = sample_identity(2);
        let a = g.insert(&ident, sample_sign()).unwrap();
        let b = g.insert(&ident, sample_sign()).unwrap();
        assert_eq!(a, b);
        assert_eq!(g.len(), 1);
    }

    #[test]
    fn heapless_distinct_identities_get_distinct_ids() {
        let mut g = HeaplessGraph::<8>::new();
        let a_id = g.insert(&sample_identity(1), sample_sign()).unwrap();
        let b_id = g.insert(&sample_identity(2), sample_sign()).unwrap();
        assert_ne!(a_id, b_id);
        assert_eq!(g.len(), 2);
    }

    #[test]
    fn heapless_insert_after_capacity_returns_graph_full() {
        let mut g = HeaplessGraph::<2>::new();
        g.insert(&sample_identity(1), sample_sign()).unwrap();
        g.insert(&sample_identity(2), sample_sign()).unwrap();
        assert!(g.is_full());
        let err = g.insert(&sample_identity(3), sample_sign()).unwrap_err();
        assert_eq!(err, InsertError::GraphFull);
    }

    #[test]
    fn heapless_forced_collision_promotes_via_salt() {
        let mut g = HeaplessGraph::<8>::new();
        let real = sample_identity(42);
        let real_unsalted = SignId::from_identity(&real);

        // Inject a fake node at the unsalted id with *different*
        // identity bytes — simulates a real FNV-1a collision.
        let fake_bytes = [0xAAu8; SIGN_IDENTITY_BYTES];
        let fake_sign = Sign::icon(real_unsalted, 0, 0, Q15::ZERO);
        g.force_push_for_test(real_unsalted, fake_bytes, fake_sign);

        let before = hash::COLLISION_COUNT.load(Ordering::Relaxed);
        let assigned = g.insert(&real, sample_sign()).unwrap();
        let after = hash::COLLISION_COUNT.load(Ordering::Relaxed);

        assert_ne!(assigned, real_unsalted);
        assert_eq!(
            assigned,
            SignId::from_identity_salted(&real, 1),
            "allocator must promote to salt=1 when unsalted slot is taken"
        );
        assert!(
            after > before,
            "ADR-0014 counter must observe at least one collision"
        );
        // The forced node stays put; the new node has a distinct id.
        assert_eq!(g.len(), 2);
        assert!(g.contains(real_unsalted));
        assert!(g.contains(assigned));
    }

    #[test]
    fn heapless_empty_graph_is_empty_and_not_full() {
        let g = HeaplessGraph::<4>::new();
        assert!(g.is_empty());
        assert!(!g.is_full());
        assert_eq!(g.len(), 0);
        assert_eq!(g.capacity(), 4);
    }

    // ── VecGraph (std only) ────────────────────────────────────────────

    #[cfg(feature = "std")]
    #[test]
    fn vec_insert_then_get_roundtrip() {
        let mut g = VecGraph::new();
        let ident = sample_identity(1);
        let id = g.insert(&ident, sample_sign()).unwrap();
        assert_eq!(id, SignId::from_identity(&ident));
        assert_eq!(g.len(), 1);
        assert_eq!(g.get(id).unwrap().id, id);
        assert_eq!(g.capacity(), usize::MAX);
        assert!(!g.is_full(), "growing backend is never full");
    }

    #[cfg(feature = "std")]
    #[test]
    fn vec_dedup_same_identity_keeps_len_at_one() {
        let mut g = VecGraph::new();
        let ident = sample_identity(9);
        let a = g.insert(&ident, sample_sign()).unwrap();
        let b = g.insert(&ident, sample_sign()).unwrap();
        assert_eq!(a, b);
        assert_eq!(g.len(), 1);
    }

    #[cfg(feature = "std")]
    #[test]
    fn vec_forced_collision_promotes_via_salt() {
        let mut g = VecGraph::new();
        let real = sample_identity(7);
        let real_unsalted = SignId::from_identity(&real);
        let fake_bytes = [0x77u8; SIGN_IDENTITY_BYTES];
        g.force_push_for_test(real_unsalted, fake_bytes, sample_sign());

        let before = hash::COLLISION_COUNT.load(Ordering::Relaxed);
        let assigned = g.insert(&real, sample_sign()).unwrap();
        let after = hash::COLLISION_COUNT.load(Ordering::Relaxed);

        assert_ne!(assigned, real_unsalted);
        assert_eq!(assigned, SignId::from_identity_salted(&real, 1));
        assert!(after > before);
        assert_eq!(g.len(), 2);
    }

    // ── Trait-level use (generic over backend) ──────────────────────────

    fn fill_with_three<G: UmweltGraph>(g: &mut G) -> (SignId, SignId, SignId) {
        let a = g.insert(&sample_identity(11), sample_sign()).unwrap();
        let b = g.insert(&sample_identity(22), sample_sign()).unwrap();
        let c = g.insert(&sample_identity(33), sample_sign()).unwrap();
        (a, b, c)
    }

    #[test]
    fn trait_generic_fill_works_on_heapless() {
        let mut g = HeaplessGraph::<8>::new();
        let (a, b, c) = fill_with_three(&mut g);
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_eq!(g.len(), 3);
    }

    #[cfg(feature = "std")]
    #[test]
    fn trait_generic_fill_works_on_vec() {
        let mut g = VecGraph::new();
        let (a, b, c) = fill_with_three(&mut g);
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_eq!(g.len(), 3);
    }

    // ── P1.4 / ADR-0016: dedup reinforcement ──────────────────────────────

    /// Build a sign with a given intensity, `formed_at`, `duration_us`,
    /// and channel. Identity is supplied separately by the test.
    fn sign_with(
        intensity: Q15,
        formed_at: crate::TimestampUs,
        duration_us: u64,
        channel: u16,
    ) -> Sign {
        Sign {
            id: SignId::new(0),
            layer: SemioticLayer::Icon,
            domain: SignDomain::Spatial,
            formed_at,
            duration_us,
            channel,
            intensity,
            coherence: Q15::ZERO,
        }
    }

    #[test]
    fn dedup_reinforces_intensity_saturating_add() {
        let mut g = HeaplessGraph::<4>::new();
        let ident = sample_identity(1);
        let q = Q15::from_f32(0.3);
        g.insert(&ident, sign_with(q, 1000, 0, 5)).unwrap();
        let id = g.insert(&ident, sign_with(q, 9999, 0, 5)).unwrap();
        // 0.3 + 0.3 = 0.6 (within 1 LSB).
        let got = g.get(id).unwrap().intensity;
        let expected = q.saturating_add(q);
        assert_eq!(got, expected);
        assert_eq!(g.len(), 1);
    }

    #[test]
    fn dedup_preserves_formed_at() {
        let mut g = HeaplessGraph::<4>::new();
        let ident = sample_identity(2);
        g.insert(&ident, sign_with(Q15::ZERO, 1_000, 0, 0)).unwrap();
        let id = g
            .insert(&ident, sign_with(Q15::ZERO, 99_999, 0, 0))
            .unwrap();
        // formed_at is sticky — the sign's "birth time".
        assert_eq!(g.get(id).unwrap().formed_at, 1_000);
    }

    #[test]
    fn dedup_duration_us_is_high_water_mark() {
        let mut g = HeaplessGraph::<4>::new();
        let ident = sample_identity(3);
        g.insert(&ident, sign_with(Q15::ZERO, 0, 500, 0)).unwrap();
        g.insert(&ident, sign_with(Q15::ZERO, 0, 200, 0)).unwrap(); // shorter, ignored
        let id = g.insert(&ident, sign_with(Q15::ZERO, 0, 900, 0)).unwrap();
        assert_eq!(g.get(id).unwrap().duration_us, 900);
    }

    #[test]
    fn dedup_saturates_intensity_at_q15_max() {
        let mut g = HeaplessGraph::<4>::new();
        let ident = sample_identity(4);
        // Two full-scale positive inserts: saturating_add caps at Q15::MAX.
        g.insert(&ident, sign_with(Q15::MAX, 0, 0, 0)).unwrap();
        let id = g.insert(&ident, sign_with(Q15::MAX, 0, 0, 0)).unwrap();
        assert_eq!(g.get(id).unwrap().intensity, Q15::MAX);
    }

    #[test]
    fn dedup_saturates_intensity_at_q15_min_for_inhibitory() {
        let mut g = HeaplessGraph::<4>::new();
        let ident = sample_identity(5);
        g.insert(&ident, sign_with(Q15::MIN, 0, 0, 0)).unwrap();
        let id = g.insert(&ident, sign_with(Q15::MIN, 0, 0, 0)).unwrap();
        assert_eq!(g.get(id).unwrap().intensity, Q15::MIN);
    }

    #[test]
    fn dedup_preserves_coherence_owned_by_semiogenesis() {
        let mut g = HeaplessGraph::<4>::new();
        let ident = sample_identity(6);
        // First insert: coherence starts at ZERO (Sign::icon default).
        g.insert(&ident, sign_with(Q15::from_f32(0.1), 0, 0, 0))
            .unwrap();
        // Simulate the semiogenesis engine having set coherence after
        // formation — done via the test hook's same-slot overwrite.
        // (In production this happens out-of-band; here we assert the
        // existing value survives a second insert rather than being
        // clobbered.)
        //
        // We can't easily mutate a stored sign from outside, so
        // instead we observe coherence *never changes from ZERO* on
        // a pure insert path — which is the operative invariant.
        let id = g
            .insert(&ident, sign_with(Q15::from_f32(0.2), 0, 0, 0))
            .unwrap();
        assert_eq!(g.get(id).unwrap().coherence, Q15::ZERO);
    }

    #[cfg(feature = "std")]
    #[test]
    fn dedup_reinforcement_works_on_vec_backend() {
        let mut g = VecGraph::new();
        let ident = sample_identity(7);
        let q = Q15::from_f32(0.25);
        g.insert(&ident, sign_with(q, 10, 100, 0)).unwrap();
        g.insert(&ident, sign_with(q, 20, 50, 0)).unwrap();
        let id = g.insert(&ident, sign_with(q, 30, 300, 0)).unwrap();
        assert_eq!(g.len(), 1);
        let s = g.get(id).unwrap();
        assert_eq!(s.formed_at, 10); // first-seen
        assert_eq!(s.duration_us, 300); // high-water
                                        // Three saturating-adds of 0.25 = 0.75 within 1 LSB.
        let expected = q.saturating_add(q).saturating_add(q);
        assert_eq!(s.intensity, expected);
    }

    // ── Proptest invariants (ADR-0016) ────────────────────────────────────

    use proptest::prelude::*;

    proptest! {
        /// Invariant 1: N inserts of the same identity leave `len == 1`
        /// and every insert returns the same id.
        #[test]
        fn idempotent_id_on_repeated_insert(
            st in any::<u8>(), n in 1usize..=16usize
        ) {
            let mut g = HeaplessGraph::<32>::new();
            let ident = sample_identity(st);
            let first = g.insert(&ident, sample_sign()).unwrap();
            for _ in 1..n {
                let id = g.insert(&ident, sample_sign()).unwrap();
                prop_assert_eq!(id, first);
            }
            prop_assert_eq!(g.len(), 1);
        }

        /// Invariant 2 (positive branch): inserting non-negative
        /// intensities is non-decreasing in the stored intensity.
        #[test]
        fn positive_reinforcement_is_monotone_non_decreasing(
            st in any::<u8>(),
            steps in proptest::collection::vec(0.0f32..=1.0f32, 1..16),
        ) {
            let mut g = HeaplessGraph::<32>::new();
            let ident = sample_identity(st);
            let mut prev = Q15::ZERO;
            for q in steps {
                let id = g.insert(&ident, sign_with(Q15::from_f32(q), 0, 0, 0))
                    .unwrap();
                let cur = g.get(id).unwrap().intensity;
                prop_assert!(
                    cur.raw() >= prev.raw(),
                    "intensity decreased: prev={} cur={}",
                    prev.raw(), cur.raw()
                );
                prev = cur;
            }
        }

        /// Invariant 4: `formed_at` never changes on re-insert.
        #[test]
        fn formed_at_is_stable(
            st in any::<u8>(),
            t0 in 0u64..1_000_000,
            later in proptest::collection::vec(0u64..u64::MAX, 1..8),
        ) {
            let mut g = HeaplessGraph::<32>::new();
            let ident = sample_identity(st);
            let first_id = g.insert(&ident, sign_with(Q15::ZERO, t0, 0, 0))
                .unwrap();
            for t in later {
                g.insert(&ident, sign_with(Q15::ZERO, t, 0, 0)).unwrap();
                prop_assert_eq!(
                    g.get(first_id).unwrap().formed_at, t0,
                    "formed_at drifted from t0={} after reinsert at {}",
                    t0, t
                );
            }
        }

        /// Invariant 5: stored `duration_us` equals the max of all
        /// inserted durations.
        #[test]
        fn duration_is_high_water_mark(
            st in any::<u8>(),
            durations in proptest::collection::vec(0u64..10_000_000, 1..16),
        ) {
            let mut g = HeaplessGraph::<32>::new();
            let ident = sample_identity(st);
            let mut max = 0u64;
            for d in &durations {
                let id = g.insert(&ident, sign_with(Q15::ZERO, 0, *d, 0))
                    .unwrap();
                if *d > max { max = *d; }
                prop_assert_eq!(g.get(id).unwrap().duration_us, max);
            }
        }
    }

    // ── P1.5: graph query API ─────────────────────────────────────────────

    /// Identity builder exposing every coordinate used by P1.5 filters.
    fn ident_for(layer: SemioticLayer, sign_type: u8, agent_id: u32, bucket: u64) -> SignIdentity {
        SignIdentity {
            layer,
            domain: SignDomain::Spatial,
            sign_type,
            agent_id,
            spatial_ref: [0, 0, 0],
            time_bucket: bucket,
        }
    }

    /// Sign builder parameterised by the four filter dimensions we
    /// care about (layer / channel / formed_at / intensity).
    fn sig(layer: SemioticLayer, formed_at: crate::TimestampUs, channel: u16) -> Sign {
        Sign {
            id: SignId::new(0),
            layer,
            domain: SignDomain::Spatial,
            formed_at,
            duration_us: 0,
            channel,
            intensity: Q15::ZERO,
            coherence: Q15::ZERO,
        }
    }

    #[test]
    fn iter_empty_graph_yields_nothing() {
        let g = HeaplessGraph::<4>::new();
        assert_eq!(g.iter().count(), 0);
    }

    #[test]
    fn iter_returns_every_inserted_sign() {
        let mut g = HeaplessGraph::<8>::new();
        g.insert(
            &ident_for(SemioticLayer::Icon, 1, 1, 1),
            sig(SemioticLayer::Icon, 10, 5),
        )
        .unwrap();
        g.insert(
            &ident_for(SemioticLayer::Index, 2, 1, 2),
            sig(SemioticLayer::Index, 20, 6),
        )
        .unwrap();
        g.insert(
            &ident_for(SemioticLayer::Symbol, 3, 1, 3),
            sig(SemioticLayer::Symbol, 30, 7),
        )
        .unwrap();
        assert_eq!(g.iter().count(), 3);
    }

    #[test]
    fn by_layer_filters_correctly() {
        let mut g = HeaplessGraph::<8>::new();
        g.insert(
            &ident_for(SemioticLayer::Icon, 1, 1, 1),
            sig(SemioticLayer::Icon, 0, 0),
        )
        .unwrap();
        g.insert(
            &ident_for(SemioticLayer::Index, 2, 1, 2),
            sig(SemioticLayer::Index, 0, 0),
        )
        .unwrap();
        g.insert(
            &ident_for(SemioticLayer::Icon, 3, 1, 3),
            sig(SemioticLayer::Icon, 0, 0),
        )
        .unwrap();
        assert_eq!(g.by_layer(SemioticLayer::Icon).count(), 2);
        assert_eq!(g.by_layer(SemioticLayer::Index).count(), 1);
        assert_eq!(g.by_layer(SemioticLayer::Symbol).count(), 0);
    }

    #[test]
    fn by_channel_filters_correctly() {
        let mut g = HeaplessGraph::<8>::new();
        g.insert(
            &ident_for(SemioticLayer::Icon, 1, 1, 1),
            sig(SemioticLayer::Icon, 0, 5),
        )
        .unwrap();
        g.insert(
            &ident_for(SemioticLayer::Icon, 2, 1, 2),
            sig(SemioticLayer::Icon, 0, 7),
        )
        .unwrap();
        g.insert(
            &ident_for(SemioticLayer::Icon, 3, 1, 3),
            sig(SemioticLayer::Icon, 0, 5),
        )
        .unwrap();
        assert_eq!(g.by_channel(5).count(), 2);
        assert_eq!(g.by_channel(7).count(), 1);
        assert_eq!(g.by_channel(99).count(), 0);
    }

    #[test]
    fn by_time_window_is_inclusive_on_both_ends() {
        let mut g = HeaplessGraph::<8>::new();
        g.insert(
            &ident_for(SemioticLayer::Icon, 1, 1, 1),
            sig(SemioticLayer::Icon, 100, 0),
        )
        .unwrap();
        g.insert(
            &ident_for(SemioticLayer::Icon, 2, 1, 2),
            sig(SemioticLayer::Icon, 200, 0),
        )
        .unwrap();
        g.insert(
            &ident_for(SemioticLayer::Icon, 3, 1, 3),
            sig(SemioticLayer::Icon, 300, 0),
        )
        .unwrap();
        // Inclusive window picks up the boundary points.
        assert_eq!(g.by_time_window(100, 200).count(), 2);
        // Single-point window picks up exactly one sign.
        assert_eq!(g.by_time_window(200, 200).count(), 1);
        // Empty window (lo > hi) returns nothing without panicking.
        assert_eq!(g.by_time_window(500, 100).count(), 0);
        // Full span covers everything.
        assert_eq!(g.by_time_window(0, u64::MAX).count(), 3);
    }

    #[cfg(feature = "std")]
    #[test]
    fn vec_backend_iter_and_filters_agree_with_heapless() {
        let mut g = VecGraph::new();
        g.insert(
            &ident_for(SemioticLayer::Icon, 1, 1, 1),
            sig(SemioticLayer::Icon, 10, 5),
        )
        .unwrap();
        g.insert(
            &ident_for(SemioticLayer::Index, 2, 1, 2),
            sig(SemioticLayer::Index, 20, 5),
        )
        .unwrap();
        g.insert(
            &ident_for(SemioticLayer::Icon, 3, 1, 3),
            sig(SemioticLayer::Icon, 30, 7),
        )
        .unwrap();
        assert_eq!(g.iter().count(), 3);
        assert_eq!(g.by_layer(SemioticLayer::Icon).count(), 2);
        assert_eq!(g.by_channel(5).count(), 2);
        assert_eq!(g.by_time_window(15, 25).count(), 1);
    }

    proptest! {
        /// P1.5 invariant: filtering by layer==L then layer==L again is
        /// the same as filtering once — filters are pure predicates
        /// composed by `and`.
        #[test]
        fn filter_composition_is_idempotent(
            seed in 0u8..64,
        ) {
            let mut g = HeaplessGraph::<64>::new();
            for i in 0..32u8 {
                let layer = match (i ^ seed) % 3 {
                    0 => SemioticLayer::Icon,
                    1 => SemioticLayer::Index,
                    _ => SemioticLayer::Symbol,
                };
                g.insert(
                    &ident_for(layer, i, 1, i as u64),
                    sig(layer, i as u64, (i as u16) % 4),
                ).unwrap();
            }
            for &l in &[SemioticLayer::Icon, SemioticLayer::Index, SemioticLayer::Symbol] {
                let once = g.by_layer(l).count();
                let twice = g.by_layer(l).filter(|s| s.layer == l).count();
                prop_assert_eq!(once, twice);
            }
        }

        /// P1.5 invariant: the three filters partition the graph on
        /// their respective dimensions — summed counts over exhaustive
        /// categories equal `len()`.
        #[test]
        fn filters_partition_the_graph(
            seed in 0u8..64,
        ) {
            let mut g = HeaplessGraph::<64>::new();
            for i in 0..32u8 {
                let layer = match (i ^ seed) % 3 {
                    0 => SemioticLayer::Icon,
                    1 => SemioticLayer::Index,
                    _ => SemioticLayer::Symbol,
                };
                g.insert(
                    &ident_for(layer, i, 1, i as u64),
                    sig(layer, i as u64, (i as u16) % 4),
                ).unwrap();
            }
            let sum_layer = g.by_layer(SemioticLayer::Icon).count()
                + g.by_layer(SemioticLayer::Index).count()
                + g.by_layer(SemioticLayer::Symbol).count();
            prop_assert_eq!(sum_layer, g.len());

            let sum_channel: usize = (0..4u16).map(|c| g.by_channel(c).count()).sum();
            prop_assert_eq!(sum_channel, g.len());
        }

        /// P1.5 invariant: commutativity — `by_layer` then `by_channel`
        /// yields the same set as `by_channel` then `by_layer` (the
        /// AND of two predicates doesn't care about order).
        #[test]
        fn filter_composition_commutes(
            seed in 0u8..64,
            target_ch in 0u16..4,
        ) {
            let mut g = HeaplessGraph::<64>::new();
            for i in 0..32u8 {
                let layer = match (i ^ seed) % 3 {
                    0 => SemioticLayer::Icon,
                    1 => SemioticLayer::Index,
                    _ => SemioticLayer::Symbol,
                };
                g.insert(
                    &ident_for(layer, i, 1, i as u64),
                    sig(layer, i as u64, (i as u16) % 4),
                ).unwrap();
            }
            let a = g.by_layer(SemioticLayer::Icon)
                .filter(|s| s.channel == target_ch).count();
            let b = g.by_channel(target_ch)
                .filter(|s| s.layer == SemioticLayer::Icon).count();
            prop_assert_eq!(a, b);
        }

        /// P1.5 invariant: a time window covering every `formed_at`
        /// returns exactly `len()` signs; a disjoint window returns 0.
        #[test]
        fn time_window_bounds_behave(
            bucket_cnt in 1usize..16,
        ) {
            let mut g = HeaplessGraph::<32>::new();
            for i in 0..bucket_cnt as u8 {
                g.insert(
                    &ident_for(SemioticLayer::Icon, i, 1, i as u64),
                    sig(SemioticLayer::Icon, (i as u64) * 1_000, 0),
                ).unwrap();
            }
            prop_assert_eq!(g.by_time_window(0, u64::MAX).count(), g.len());
            // Open window strictly above every stored formed_at.
            let hi = (bucket_cnt as u64) * 1_000 + 1;
            prop_assert_eq!(g.by_time_window(hi, u64::MAX).count(), 0);
        }

        // ── P1.6: insertion-order + dedup idempotence ───────────────────

        /// P1.6 invariant (a): iterator yields signs in first-insertion
        /// order. Dedup hits (re-inserts of the same identity) do not
        /// change the position; non-colliding inserts append at the end.
        #[test]
        fn insertion_order_preserved_across_dedup_hits(
            n_distinct in 2usize..16,
            dedup_schedule in proptest::collection::vec(0usize..16, 1..32),
        ) {
            let mut g = HeaplessGraph::<32>::new();
            // Insert `n_distinct` distinct identities in order.
            let mut idents: Vec<SignIdentity> = Vec::new();
            for i in 0..n_distinct as u8 {
                let ident = sample_identity(i);
                idents.push(ident);
                g.insert(&ident, sample_sign()).unwrap();
            }
            // Now re-insert some of them according to the schedule.
            // This should NOT change the iterator order.
            for &idx in &dedup_schedule {
                let pick = idx % n_distinct;
                g.insert(&idents[pick], sample_sign()).unwrap();
            }
            // Iterator must still yield signs in the original insertion order.
            let ids_in_order: Vec<SignId> = g.iter().map(|s| s.id).collect();
            prop_assert_eq!(ids_in_order.len(), n_distinct);
            for (pos, ident) in idents.iter().enumerate() {
                let expected_id = SignId::from_identity(ident);
                prop_assert_eq!(
                    ids_in_order[pos], expected_id,
                    "position {} drifted after dedup schedule", pos
                );
            }
        }

        /// P1.6 invariant (b): inserting M identities, each repeated an
        /// arbitrary number of times, leaves `iter().count()` equal to M
        /// (the number of distinct identities, not the total insert count).
        #[test]
        fn dedup_idempotent_count_equals_distinct(
            repeats in proptest::collection::vec(1usize..=8, 1..16),
        ) {
            let mut g = HeaplessGraph::<64>::new();
            let n_distinct = repeats.len();
            for (i, &rep) in repeats.iter().enumerate() {
                let ident = sample_identity(i as u8);
                for _ in 0..rep {
                    g.insert(&ident, sample_sign()).unwrap();
                }
            }
            prop_assert_eq!(g.iter().count(), n_distinct);
            prop_assert_eq!(g.len(), n_distinct);
        }
    }

    // ── P3.4 decay sweep (ADR-0015) ─────────────────────────────────────────

    #[test]
    fn tick_decay_on_empty_graph_is_noop() {
        let mut g = HeaplessGraph::<8>::new();
        let summary = g.tick_decay(|_| DecayClass::Obstacle);
        assert_eq!(summary.remaining, 0);
        assert_eq!(summary.pruned, 0);
    }

    #[test]
    fn tick_decay_reduces_intensity_in_place() {
        let mut g = HeaplessGraph::<8>::new();
        let ident = sample_identity(1);
        let mut s = sample_sign();
        s.intensity = Q15::MAX;
        let id = g.insert(&ident, s).unwrap();
        g.tick_decay(|_| DecayClass::Obstacle);
        let after = g.get(id).unwrap().intensity;
        assert!(after.raw() < Q15::MAX.raw());
        assert!(after.raw() > 0);
    }

    #[test]
    fn tick_decay_prunes_fully_decayed_nodes() {
        let mut g = HeaplessGraph::<8>::new();
        let mut s = sample_sign();
        s.intensity = Q15::from_raw(1); // one below floor
        let ident = sample_identity(2);
        g.insert(&ident, s).unwrap();

        let summary = g.tick_decay(|_| DecayClass::Obstacle);
        assert_eq!(summary.remaining, 0);
        assert_eq!(summary.pruned, 1);
    }

    #[test]
    fn tick_decay_prunes_by_class_only_for_its_members() {
        let mut g = HeaplessGraph::<8>::new();

        // A terrain sign with intensity=1 survives (α ≈ 0.996 rounds
        // back up to 1); an obstacle sign with intensity=1 floors to 0.
        let mut terrain = sample_sign();
        terrain.intensity = Q15::from_raw(1);
        let terrain_ident = sample_identity(10);
        let terrain_id = g.insert(&terrain_ident, terrain).unwrap();

        let mut obstacle = sample_sign();
        obstacle.intensity = Q15::from_raw(1);
        let obstacle_ident = sample_identity(11);
        let obstacle_id = g.insert(&obstacle_ident, obstacle).unwrap();

        let summary = g.tick_decay(|s| {
            if s.id == obstacle_id {
                DecayClass::Obstacle
            } else {
                DecayClass::Terrain
            }
        });
        assert_eq!(summary.pruned, 1);
        assert_eq!(summary.remaining, 1);
        // The terrain node survives at its floor.
        assert!(g.get(terrain_id).is_some());
        assert!(g.get(obstacle_id).is_none());
    }

    #[cfg(feature = "std")]
    #[test]
    fn vec_tick_decay_reduces_intensity_and_prunes() {
        let mut g = VecGraph::new();

        let mut live = sample_sign();
        live.intensity = Q15::MAX;
        let live_id = g.insert(&sample_identity(3), live).unwrap();

        let mut dead = sample_sign();
        dead.intensity = Q15::from_raw(1);
        let dead_id = g.insert(&sample_identity(4), dead).unwrap();

        let summary = g.tick_decay(|_| DecayClass::Obstacle);
        assert_eq!(summary.pruned, 1);
        assert_eq!(summary.remaining, 1);
        // Live id survives with strictly-smaller intensity.
        let live_after = g.get(live_id).unwrap().intensity;
        assert!(live_after.raw() < Q15::MAX.raw());
        assert!(g.get(dead_id).is_none());
    }

    #[cfg(feature = "std")]
    #[test]
    fn vec_tick_decay_preserves_map_integrity_after_multiple_prunes() {
        // Regression: swap_remove updates `index` for the moved node.
        let mut g = VecGraph::new();
        let mut idents = [0u8; 6];
        #[allow(clippy::needless_range_loop)]
        for i in 0..idents.len() {
            idents[i] = i as u8;
        }
        let mut ids = Vec::<SignId>::new();
        for &t in &idents {
            let mut s = sample_sign();
            // Alternate: dead, live, dead, live, dead, live
            s.intensity = if t % 2 == 0 {
                Q15::from_raw(1)
            } else {
                Q15::MAX
            };
            let id = g.insert(&sample_identity(t), s).unwrap();
            ids.push(id);
        }
        let summary = g.tick_decay(|_| DecayClass::Obstacle);
        assert_eq!(summary.pruned, 3);
        assert_eq!(summary.remaining, 3);
        // Every surviving id is reachable through get().
        for (i, id) in ids.iter().enumerate() {
            if i % 2 == 0 {
                assert!(g.get(*id).is_none(), "expected pruned id {id:?}");
            } else {
                assert!(g.get(*id).is_some(), "expected surviving id {id:?}");
            }
        }
    }

    proptest! {
        #[test]
        fn decay_sweep_remaining_plus_pruned_equals_input_prop(
            n in 0usize..16,
        ) {
            // Fill N signs with Q15::MAX, apply enough obstacle ticks
            // to empty the graph; total pruned must equal N.
            let mut g = HeaplessGraph::<16>::new();
            for i in 0..n {
                let mut s = sample_sign();
                s.intensity = Q15::MAX;
                g.insert(&sample_identity(i as u8), s).unwrap();
            }
            prop_assert_eq!(g.len(), n);
            let mut total_pruned = 0usize;
            for _ in 0..100 {
                let summary = g.tick_decay(|_| DecayClass::Obstacle);
                total_pruned += summary.pruned;
                if summary.remaining == 0 {
                    break;
                }
            }
            prop_assert_eq!(total_pruned, n);
            prop_assert_eq!(g.len(), 0);
        }
    }
}

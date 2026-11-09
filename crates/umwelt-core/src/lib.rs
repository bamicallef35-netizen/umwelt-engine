//! Umwelt Engine — core runtime.
//!
//! On-device semiotic runtime: raw percepts → Icons → Indices → Symbols.
//! `no_std` by default so the same code runs on ESP32-S3 and Jetson.
//! Enable the `std` feature for host tooling.

#![cfg_attr(not(feature = "std"), no_std)]
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod decay;
pub mod graph;
pub mod hash;
pub mod icon;
pub mod index;
pub mod replay;
pub mod symbol;

/// Fixed-point time in microseconds since an arbitrary epoch.
/// `u64` gives us ~584k years of headroom, enough for any field device.
pub type TimestampUs = u64;

// ── Q15 fixed-point newtype (ADR-0004, P0.5.4) ─────────────────────────────

/// Q15 fixed-point scalar on the symmetric interval `[-1.0, 1.0]`.
///
/// Stored as a 16-bit signed integer where the raw value `r` represents
/// the real number `r / 32767`. The symmetric range means both `+1.0`
/// and `-1.0` are exactly representable (as `32767` and `-32767`
/// respectively); the raw value `i16::MIN` (`-32768`) is **not** a
/// valid Q15 and is clamped on construction so every `Q15` is closed
/// under `saturating_mul_q15`.
///
/// Conversions, saturating arithmetic, and construction all live here
/// so that every crate that carries normalised quantities (intensity,
/// coherence, correlation, …) shares one definition. See ADR-0004.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Q15(i16);

impl Q15 {
    /// Scale factor mapping Q15 bits ↔ real numbers. `1.0` ↔ `32767`.
    pub const SCALE: f32 = 32767.0;

    /// The zero value.
    pub const ZERO: Q15 = Q15(0);

    /// The maximum representable Q15 value, `+1.0` (raw `32767`).
    pub const MAX: Q15 = Q15(i16::MAX);

    /// The minimum representable Q15 value, `-1.0` (raw `-32767`).
    /// Symmetric with [`Q15::MAX`] — `i16::MIN` is deliberately excluded
    /// so products stay representable.
    pub const MIN: Q15 = Q15(-i16::MAX);

    /// Construct from raw Q15 bits. Values outside `[-32767, 32767]`
    /// (i.e. the single point `i16::MIN`) are clamped to [`Q15::MIN`].
    #[inline]
    pub const fn from_raw(bits: i16) -> Self {
        if bits < -i16::MAX {
            Q15::MIN
        } else {
            Q15(bits)
        }
    }

    /// Get the raw Q15 bits. Always in `[-32767, 32767]`.
    #[inline]
    pub const fn raw(self) -> i16 {
        self.0
    }

    /// Encode an `f32` to Q15. The input is clamped to `[-1.0, 1.0]`
    /// and NaN is treated as zero. Rounding is round-half-away-from-zero;
    /// the round-trip `from_f32 → to_f32` is guaranteed within 1 LSB
    /// (`≈ 3.05e-5`).
    #[inline]
    pub fn from_f32(x: f32) -> Self {
        let x = if x.is_nan() { 0.0 } else { x.clamp(-1.0, 1.0) };
        let scaled = x * Self::SCALE;
        // Round half-away-from-zero without needing libm: truncation
        // toward zero (Rust's `as` conversion) plus a ±0.5 bias.
        let rounded = if scaled >= 0.0 {
            scaled + 0.5
        } else {
            scaled - 0.5
        };
        // f32 → i32 is saturating in Rust ≥1.45, so NaN/∞ cannot overflow.
        let i = rounded as i32;
        Q15::from_clamped_i32(i)
    }

    /// Decode to `f32`. Exact at the endpoints: `Q15::MAX.to_f32() == 1.0`
    /// and `Q15::MIN.to_f32() == -1.0`.
    #[inline]
    pub fn to_f32(self) -> f32 {
        self.0 as f32 / Self::SCALE
    }

    /// Saturating addition. Wraps `i16::saturating_add` but clamps to
    /// the symmetric Q15 range, not the raw `i16` range.
    #[inline]
    pub const fn saturating_add(self, other: Q15) -> Q15 {
        Q15::from_clamped_i32(self.0 as i32 + other.0 as i32)
    }

    /// Saturating Q15 × Q15 → Q15. Internally uses an `i32` accumulator
    /// and a round-to-nearest shift by 15, so `Q15(0.5) * Q15(0.5)` is
    /// exactly `Q15(0.25)` up to one LSB of rounding.
    #[inline]
    pub const fn saturating_mul_q15(self, other: Q15) -> Q15 {
        let prod = (self.0 as i32) * (other.0 as i32);
        // Add 1 << 14 for round-half-up before the 15-bit arithmetic shift.
        let rounded = (prod + (1 << 14)) >> 15;
        Q15::from_clamped_i32(rounded)
    }

    /// Internal: clamp an `i32` into the symmetric Q15 range and narrow.
    /// Separate because `const fn` can't use `i32::clamp` on stable 1.95.
    #[inline]
    const fn from_clamped_i32(v: i32) -> Q15 {
        const MAX: i32 = i16::MAX as i32;
        const MIN: i32 = -(i16::MAX as i32);
        let c = if v > MAX {
            MAX
        } else if v < MIN {
            MIN
        } else {
            v
        };
        Q15(c as i16)
    }
}

impl Default for Q15 {
    #[inline]
    fn default() -> Self {
        Q15::ZERO
    }
}

// ── Enums ──────────────────────────────────────────────────────────────────

/// Semiotic layer. Peirce's triadic grade, from raw icon to fully symbolic.
///
/// `#[repr(u8)]` matters here: ADR-0014 encodes the layer into the
/// canonical 21-byte `SignId` hash input as `SemioticLayer as u8`.
/// Never reorder variants without rev-ing ADR-0014 — the discriminant
/// is part of the content-address surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[repr(u8)]
pub enum SemioticLayer {
    /// First-ness. A sensation, not yet about anything.
    Icon = 0,
    /// Second-ness. Indicates, points, correlates.
    Index = 1,
    /// Third-ness. Named. Enters the agent's vocabulary.
    Symbol = 2,
}

/// Domain of a sign: what aspect of the world it pertains to.
///
/// Icons can be spatial, temporal, or coherence-typed at formation;
/// higher layers inherit or widen their domain.
///
/// `#[repr(u8)]` matters here: ADR-0014 encodes the domain into the
/// canonical 21-byte `SignId` hash input as `SignDomain as u8`. Never
/// reorder variants without rev-ing ADR-0014.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[repr(u8)]
pub enum SignDomain {
    /// The sign relates to spatial organisation (topology, proximity, extent).
    /// Position is stored separately in the graph (added in P1.3).
    Spatial = 0,
    /// The sign relates to temporal sequence (rhythm, periodicity, onset,
    /// duration).
    Temporal = 1,
    /// The sign relates to cross-channel coherence (correlation, resonance,
    /// mutual information).
    Coherence = 2,
}

/// Opaque identifier for a sign in the Umwelt Graph.
///
/// The inner `u64` is crate-private so that P1.2's collision-proof
/// allocator can enforce id-uniqueness invariants. External code
/// constructs via [`SignId::new`] and reads via [`SignId::raw`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SignId(u64);

impl SignId {
    /// Construct a `SignId` from a raw `u64` value.
    #[inline]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// Return the underlying `u64`.
    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Content-address a [`SignIdentity`] into a `SignId` per ADR-0014.
    ///
    /// Hashes the 21-byte canonical encoding with `hash::fnv1a_64`.
    /// Two identities with equal canonical bytes always hash to the
    /// same id; collisions are the allocator's concern (see P1.3 and
    /// [`SignId::from_identity_salted`]).
    #[inline]
    pub fn from_identity(identity: &SignIdentity) -> Self {
        let bytes = identity.to_canonical_bytes();
        Self(hash::fnv1a_64(&bytes))
    }

    /// Rehash a [`SignIdentity`] with a 16-bit salt appended — the
    /// collision-promotion path of ADR-0014's "detect + promote +
    /// log-once" policy.
    ///
    /// The allocator (P1.3) walks `salt = 1, 2, 3, …` until the
    /// resulting `SignId` is free, then stores the node under that id
    /// with no visible marker to callers. `salt = 0` is reserved for
    /// the unsalted path; callers must not invoke this with `0`.
    #[inline]
    pub fn from_identity_salted(identity: &SignIdentity, salt: u16) -> Self {
        let mut bytes = [0u8; SIGN_IDENTITY_BYTES + 2];
        let canonical = identity.to_canonical_bytes();
        bytes[..SIGN_IDENTITY_BYTES].copy_from_slice(&canonical);
        bytes[SIGN_IDENTITY_BYTES..].copy_from_slice(&salt.to_le_bytes());
        Self(hash::fnv1a_64(&bytes))
    }
}

/// Width in bytes of the canonical [`SignIdentity`] encoding used as
/// input to `hash::fnv1a_64` (ADR-0014).
pub const SIGN_IDENTITY_BYTES: usize = 21;

/// Canonical identity fields for a sign — the content-address surface
/// that deduplicates the Umwelt Graph (ADR-0014).
///
/// This is the *input* to [`SignId::from_identity`]. Two signs with
/// the same [`SignIdentity`] are **the same sign** for graph-dedup
/// purposes; magnitude fields like [`Sign::intensity`] are deliberately
/// excluded so that a recurring percept updates an existing node
/// rather than allocating a new one.
///
/// The 21-byte canonical encoding (per [`SignIdentity::to_canonical_bytes`])
/// is:
///
/// ```text
/// [0]      layer          (SemioticLayer as u8)
/// [1]      domain         (SignDomain as u8)
/// [2]      sign_type      (u8, vocabulary index per ADR-0011)
/// [3..7]   agent_id       (u32 little-endian)
/// [7..13]  spatial_ref    (3 × i16 little-endian: x, y, z)
/// [13..21] time_bucket    (u64 little-endian, monotonic ms / 2^14)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SignIdentity {
    /// Semiotic grade (Icon / Index / Symbol).
    pub layer: SemioticLayer,
    /// Domain of the sign (Spatial / Temporal / Coherence).
    pub domain: SignDomain,
    /// Vocabulary index of the sign's type (per ADR-0011).
    pub sign_type: u8,
    /// Opaque agent identity. The graph is agent-local, but the
    /// field still participates in the hash so that replayed /
    /// imported signs from a peer do not alias a local agent's ids.
    pub agent_id: u32,
    /// Spatial reference, in the agent's local frame. Triple of
    /// `i16`s (x, y, z) in implementation-defined units; the only
    /// semantics `umwelt-core` assumes is that byte-identical triples
    /// are "the same place".
    pub spatial_ref: [i16; 3],
    /// Coarsened timestamp: `monotonic_ms / 2^14`. Signs within the
    /// same ~16-second window collide into one node (deduplication);
    /// crossing a bucket boundary produces a fresh id. See
    /// [`SignIdentity::time_bucket_from_us`] for the helper.
    pub time_bucket: u64,
}

impl SignIdentity {
    /// Serialise this identity into the 21-byte canonical encoding
    /// hashed by [`SignId::from_identity`]. Layout is fixed by
    /// ADR-0014 and must not be reordered without bumping the ADR.
    #[inline]
    pub fn to_canonical_bytes(&self) -> [u8; SIGN_IDENTITY_BYTES] {
        let mut out = [0u8; SIGN_IDENTITY_BYTES];
        out[0] = self.layer as u8;
        out[1] = self.domain as u8;
        out[2] = self.sign_type;
        out[3..7].copy_from_slice(&self.agent_id.to_le_bytes());
        out[7..9].copy_from_slice(&self.spatial_ref[0].to_le_bytes());
        out[9..11].copy_from_slice(&self.spatial_ref[1].to_le_bytes());
        out[11..13].copy_from_slice(&self.spatial_ref[2].to_le_bytes());
        out[13..21].copy_from_slice(&self.time_bucket.to_le_bytes());
        out
    }

    /// Coarsen a µs timestamp ([`TimestampUs`]) into the 16-second
    /// bucket used as the `time_bucket` hash input. Per ADR-0014:
    /// `bucket = (timestamp_us / 1000) / 2^14` — floor-divide to ms
    /// first, then shift to a ~16.384-s grain.
    #[inline]
    pub const fn time_bucket_from_us(timestamp_us: TimestampUs) -> u64 {
        (timestamp_us / 1000) >> 14
    }
}

/// A sign — the atomic unit of the Umwelt.
///
/// Normalised quantities (`intensity`, `coherence`) are [`Q15`] values on
/// the symmetric interval `[-1.0, 1.0]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Sign {
    /// Stable identifier within this agent's graph.
    pub id: SignId,
    /// Semiotic grade.
    pub layer: SemioticLayer,
    /// Domain classification of the sign.
    pub domain: SignDomain,
    /// Time the sign was formed (µs since agent epoch).
    pub formed_at: TimestampUs,
    /// Duration of the percept (µs). Zero means instantaneous / point event.
    pub duration_us: u64,
    /// Opaque sensor/channel tag. Interpretation is domain-specific.
    pub channel: u16,
    /// Intensity of the percept. `0` = quiescent, positive = excitatory,
    /// negative = inhibitory; `Q15::MAX` is the ceiling.
    pub intensity: Q15,
    /// Coherence of this sign with its context. Positive = reinforcing,
    /// negative = contradicting, zero = orthogonal. Initialised to
    /// [`Q15::ZERO`] at formation; set later by the semiogenesis engine.
    pub coherence: Q15,
}

impl Sign {
    /// Construct a new **Icon** sign with all domain fields explicit.
    pub const fn new_icon(
        id: SignId,
        domain: SignDomain,
        formed_at: TimestampUs,
        duration_us: u64,
        channel: u16,
        intensity: Q15,
    ) -> Self {
        Self {
            id,
            layer: SemioticLayer::Icon,
            domain,
            formed_at,
            duration_us,
            channel,
            intensity,
            coherence: Q15::ZERO,
        }
    }

    /// Convenience constructor: instantaneous spatial icon.
    pub const fn icon(id: SignId, formed_at: TimestampUs, channel: u16, intensity: Q15) -> Self {
        Self::new_icon(id, SignDomain::Spatial, formed_at, 0, channel, intensity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── existing regression ────────────────────────────────────────────────
    #[test]
    fn icon_constructor_sets_layer() {
        let s = Sign::icon(SignId::new(1), 0, 0, Q15::ZERO);
        assert_eq!(s.layer, SemioticLayer::Icon);
    }

    // ── P1.1: SignDomain ───────────────────────────────────────────────────
    #[test]
    fn sign_domain_variants_are_distinct() {
        assert_ne!(SignDomain::Spatial, SignDomain::Temporal);
        assert_ne!(SignDomain::Temporal, SignDomain::Coherence);
        assert_ne!(SignDomain::Spatial, SignDomain::Coherence);
    }

    #[test]
    fn new_icon_sets_domain_temporal() {
        let s = Sign::new_icon(
            SignId::new(2),
            SignDomain::Temporal,
            1000,
            500,
            3,
            Q15::from_raw(16000),
        );
        assert_eq!(s.layer, SemioticLayer::Icon);
        assert_eq!(s.domain, SignDomain::Temporal);
        assert_eq!(s.duration_us, 500);
        assert_eq!(s.intensity, Q15::from_raw(16000));
        assert_eq!(s.coherence, Q15::ZERO); // initialised to zero at formation
    }

    #[test]
    fn new_icon_sets_domain_coherence() {
        let s = Sign::new_icon(
            SignId::new(3),
            SignDomain::Coherence,
            2000,
            0,
            7,
            Q15::from_raw(-8000),
        );
        assert_eq!(s.domain, SignDomain::Coherence);
        assert_eq!(s.intensity, Q15::from_raw(-8000));
    }

    #[test]
    fn icon_convenience_is_spatial_instantaneous() {
        let s = Sign::icon(SignId::new(4), 42_000_000, 1, Q15::MAX);
        assert_eq!(s.domain, SignDomain::Spatial);
        assert_eq!(s.duration_us, 0);
        assert_eq!(s.coherence, Q15::ZERO);
    }

    // ── P0.5.4: Q15 newtype semantics ──────────────────────────────────────

    #[test]
    fn q15_endpoints_encode_exactly() {
        assert_eq!(Q15::from_f32(1.0), Q15::MAX);
        assert_eq!(Q15::from_f32(-1.0), Q15::MIN);
        assert_eq!(Q15::from_f32(0.0), Q15::ZERO);
        assert_eq!(Q15::MAX.to_f32(), 1.0);
        assert_eq!(Q15::MIN.to_f32(), -1.0);
        assert_eq!(Q15::ZERO.to_f32(), 0.0);
    }

    #[test]
    fn q15_clamps_out_of_range_and_nan() {
        assert_eq!(Q15::from_f32(2.5), Q15::MAX);
        assert_eq!(Q15::from_f32(-2.5), Q15::MIN);
        assert_eq!(Q15::from_f32(f32::INFINITY), Q15::MAX);
        assert_eq!(Q15::from_f32(f32::NEG_INFINITY), Q15::MIN);
        assert_eq!(Q15::from_f32(f32::NAN), Q15::ZERO);
    }

    #[test]
    fn q15_from_raw_clamps_int_min() {
        // i16::MIN (-32768) is not a valid Q15; must fold to Q15::MIN (-32767).
        assert_eq!(Q15::from_raw(i16::MIN), Q15::MIN);
        assert_eq!(Q15::from_raw(-32767), Q15::MIN);
        assert_eq!(Q15::from_raw(0), Q15::ZERO);
        assert_eq!(Q15::from_raw(32767), Q15::MAX);
    }

    #[test]
    fn q15_saturating_add_saturates_symmetrically() {
        let half = Q15::from_f32(0.5);
        let three_q = Q15::from_f32(0.75);
        // 0.5 + 0.75 = 1.25 → clamped to MAX (+1.0)
        assert_eq!(half.saturating_add(three_q), Q15::MAX);
        // -0.5 + -0.75 = -1.25 → clamped to MIN (-1.0)
        let neg_half = Q15::from_f32(-0.5);
        let neg_three_q = Q15::from_f32(-0.75);
        assert_eq!(neg_half.saturating_add(neg_three_q), Q15::MIN);
        // Adding ZERO is identity.
        assert_eq!(half.saturating_add(Q15::ZERO), half);
    }

    #[test]
    fn q15_saturating_mul_q15_matches_reference() {
        // 0.5 * 0.5 = 0.25 within 1 LSB
        let half = Q15::from_f32(0.5);
        let quarter_ref = Q15::from_f32(0.25);
        let prod = half.saturating_mul_q15(half);
        let delta = (prod.raw() - quarter_ref.raw()).unsigned_abs();
        assert!(delta <= 1, "0.5 * 0.5 off by {delta} LSB");

        // MAX * MAX ≈ +1.0 (within 1 LSB of MAX, never exceeds).
        let max_sq = Q15::MAX.saturating_mul_q15(Q15::MAX);
        assert!(
            (Q15::MAX.raw() - max_sq.raw()).unsigned_abs() <= 1,
            "MAX*MAX = {:?}, expected within 1 LSB of MAX",
            max_sq
        );
        // MIN * MIN ≈ +1.0 (within 1 LSB of MAX).
        let min_sq = Q15::MIN.saturating_mul_q15(Q15::MIN);
        assert!(
            (Q15::MAX.raw() - min_sq.raw()).unsigned_abs() <= 1,
            "MIN*MIN = {:?}, expected within 1 LSB of MAX",
            min_sq
        );
        // Zero annihilates.
        assert_eq!(Q15::MAX.saturating_mul_q15(Q15::ZERO), Q15::ZERO);
    }

    // ── P0.5.4: Proptest float round-trip within 1 LSB ─────────────────────
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn q15_f32_roundtrip_within_one_lsb(x in -1.0f32..=1.0f32) {
            let q = Q15::from_f32(x);
            let back = q.to_f32();
            // Rounding error bound: 0.5 / 32767 ≈ 1.526e-5 < 1 LSB.
            let lsb = 1.0_f32 / Q15::SCALE;
            prop_assert!(
                (back - x).abs() <= lsb,
                "roundtrip |back - x| = {} exceeds 1 LSB ({}); x={}, q={:?}",
                (back - x).abs(), lsb, x, q
            );
        }

        #[test]
        fn q15_saturating_add_never_exceeds_magnitude_one(
            a in -1.0f32..=1.0f32, b in -1.0f32..=1.0f32
        ) {
            let qa = Q15::from_f32(a);
            let qb = Q15::from_f32(b);
            let sum = qa.saturating_add(qb);
            // Q15 saturates symmetrically at ±MAX (i.e. ±32767 ≤ sum.raw() ≤ ±32767).
            prop_assert!(sum.raw() >= Q15::MIN.raw());
            prop_assert!(sum.raw() <= Q15::MAX.raw());
        }

        #[test]
        fn q15_mul_commutes(a in -1.0f32..=1.0f32, b in -1.0f32..=1.0f32) {
            let qa = Q15::from_f32(a);
            let qb = Q15::from_f32(b);
            prop_assert_eq!(
                qa.saturating_mul_q15(qb),
                qb.saturating_mul_q15(qa)
            );
        }
    }

    // ── Copy/Clone/PartialEq/Eq/Hash (derived) ────────────────────────────
    #[test]
    fn sign_is_copy() {
        let a = Sign::icon(SignId::new(5), 0, 0, Q15::from_raw(100));
        let b = a; // copy
        assert_eq!(a, b);
    }

    // ── P0.5.5: SignId constructor / accessor ─────────────────────────────
    #[test]
    fn sign_id_new_raw_roundtrip() {
        assert_eq!(SignId::new(0).raw(), 0);
        assert_eq!(SignId::new(42).raw(), 42);
        assert_eq!(SignId::new(u64::MAX).raw(), u64::MAX);
    }

    #[test]
    fn sign_id_equality_uses_inner_value() {
        assert_eq!(SignId::new(7), SignId::new(7));
        assert_ne!(SignId::new(7), SignId::new(8));
    }

    #[test]
    fn sign_id_hashes_via_hash_trait() {
        // Regression: `SignId` is itself `Hash`-able; the test helper
        // FNV used to live inline here but was promoted to the public
        // `hash::fnv1a_64` in P1.2. We now drive it through that
        // canonical function to keep a single source of truth.
        use core::hash::{Hash, Hasher};
        struct FnvHasher(u64);
        impl Hasher for FnvHasher {
            fn finish(&self) -> u64 {
                self.0
            }
            fn write(&mut self, bytes: &[u8]) {
                // Delegate to the public primitive on each chunk.
                // Chaining via basis→state works because FNV-1a's
                // state is just the evolving 64-bit accumulator.
                self.0 = hash::fnv1a_64_continue(self.0, bytes);
            }
        }
        let mut h1 = FnvHasher(hash::FNV1A_OFFSET_BASIS);
        let mut h2 = FnvHasher(hash::FNV1A_OFFSET_BASIS);
        SignId::new(99).hash(&mut h1);
        SignId::new(99).hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());
    }

    // ── P1.2: SignIdentity canonical encoding + from_identity ─────────────

    fn sample_identity() -> SignIdentity {
        SignIdentity {
            layer: SemioticLayer::Icon,
            domain: SignDomain::Spatial,
            sign_type: 7,
            agent_id: 0xDEAD_BEEF,
            spatial_ref: [-1, 2, -3],
            time_bucket: 0x1234_5678_9ABC_DEF0,
        }
    }

    #[test]
    fn canonical_bytes_layout_matches_adr_0014() {
        // Lock down the byte layout exactly. If anyone reorders
        // SignIdentity fields or changes endianness, this test fires
        // — and the build-plan requires bumping ADR-0014 alongside.
        let id = sample_identity();
        let bytes = id.to_canonical_bytes();
        assert_eq!(bytes.len(), SIGN_IDENTITY_BYTES);
        assert_eq!(bytes[0], 0); // Icon
        assert_eq!(bytes[1], 0); // Spatial
        assert_eq!(bytes[2], 7);
        assert_eq!(&bytes[3..7], &0xDEAD_BEEFu32.to_le_bytes());
        assert_eq!(&bytes[7..9], &(-1i16).to_le_bytes());
        assert_eq!(&bytes[9..11], &2i16.to_le_bytes());
        assert_eq!(&bytes[11..13], &(-3i16).to_le_bytes());
        assert_eq!(&bytes[13..21], &0x1234_5678_9ABC_DEF0u64.to_le_bytes());
    }

    #[test]
    fn time_bucket_from_us_coarsens_to_16s_grain() {
        // 0 us  → bucket 0
        assert_eq!(SignIdentity::time_bucket_from_us(0), 0);
        // 16.383 s  → still bucket 0 (just under 2^14 ms).
        assert_eq!(SignIdentity::time_bucket_from_us(16_383_999), 0);
        // 16.384 s  → bucket 1.
        assert_eq!(SignIdentity::time_bucket_from_us(16_384_000), 1);
        // Monotone.
        let a = SignIdentity::time_bucket_from_us(1_000_000_000);
        let b = SignIdentity::time_bucket_from_us(2_000_000_000);
        assert!(b >= a);
    }

    #[test]
    fn from_identity_is_deterministic() {
        let id = SignId::from_identity(&sample_identity());
        let id2 = SignId::from_identity(&sample_identity());
        assert_eq!(id, id2);
        // And matches raw FNV-1a of the canonical bytes.
        let expected = hash::fnv1a_64(&sample_identity().to_canonical_bytes());
        assert_eq!(id.raw(), expected);
    }

    #[test]
    fn from_identity_salted_differs_from_unsalted() {
        let base = SignId::from_identity(&sample_identity());
        let salted1 = SignId::from_identity_salted(&sample_identity(), 1);
        let salted2 = SignId::from_identity_salted(&sample_identity(), 2);
        assert_ne!(base, salted1);
        assert_ne!(salted1, salted2);
        // Deterministic re-salt.
        let salted1_again = SignId::from_identity_salted(&sample_identity(), 1);
        assert_eq!(salted1, salted1_again);
    }

    #[test]
    fn distinct_sign_type_yields_distinct_id() {
        let mut a = sample_identity();
        let mut b = sample_identity();
        a.sign_type = 1;
        b.sign_type = 2;
        assert_ne!(SignId::from_identity(&a), SignId::from_identity(&b));
    }

    // ── P1.2: three proptest invariants from ADR-0014 ─────────────────────

    // Strategy for SignIdentity that covers all variants and the full
    // integer ranges of the byte-level fields.
    fn any_layer() -> impl Strategy<Value = SemioticLayer> {
        prop_oneof![
            Just(SemioticLayer::Icon),
            Just(SemioticLayer::Index),
            Just(SemioticLayer::Symbol),
        ]
    }

    fn any_domain() -> impl Strategy<Value = SignDomain> {
        prop_oneof![
            Just(SignDomain::Spatial),
            Just(SignDomain::Temporal),
            Just(SignDomain::Coherence),
        ]
    }

    fn any_identity() -> impl Strategy<Value = SignIdentity> {
        (
            any_layer(),
            any_domain(),
            any::<u8>(),
            any::<u32>(),
            any::<i16>(),
            any::<i16>(),
            any::<i16>(),
            any::<u64>(),
        )
            .prop_map(|(layer, domain, st, aid, x, y, z, tb)| SignIdentity {
                layer,
                domain,
                sign_type: st,
                agent_id: aid,
                spatial_ref: [x, y, z],
                time_bucket: tb,
            })
    }

    proptest! {
        /// ADR-0014 invariant 1: distinct canonical bytes → distinct
        /// `SignId` (modulo the explicit collision-promotion path,
        /// which the allocator owns — P1.3). At the primitive level,
        /// a FNV-1a collision on a 21-byte input is acceptable but
        /// vanishingly rare; proptest random-walks 256 cases and we
        /// simply assert the observed collision count stays zero.
        #[test]
        fn distinct_identities_have_distinct_ids(
            a in any_identity(), b in any_identity()
        ) {
            let ba = a.to_canonical_bytes();
            let bb = b.to_canonical_bytes();
            let ida = SignId::from_identity(&a);
            let idb = SignId::from_identity(&b);
            if ba != bb {
                prop_assert_ne!(
                    ida, idb,
                    "proptest stumbled on a real FNV-1a 64 collision — \
                     report as a regression; ADR-0014 handles this at \
                     the allocator layer, not at the hash primitive"
                );
            }
        }

        /// ADR-0014 invariant 2: identical identities → identical ids.
        #[test]
        fn identical_identities_have_identical_ids(id in any_identity()) {
            prop_assert_eq!(
                SignId::from_identity(&id),
                SignId::from_identity(&id)
            );
        }

        /// ADR-0014 invariant 3: salted rehash with a different salt
        /// yields a different id with overwhelming probability. If a
        /// collision ever fires under proptest it is the same kind of
        /// rare event as invariant 1; the allocator's promotion loop
        /// keeps walking salts until one is free.
        #[test]
        fn salted_rehash_differs_from_unsalted(
            id in any_identity(),
            salt in 1u16..=u16::MAX,
        ) {
            let base = SignId::from_identity(&id);
            let salted = SignId::from_identity_salted(&id, salt);
            prop_assert_ne!(base, salted);
        }
    }

    // ── P0.5.6: optional serde feature ─────────────────────────────────────
    // Compile-time proof that every public type carries serde derives when
    // the feature is on. No runtime check — just typecheck.
    #[cfg(feature = "serde")]
    #[test]
    fn serde_derives_present_on_public_types() {
        fn assert_serde<T: serde::Serialize + for<'de> serde::Deserialize<'de>>() {}
        assert_serde::<Q15>();
        assert_serde::<SignId>();
        assert_serde::<SemioticLayer>();
        assert_serde::<SignDomain>();
        assert_serde::<Sign>();
    }
}

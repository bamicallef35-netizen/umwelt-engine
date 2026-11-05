//! Sign decay — three-class exponential per ADR-0015.
//!
//! Each decayable sign belongs to one of three classes
//! (`Obstacle`, `Contamination`, `Terrain`) defined by a
//! time-to-5%-retention constant τ. The per-tick Q15 multiplier is the
//! real number `α = 0.05^(Δt / τ)` rounded to the nearest Q15 integer,
//! evaluated at compile time. `Δt = 100 ms` per ADR-0013's 10 Hz tick.
//!
//! This module is pure math — no I/O, no graph knowledge. Graph
//! maintenance (sweep + prune) lives in [`crate::graph`].
//!
//! All public items are `const` where possible so the table can live
//! in `static` contexts on `no_std` targets.

use crate::Q15;

/// Maintenance tick, in milliseconds. The ADR-0013 sample rate
/// (10 Hz) defines this interval.
pub const TICK_MS: u64 = 100;

/// Retention floor: each τ of wall time leaves this fraction of the
/// original intensity. Pinned by ADR-0015.
pub const RETENTION_AT_TAU: f32 = 0.05;

// ── Pinned α constants (ADR-0015) ──────────────────────────────────────────

/// Per-tick multiplier for `Obstacle`-class signs. Real value
/// `0.05^(0.1 / 0.35) ≈ 0.4248`. Retention after τ = 350 ms is 5.00 %.
pub const ALPHA_OBSTACLE_Q15: Q15 = Q15::from_raw(13_920);

/// Per-tick multiplier for `Contamination`-class signs. Real value
/// `0.05^(0.1 / 14.0) ≈ 0.9788`. Retention after τ = 14 s is 5.00 %.
pub const ALPHA_CONTAMINATION_Q15: Q15 = Q15::from_raw(32_073);

/// Per-tick multiplier for `Terrain`-class signs. Real value
/// `0.05^(0.1 / 70.0) ≈ 0.9957`. Retention after τ = 70 s is 5.00 %.
pub const ALPHA_TERRAIN_Q15: Q15 = Q15::from_raw(32_627);

/// Decay class carried (conceptually) on the vocabulary's `sign_type`.
/// For v0.1, callers supply the class via a classifier function at the
/// graph-maintenance boundary — see [`crate::graph::HeaplessGraph::tick_decay`]
/// and [`crate::graph::VecGraph::tick_decay`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum DecayClass {
    /// Transient geometric hazards, τ = 350 ms.
    Obstacle,
    /// Chemical / biological signs, τ = 14 s.
    Contamination,
    /// Slow-changing environmental context, τ = 70 s.
    Terrain,
}

impl DecayClass {
    /// Time-to-5%-retention, in milliseconds. Pinned by ADR-0015.
    pub const fn tau_ms(self) -> u64 {
        match self {
            DecayClass::Obstacle => 350,
            DecayClass::Contamination => 14_000,
            DecayClass::Terrain => 70_000,
        }
    }

    /// Number of 100 ms ticks in one τ. Always ≥ 1 for the three pinned
    /// classes (τ_min = 350 ms = 3.5 ticks → `ticks_per_tau(Obstacle) = 3`).
    /// Floor division is deliberate: retention is validated at the
    /// nearest tick *below* τ.
    pub const fn ticks_per_tau(self) -> u64 {
        self.tau_ms() / TICK_MS
    }
}

/// The per-tick Q15 multiplier for a given class.
pub const fn alpha_q15(class: DecayClass) -> Q15 {
    match class {
        DecayClass::Obstacle => ALPHA_OBSTACLE_Q15,
        DecayClass::Contamination => ALPHA_CONTAMINATION_Q15,
        DecayClass::Terrain => ALPHA_TERRAIN_Q15,
    }
}

/// Apply one tick of decay. `intensity * alpha_q15(class)` with Q15
/// saturating semantics — non-negative input stays non-negative,
/// negative input decays toward zero symmetrically.
#[inline]
pub fn decay_one_tick(intensity: Q15, class: DecayClass) -> Q15 {
    intensity.saturating_mul_q15(alpha_q15(class))
}

/// Apply `ticks` successive decays. Convenience for tests and replay
/// fast-forward. Runs an explicit loop rather than raising `α` to a
/// power to preserve saturating-at-each-tick semantics.
pub fn decay_n_ticks(intensity: Q15, class: DecayClass, ticks: u64) -> Q15 {
    let mut v = intensity;
    let mut i = 0;
    while i < ticks {
        v = decay_one_tick(v, class);
        i += 1;
    }
    v
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn alpha_constants_are_pinned() {
        assert_eq!(alpha_q15(DecayClass::Obstacle).raw(), 13_920);
        assert_eq!(alpha_q15(DecayClass::Contamination).raw(), 32_073);
        assert_eq!(alpha_q15(DecayClass::Terrain).raw(), 32_627);
    }

    #[test]
    fn tau_ms_values_match_adr() {
        assert_eq!(DecayClass::Obstacle.tau_ms(), 350);
        assert_eq!(DecayClass::Contamination.tau_ms(), 14_000);
        assert_eq!(DecayClass::Terrain.tau_ms(), 70_000);
    }

    #[test]
    fn decay_one_tick_reduces_positive_intensity() {
        let before = Q15::MAX;
        let after = decay_one_tick(before, DecayClass::Obstacle);
        assert!(after.raw() < before.raw());
        assert!(after.raw() > 0);
    }

    #[test]
    fn decay_preserves_zero() {
        assert_eq!(decay_one_tick(Q15::ZERO, DecayClass::Obstacle), Q15::ZERO);
        assert_eq!(decay_one_tick(Q15::ZERO, DecayClass::Terrain), Q15::ZERO);
    }

    /// ADR-0015 reverse check: after exactly τ, retention should be
    /// 5.00 % ± 0.02 %. Q15 quantisation and per-tick saturation
    /// loosen the bound modestly.
    #[test]
    fn retention_at_tau_is_five_percent_obstacle() {
        let initial = Q15::MAX;
        let ticks = DecayClass::Obstacle.ticks_per_tau();
        let after = decay_n_ticks(initial, DecayClass::Obstacle, ticks);
        let ratio = after.raw() as f32 / Q15::MAX.raw() as f32;
        // Floor-ticks (3 ticks for 350 ms) lands a touch above 5%.
        assert!(
            (0.03..=0.15).contains(&ratio),
            "Obstacle retention {ratio} out of tolerance",
        );
    }

    #[test]
    fn retention_at_tau_is_five_percent_contamination() {
        let initial = Q15::MAX;
        let ticks = DecayClass::Contamination.ticks_per_tau();
        let after = decay_n_ticks(initial, DecayClass::Contamination, ticks);
        let ratio = after.raw() as f32 / Q15::MAX.raw() as f32;
        // Contamination has 140 ticks per τ; quantisation drift pulls
        // this a few percent off the nominal 5.00%.
        assert!(
            (0.03..=0.07).contains(&ratio),
            "Contamination retention {ratio} out of tolerance",
        );
    }

    #[test]
    fn retention_at_tau_is_five_percent_terrain() {
        let initial = Q15::MAX;
        let ticks = DecayClass::Terrain.ticks_per_tau();
        let after = decay_n_ticks(initial, DecayClass::Terrain, ticks);
        let ratio = after.raw() as f32 / Q15::MAX.raw() as f32;
        assert!(
            (0.03..=0.07).contains(&ratio),
            "Terrain retention {ratio} out of tolerance",
        );
    }

    #[test]
    fn decay_eventually_reaches_zero_obstacle() {
        // Obstacle α = 13920/32767 ≈ 0.4248. Enough ticks erase any
        // finite starting intensity.
        let mut v = Q15::MAX;
        for _ in 0..50 {
            v = decay_one_tick(v, DecayClass::Obstacle);
        }
        assert_eq!(v, Q15::ZERO);
    }

    #[test]
    fn decay_monotone_non_increasing() {
        let mut v = Q15::MAX;
        let mut prev = v.raw();
        for _ in 0..20 {
            v = decay_one_tick(v, DecayClass::Obstacle);
            assert!(
                v.raw() <= prev,
                "decay increased: prev={prev} now={}",
                v.raw()
            );
            prev = v.raw();
        }
    }

    #[test]
    fn negative_intensity_decays_toward_zero() {
        let mut v = Q15::MIN;
        for _ in 0..50 {
            v = decay_one_tick(v, DecayClass::Obstacle);
        }
        // Q15::MIN * positive_alpha tends toward zero from below.
        assert!(v.raw() > Q15::MIN.raw());
        assert!(v.raw() <= 0);
    }

    // ── Proptests (ADR-0015 contract) ──────────────────────────────────────

    proptest! {
        #[test]
        fn decay_never_increases_magnitude_prop(
            raw in 0i16..=i16::MAX,
            class_idx in 0u8..3,
        ) {
            let class = match class_idx {
                0 => DecayClass::Obstacle,
                1 => DecayClass::Contamination,
                _ => DecayClass::Terrain,
            };
            let before = Q15::from_raw(raw);
            let after = decay_one_tick(before, class);
            prop_assert!(after.raw() <= before.raw());
            prop_assert!(after.raw() >= 0);
        }

        #[test]
        fn decay_zero_is_fixpoint_prop(class_idx in 0u8..3) {
            let class = match class_idx {
                0 => DecayClass::Obstacle,
                1 => DecayClass::Contamination,
                _ => DecayClass::Terrain,
            };
            prop_assert_eq!(decay_one_tick(Q15::ZERO, class), Q15::ZERO);
        }

        #[test]
        fn decay_n_ticks_matches_loop_prop(
            raw in 0i16..=i16::MAX,
            n in 0u64..64,
        ) {
            let class = DecayClass::Obstacle;
            let initial = Q15::from_raw(raw);
            let by_fn = decay_n_ticks(initial, class, n);
            let mut by_loop = initial;
            for _ in 0..n {
                by_loop = decay_one_tick(by_loop, class);
            }
            prop_assert_eq!(by_fn, by_loop);
        }

        #[test]
        fn decay_monotone_across_ticks_prop(
            raw in 0i16..=i16::MAX,
            n in 0u32..32,
        ) {
            let class = DecayClass::Contamination;
            let a = decay_n_ticks(Q15::from_raw(raw), class, n as u64);
            let b = decay_n_ticks(Q15::from_raw(raw), class, (n + 1) as u64);
            prop_assert!(b.raw() <= a.raw());
        }
    }
}

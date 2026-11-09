//! Icon layer (P3.1) — thresholding + rolling-entropy trigger per channel.
//!
//! This is the bottom of the semiotic pipeline. A per-channel
//! [`IconRing`] holds the last `W` samples, maintains a `B`-bin
//! histogram incrementally, computes rolling Shannon entropy from the
//! histogram, and emits an [`IconEvent`] when ADR-0013's three-way
//! trigger fires:
//!
//! 1. **Activity gate** — the current sample's magnitude is at least
//!    `magnitude_gate` in `|Q15|` units.
//! 2. **Novelty** — the entropy differs from an EWMA baseline by at
//!    least `entropy_delta`.
//! 3. **Cooldown** — no icon has fired on this channel within the
//!    last `cooldown_samples` observations.
//!
//! Everything is `no_std`, heap-free, and integer-only on the hot
//! path: the Shannon `log2` lookups come from a `const`-computed
//! Q4.12 table ([`LOG2_Q12`]), not `libm::log2f`.
//!
//! See `docs/decisions.md` § ADR-0013 (signification trigger) for the
//! binding parameter choices and § ADR-0015 (decay profiles) for the
//! downstream tick budget this layer feeds.

use crate::{TimestampUs, Q15};

// ── Public types ───────────────────────────────────────────────────────────

/// One icon emission from a channel.
///
/// Carries the raw signal that fired the trigger plus the metrics the
/// index layer (P3.2) needs to decide whether to correlate it with
/// events on neighbouring channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct IconEvent {
    /// Channel that fired the icon.
    pub channel: u16,
    /// Timestamp (µs) of the triggering sample.
    pub formed_at: TimestampUs,
    /// Normalised Shannon entropy of the channel's current `W`-sample
    /// window, expressed as a `Q15` fraction of `log2(B)` bits.
    /// Always non-negative; `Q15::MAX` corresponds to the uniform
    /// `B`-bin distribution (entropy = `log2(B)` bits).
    pub entropy: Q15,
    /// Absolute magnitude of the triggering sample (the input that
    /// passed the activity gate). Saturating: clamped to `Q15::MAX`.
    pub magnitude: Q15,
}

/// Per-channel icon trigger: ring buffer + histogram + entropy gate.
///
/// `W` is the window length (ADR-0013 default: `64`) and `B` the
/// histogram bin count (default: `8`). Both are `const` so the
/// storage is known at compile time — no heap, no allocation.
///
/// Construct with [`IconRing::new`] for ADR-0013 defaults or
/// [`IconRing::with_thresholds`] to tune per-channel. Feed samples
/// in with [`IconRing::observe`].
#[derive(Debug, Clone)]
pub struct IconRing<const W: usize, const B: usize> {
    channel: u16,
    /// Raw sample ring, head-written.
    buf: [Q15; W],
    /// Next write position, wrapping at `W`.
    head: usize,
    /// Histogram over `B` uniform bins on `[-1, 1]`. Values fit in
    /// `u16` because `W <= 256` in practice (the Q4.12 `LOG2_Q12`
    /// table caps counts at 256).
    histogram: [u16; B],
    /// Samples seen so far; `observe` only emits once this reaches
    /// `W` so entropy is defined over a full window.
    samples_seen: u32,
    /// Samples observed since the last icon fired. Clamped at
    /// `u32::MAX`; only compared against `cooldown_samples`.
    samples_since_icon: u32,
    /// EWMA baseline of the normalised entropy. Warm until the first
    /// `W` samples have arrived, then updated each observe.
    baseline: Q15,
    /// Minimum `|sample|` required to consider firing.
    magnitude_gate: Q15,
    /// Minimum `|entropy - baseline|` (in normalised-Q15) required
    /// to fire.
    entropy_delta: Q15,
    /// Floor on `samples_since_icon` before the next fire is allowed.
    cooldown_samples: u8,
}

// ── Defaults (ADR-0013) ────────────────────────────────────────────────────

/// Default magnitude gate (ADR-0013 param table): `|sample| >= ~0.1`.
pub const DEFAULT_MAGNITUDE_GATE: Q15 = Q15::from_raw(3277);

/// Default entropy delta (ADR-0013 param table): `0.5` bits, converted
/// to the normalised-Q15 scale this module uses (i.e. `0.5 / log2(B)`
/// of `Q15::MAX`). For `B = 8`, `log2(B) = 3`, so the delta is
/// `(0.5 / 3.0) * 32767 ≈ 5461`. Callers with non-default `B` should
/// use [`IconRing::with_thresholds`] to override.
pub const DEFAULT_ENTROPY_DELTA_B8: Q15 = Q15::from_raw(5461);

/// Default cooldown (ADR-0013 param table): four samples.
pub const DEFAULT_COOLDOWN_SAMPLES: u8 = 4;

// ── log2 LUT (ADR-0013: 256-entry, no libm) ────────────────────────────────

/// Size of the `log2` lookup table. Indices `1..=256` hold
/// `log2(k)` in Q4.12; index `0` is a defined-zero hole that is never
/// read on the hot path (zero-count bins are skipped).
pub const LOG2_LUT_LEN: usize = 257;

/// Shift of the fixed-point Q-format used by [`LOG2_Q12`].
pub const LOG2_Q_SHIFT: u32 = 12;

/// Unit value in the `LOG2_Q12` Q-format (`1.0_bit`).
pub const LOG2_Q_ONE: u32 = 1 << LOG2_Q_SHIFT;

/// Precomputed `log2(k)` for `k ∈ [1, 256]` in Q4.12 fixed-point.
///
/// Computed once at build time via [`const_log2_q12`]. Accuracy:
/// 12 fractional bits of the binary expansion, i.e. within 1 LSB of
/// the true `log2(k)` for every entry. No `libm` dependency.
pub const LOG2_Q12: [u32; LOG2_LUT_LEN] = {
    let mut lut = [0u32; LOG2_LUT_LEN];
    let mut k = 1;
    while k < LOG2_LUT_LEN {
        lut[k] = const_log2_q12(k as u32);
        k += 1;
    }
    lut
};

/// Compute `log2(k)` in Q4.12, at compile time. Integer-only: no
/// `libm`, no floating-point. Uses the binary-squaring algorithm —
/// on each of 12 iterations, square the normalised mantissa, extract
/// the integer bit, and renormalise.
#[inline]
const fn const_log2_q12(k: u32) -> u32 {
    if k < 2 {
        return 0;
    }
    let int_part = 31 - k.leading_zeros();
    // Normalise `k` into `[1.0, 2.0)` at the Q.61 scale used inside
    // the squaring loop: `m = (k / 2^int_part) * 2^61`.
    let mut m: u64 = (k as u64) << (61 - int_part);
    let mut frac: u32 = 0;
    let mut i = 0;
    while i < LOG2_Q_SHIFT {
        // Square in u128 to avoid overflow; shift 61 to return to
        // Q.61, giving a value in `[1.0, 4.0)`.
        m = ((m as u128 * m as u128) >> 61) as u64;
        frac <<= 1;
        // Bit 62 of a Q.61 number is the integer-part bit. If it is
        // set, the squared value is >= 2.0 → the next fractional
        // bit of log2 is 1; halve `m` to renormalise into [1, 2).
        if m >> 62 != 0 {
            frac |= 1;
            m >>= 1;
        }
        i += 1;
    }
    (int_part << LOG2_Q_SHIFT) | frac
}

// ── IconRing impl ──────────────────────────────────────────────────────────

impl<const W: usize, const B: usize> IconRing<W, B> {
    /// Construct an `IconRing` with ADR-0013 defaults. `B` must be in
    /// `[2, 256]` and `W` in `[1, 256]` so the `LOG2_Q12` table
    /// covers every reachable count; enforced at runtime on the
    /// first `observe`.
    ///
    /// The default `entropy_delta` is calibrated for `B = 8`
    /// ([`DEFAULT_ENTROPY_DELTA_B8`]). For other `B`, use
    /// [`IconRing::with_thresholds`].
    #[inline]
    pub const fn new(channel: u16) -> Self {
        Self::with_thresholds(
            channel,
            DEFAULT_MAGNITUDE_GATE,
            DEFAULT_ENTROPY_DELTA_B8,
            DEFAULT_COOLDOWN_SAMPLES,
        )
    }

    /// Construct with caller-chosen thresholds. `magnitude_gate` and
    /// `entropy_delta` are both in the normalised-Q15 range (the
    /// former absolute magnitude on `[0, 1]`, the latter a fraction
    /// of `log2(B)` bits).
    #[inline]
    pub const fn with_thresholds(
        channel: u16,
        magnitude_gate: Q15,
        entropy_delta: Q15,
        cooldown_samples: u8,
    ) -> Self {
        Self {
            channel,
            buf: [Q15::ZERO; W],
            head: 0,
            histogram: [0u16; B],
            samples_seen: 0,
            samples_since_icon: u32::MAX, // ready to fire immediately
            baseline: Q15::ZERO,
            magnitude_gate,
            entropy_delta,
            cooldown_samples,
        }
    }

    /// Channel id this ring was constructed for. Copied verbatim
    /// onto every emitted [`IconEvent`].
    #[inline]
    pub const fn channel(&self) -> u16 {
        self.channel
    }

    /// Current EWMA baseline of the normalised entropy.
    #[inline]
    pub const fn baseline(&self) -> Q15 {
        self.baseline
    }

    /// Number of samples observed so far, saturating at `u32::MAX`.
    #[inline]
    pub const fn samples_seen(&self) -> u32 {
        self.samples_seen
    }

    /// Current entropy over the `W`-sample window, as a normalised
    /// `Q15` fraction of `log2(B)`. Returns [`Q15::ZERO`] before the
    /// window is full.
    #[inline]
    pub fn entropy(&self) -> Q15 {
        if (self.samples_seen as usize) < W {
            return Q15::ZERO;
        }
        self.compute_entropy_q15()
    }

    /// Push one sample in. Emits an [`IconEvent`] iff the ADR-0013
    /// three-way trigger fires. Always updates the ring, histogram,
    /// and baseline regardless of whether an icon is emitted.
    pub fn observe(&mut self, sample: Q15, now_us: TimestampUs) -> Option<IconEvent> {
        // Debug-assert the const-generic envelope once per call.
        debug_assert!(W >= 1 && W <= 256, "W must be in [1, 256]");
        debug_assert!(B >= 2 && B <= 256, "B must be in [2, 256]");

        // ── 1. Update ring + histogram.
        let new_bin = bin_of::<B>(sample);
        if (self.samples_seen as usize) >= W {
            // Evict the sample at head.
            let old = self.buf[self.head];
            let old_bin = bin_of::<B>(old);
            self.histogram[old_bin] = self.histogram[old_bin].saturating_sub(1);
        }
        self.buf[self.head] = sample;
        self.head = (self.head + 1) % W;
        self.histogram[new_bin] = self.histogram[new_bin].saturating_add(1);
        self.samples_seen = self.samples_seen.saturating_add(1);
        self.samples_since_icon = self.samples_since_icon.saturating_add(1);

        // ── 2. Entropy + baseline update. Both require a full window.
        let window_full = (self.samples_seen as usize) >= W;
        if !window_full {
            return None;
        }
        let h = self.compute_entropy_q15();
        self.baseline = ewma_update(self.baseline, h);

        // ── 3. Triggers.
        let magnitude = abs_q15(sample);
        if magnitude.raw() < self.magnitude_gate.raw() {
            return None;
        }
        let delta = abs_q15_diff(h, self.baseline);
        if delta.raw() < self.entropy_delta.raw() {
            return None;
        }
        if self.samples_since_icon < self.cooldown_samples as u32 {
            return None;
        }

        // ── 4. Fire.
        self.samples_since_icon = 0;
        Some(IconEvent {
            channel: self.channel,
            formed_at: now_us,
            entropy: h,
            magnitude,
        })
    }

    /// Compute normalised entropy from the current histogram.
    /// Assumes the window is full; the caller checks that.
    ///
    /// Works in Q4.12 bits internally, then scales into normalised
    /// Q15 by `Q15::MAX / log2(B)`. Zero-count bins are skipped so
    /// `log2(0)` never enters the sum.
    fn compute_entropy_q15(&self) -> Q15 {
        // Σ count_i * log2(count_i) in Q4.12. Sum fits in u32 because
        // count_i ≤ W ≤ 256 and log2(count_i) ≤ 8 bits = 8*4096 =
        // 32768 Q4.12 units; 256 bins * 256 * 32768 ≈ 2 billion > u32.
        // In practice B ≤ 256 and W ≤ 256 so the bound is tighter,
        // but we use u64 for the sum to keep headroom.
        let mut sum_q12: u64 = 0;
        let mut i = 0;
        while i < B {
            let c = self.histogram[i] as u32;
            if c > 0 {
                // LOG2_Q12[c] is safe because c ≤ W ≤ 256.
                sum_q12 += (c as u64) * (LOG2_Q12[c as usize] as u64);
            }
            i += 1;
        }
        // H_bits = log2(W) - sum_q12 / W. Keep in Q4.12.
        let log2_w_q12 = LOG2_Q12[W] as u64;
        // `W * log2_w_q12` fits easily: W ≤ 256, log2_w_q12 ≤ 8*4096.
        // We avoid a potential underflow by computing `(W * log2_w) - sum` / W.
        let numerator = (W as u64) * log2_w_q12;
        let h_q12 = if numerator >= sum_q12 {
            (numerator - sum_q12) / (W as u64)
        } else {
            // Shouldn't happen (Σ count = W, log2 is monotone), but
            // stay defensive: saturate at 0.
            0
        };
        // Normalise into Q15 by dividing by log2(B).
        let log2_b_q12 = LOG2_Q12[B] as u64;
        debug_assert!(log2_b_q12 > 0, "B must be >= 2");
        // h_norm = h_q12 / log2_b_q12, then scaled into Q15::MAX.
        // Compute as `h_q12 * Q15::MAX / log2_b_q12` to preserve
        // precision. Max numerator: 12288 * 32767 ≈ 4e8, fits u64.
        let scaled = h_q12 * (Q15::MAX.raw() as u64) / log2_b_q12;
        let clamped = if scaled > Q15::MAX.raw() as u64 {
            Q15::MAX.raw() as u64
        } else {
            scaled
        };
        Q15::from_raw(clamped as i16)
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// Place a `Q15` sample into one of `B` uniform bins over `[-1, 1]`.
#[inline]
fn bin_of<const B: usize>(sample: Q15) -> usize {
    // Shift Q15 raw from [-Q15::MAX, Q15::MAX] to [0, 2*Q15::MAX].
    let shifted = (sample.raw() as i32) + (Q15::MAX.raw() as i32);
    let span = 2 * (Q15::MAX.raw() as i32); // 65534
                                            // Linear partition; guard against the span edge with a clamp.
    let mut bin = ((shifted as u64) * (B as u64)) / (span as u64 + 1);
    if bin >= B as u64 {
        bin = (B - 1) as u64;
    }
    bin as usize
}

/// `|x|` with saturation — `Q15::MIN` (`-32767`) maps to `Q15::MAX`.
#[inline]
fn abs_q15(x: Q15) -> Q15 {
    let r = x.raw();
    if r >= 0 {
        x
    } else {
        // raw is in [-32767, -1]; negating stays in i16 range.
        Q15::from_raw(-r)
    }
}

/// `|a - b|` in Q15, saturating. Used for entropy-vs-baseline delta.
#[inline]
fn abs_q15_diff(a: Q15, b: Q15) -> Q15 {
    let d = (a.raw() as i32) - (b.raw() as i32);
    let mag = if d < 0 { -d } else { d };
    let clamped = if mag > Q15::MAX.raw() as i32 {
        Q15::MAX.raw() as i32
    } else {
        mag
    };
    Q15::from_raw(clamped as i16)
}

/// Integer square root of a `u32` via Newton iteration.
///
/// Returns `floor(sqrt(n))`. Terminates in ≤ 16 iterations for any
/// `u32` input (each step at least halves the error). `const fn`
/// compatible, no libm, no float.
///
/// Used by `umwelt-core::index` for the geometric-mean coherence
/// scalar (ADR-0017): `sqrt(m_a * m_b)` where both magnitudes are
/// `|Q15|` and the product fits `u32`.
#[inline]
pub(crate) const fn sqrt_u32(n: u32) -> u32 {
    if n == 0 {
        return 0;
    }
    // Initial guess: next power-of-two root. Bit tricks give us
    // `2^(ceil(log2(n+1))/2)` which is always >= floor(sqrt(n)).
    let shift = (32 - n.leading_zeros()).div_ceil(2);
    let mut x = 1u32 << shift;
    loop {
        let next = (x + n / x) / 2;
        if next >= x {
            return x;
        }
        x = next;
    }
}

/// EWMA step with α = 1/16: `next = prev + ((cur - prev) >> 4)`.
/// Integer-only, no division.
#[inline]
fn ewma_update(prev: Q15, cur: Q15) -> Q15 {
    let diff = (cur.raw() as i32) - (prev.raw() as i32);
    // Arithmetic shift-right preserves sign.
    let step = diff >> 4;
    let next = (prev.raw() as i32) + step;
    Q15::from_raw(next.clamp(Q15::MIN.raw() as i32, Q15::MAX.raw() as i32) as i16)
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // ── sqrt_u32 ─────────────────────────────────────────────────────────

    #[test]
    fn sqrt_u32_known_values() {
        assert_eq!(sqrt_u32(0), 0);
        assert_eq!(sqrt_u32(1), 1);
        assert_eq!(sqrt_u32(4), 2);
        assert_eq!(sqrt_u32(9), 3);
        assert_eq!(sqrt_u32(15), 3); // floor
        assert_eq!(sqrt_u32(16), 4);
        assert_eq!(sqrt_u32(u32::MAX), 65535);
        // Q15 product bound: 32767^2 = 1_073_676_289
        assert_eq!(sqrt_u32(32767u32 * 32767u32), 32767);
    }

    // ── log2 LUT sanity ────────────────────────────────────────────────────

    #[test]
    fn log2_lut_pow2_exact() {
        // Powers of two should be exact integer multiples of LOG2_Q_ONE.
        for shift in 0..=8u32 {
            let k = 1u32 << shift;
            assert_eq!(
                LOG2_Q12[k as usize],
                shift << LOG2_Q_SHIFT,
                "log2({k}) expected {} Q4.12",
                shift << LOG2_Q_SHIFT,
            );
        }
    }

    #[test]
    fn log2_lut_monotone() {
        for k in 2..LOG2_LUT_LEN {
            assert!(
                LOG2_Q12[k] >= LOG2_Q12[k - 1],
                "LOG2_Q12 not monotone at k={k}",
            );
        }
    }

    #[test]
    fn log2_lut_midpoints_within_1lsb() {
        // log2(3) ≈ 1.5849625; Q4.12 ≈ 6492. Within 1 LSB.
        assert!((LOG2_Q12[3] as i32 - 6492).abs() <= 1);
        // log2(6) ≈ 2.5849625; Q4.12 ≈ 10588.
        assert!((LOG2_Q12[6] as i32 - 10588).abs() <= 1);
        // log2(100) ≈ 6.643856; Q4.12 ≈ 27214.
        assert!((LOG2_Q12[100] as i32 - 27214).abs() <= 1);
        // log2(256) == 8 exactly.
        assert_eq!(LOG2_Q12[256], 8 << LOG2_Q_SHIFT);
    }

    // ── bin_of ──────────────────────────────────────────────────────────────

    #[test]
    fn bin_of_spans_all_bins() {
        // With B=8, equally-spaced samples hit all eight bins.
        let samples = [
            Q15::MIN,
            Q15::from_raw(-24575),
            Q15::from_raw(-16383),
            Q15::from_raw(-8191),
            Q15::from_raw(8191),
            Q15::from_raw(16383),
            Q15::from_raw(24575),
            Q15::MAX,
        ];
        let bins: [usize; 8] = core::array::from_fn(|i| bin_of::<8>(samples[i]));
        let mut seen = [false; 8];
        for b in bins {
            assert!(b < 8);
            seen[b] = true;
        }
        assert!(seen.iter().all(|&x| x), "bins: {bins:?}");
    }

    #[test]
    fn bin_of_extremes_saturate() {
        assert_eq!(bin_of::<8>(Q15::MIN), 0);
        assert_eq!(bin_of::<8>(Q15::MAX), 7);
    }

    // ── IconRing: shape ────────────────────────────────────────────────────

    #[test]
    fn no_fire_until_window_fills() {
        let mut r: IconRing<64, 8> = IconRing::new(0);
        for i in 0..63 {
            let s = Q15::from_raw((i * 300) as i16);
            assert!(r.observe(s, i as u64).is_none(), "fired at sample {i}");
        }
    }

    #[test]
    fn constant_stream_never_fires() {
        // A perfectly-constant stream has entropy = 0 and baseline = 0
        // forever, so |H - baseline| = 0 < delta. Never fires.
        let mut r: IconRing<64, 8> = IconRing::new(0);
        let s = Q15::from_raw(10_000);
        let mut fires = 0;
        for i in 0..500 {
            if r.observe(s, i as u64).is_some() {
                fires += 1;
            }
        }
        assert_eq!(fires, 0);
    }

    #[test]
    fn zero_variance_window_has_zero_entropy() {
        // Same-bin samples → all mass in one bin → H = 0.
        let mut r: IconRing<64, 8> = IconRing::new(0);
        let s = Q15::from_raw(10_000);
        for i in 0..64 {
            r.observe(s, i);
        }
        assert_eq!(r.entropy(), Q15::ZERO);
    }

    #[test]
    fn uniform_distribution_reaches_full_entropy() {
        // Fill the ring with one sample per bin, repeated until
        // exactly 8 samples per bin. Entropy should be ~Q15::MAX
        // (the normalisation cap).
        let mut r: IconRing<64, 8> = IconRing::new(0);
        // 8 values, each hitting a distinct bin, cycled 8 times.
        let bins: [Q15; 8] = [
            Q15::from_raw(-28000),
            Q15::from_raw(-20000),
            Q15::from_raw(-12000),
            Q15::from_raw(-4000),
            Q15::from_raw(4000),
            Q15::from_raw(12000),
            Q15::from_raw(20000),
            Q15::from_raw(28000),
        ];
        for tick in 0..64 {
            r.observe(bins[tick % 8], tick as u64);
        }
        let h = r.entropy();
        // Allow 1% slack from quantisation.
        assert!(
            (h.raw() as i32) >= (Q15::MAX.raw() as i32) - 328,
            "entropy {} not near max",
            h.raw()
        );
    }

    #[test]
    fn step_to_noise_eventually_fires() {
        // 128 constant samples to warm the baseline near 0, then
        // inject uniform noise. Entropy should jump; the trigger
        // fires at least once once the window holds enough noise.
        let mut r: IconRing<64, 8> = IconRing::new(7);
        let c = Q15::from_raw(5000);
        for i in 0..128u64 {
            r.observe(c, i);
        }
        let bins: [Q15; 8] = [
            Q15::from_raw(-28000),
            Q15::from_raw(-20000),
            Q15::from_raw(-12000),
            Q15::from_raw(-4000),
            Q15::from_raw(4000),
            Q15::from_raw(12000),
            Q15::from_raw(20000),
            Q15::from_raw(28000),
        ];
        let mut any_fire = false;
        let mut channel_ok = true;
        for tick in 128u64..(128 + 256) {
            if let Some(ev) = r.observe(bins[(tick as usize) % 8], tick) {
                any_fire = true;
                if ev.channel != 7 {
                    channel_ok = false;
                }
            }
        }
        assert!(any_fire, "no icon emitted after step to noise");
        assert!(channel_ok);
    }

    #[test]
    fn magnitude_gate_suppresses_low_signal() {
        // Even with high entropy, sub-threshold magnitude blocks firing.
        let mut r: IconRing<64, 8> = IconRing::with_thresholds(
            0,
            Q15::from_raw(10_000), // high gate: ~0.305
            DEFAULT_ENTROPY_DELTA_B8,
            DEFAULT_COOLDOWN_SAMPLES,
        );
        // Hand-picked small samples in different bins: entropy grows,
        // but none exceeds the gate.
        let small = [
            Q15::from_raw(-1000),
            Q15::from_raw(-500),
            Q15::from_raw(500),
            Q15::from_raw(1000),
        ];
        for tick in 0..512u64 {
            let _ = r.observe(small[(tick as usize) % 4], tick);
        }
        // A higher-entropy run at low magnitude should never fire.
        let mut fires = 0;
        for tick in 0..512u64 {
            if r.observe(small[(tick as usize) % 4], tick).is_some() {
                fires += 1;
            }
        }
        assert_eq!(fires, 0);
    }

    #[test]
    fn cooldown_gate_spaces_fires() {
        // Construct a ring with a tiny cooldown and a small delta so
        // the trigger is eager, then confirm no two fires are
        // adjacent.
        let mut r: IconRing<64, 8> = IconRing::with_thresholds(
            1,
            DEFAULT_MAGNITUDE_GATE,
            Q15::from_raw(100), // very small delta — easy to trip
            6,
        );
        let bins: [Q15; 8] = [
            Q15::from_raw(-28000),
            Q15::from_raw(-20000),
            Q15::from_raw(-12000),
            Q15::from_raw(-4000),
            Q15::from_raw(4000),
            Q15::from_raw(12000),
            Q15::from_raw(20000),
            Q15::from_raw(28000),
        ];
        // Alternate noise and constant to keep the baseline chasing.
        let c = Q15::from_raw(10_000);
        for tick in 0..200u64 {
            let s = if (tick / 8) % 2 == 0 {
                bins[(tick as usize) % 8]
            } else {
                c
            };
            r.observe(s, tick);
        }
        let mut last_fire: Option<u64> = None;
        for tick in 200u64..1000u64 {
            let s = if (tick / 8) % 2 == 0 {
                bins[(tick as usize) % 8]
            } else {
                c
            };
            if let Some(ev) = r.observe(s, tick) {
                if let Some(prev) = last_fire {
                    assert!(
                        ev.formed_at - prev >= 6,
                        "cooldown violated: {prev} -> {}",
                        ev.formed_at
                    );
                }
                last_fire = Some(ev.formed_at);
            }
        }
    }

    // ── Proptest invariants ────────────────────────────────────────────────

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Entropy is always a valid Q15 fraction in `[0, 1]` of
        /// `log2(B)` bits, regardless of the sample sequence.
        #[test]
        fn entropy_in_unit_range(
            samples in proptest::collection::vec(any::<i16>(), 0..512),
        ) {
            let mut r: IconRing<64, 8> = IconRing::new(0);
            for (i, &raw) in samples.iter().enumerate() {
                r.observe(Q15::from_raw(raw), i as u64);
                let h = r.entropy();
                prop_assert!(h.raw() >= 0, "entropy negative: {}", h.raw());
                prop_assert!(h.raw() <= Q15::MAX.raw(), "entropy > MAX: {}", h.raw());
            }
        }

        /// `observe` never panics on arbitrary input, and any emitted
        /// event's magnitude is saturating (non-negative, ≤ Q15::MAX)
        /// and channel is preserved verbatim from construction.
        #[test]
        fn observe_never_panics_and_event_invariants_hold(
            samples in proptest::collection::vec(any::<i16>(), 0..512),
            channel in any::<u16>(),
        ) {
            let mut r: IconRing<64, 8> = IconRing::new(channel);
            for (i, &raw) in samples.iter().enumerate() {
                if let Some(ev) = r.observe(Q15::from_raw(raw), i as u64) {
                    prop_assert!(ev.magnitude.raw() >= 0);
                    prop_assert!(ev.magnitude.raw() <= Q15::MAX.raw());
                    prop_assert!(ev.entropy.raw() >= 0);
                    prop_assert!(ev.entropy.raw() <= Q15::MAX.raw());
                    prop_assert_eq!(ev.channel, channel);
                    prop_assert_eq!(ev.formed_at, i as u64);
                }
            }
        }

        /// Zero-variance windows (all samples in one bin) produce
        /// zero entropy — the identity case of the Shannon sum.
        #[test]
        fn zero_variance_identity(
            raw in any::<i16>(),
        ) {
            let mut r: IconRing<64, 8> = IconRing::new(0);
            let s = Q15::from_raw(raw);
            for i in 0..128u64 {
                r.observe(s, i);
            }
            prop_assert_eq!(r.entropy(), Q15::ZERO);
        }
    }
}

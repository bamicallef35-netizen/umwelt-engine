//! SEP inner-frame codec (P2.1).
//!
//! Canonical 24-byte, little-endian, sign-per-frame wire format.
//! See ADR-0011 for the binding layout; this module implements the
//! `encode` / `decode` halves of its implementation contract.
//!
//! ```text
//!   offset  size  field
//!   ──────  ────  ──────────────────────────────────────────────
//!     0      4    agent_id   : u32   (LE)
//!     4      1    sign_type  : u8
//!     5      1    magnitude  : u8
//!     6      1    ttl_flags  : u8   (high nibble = ttl,
//!                                    low  nibble = flags)
//!     7      1    _reserved0 : u8   (MUST be 0 on send)
//!     8      2    spatial_x  : i16  (cm, LE)
//!    10      2    spatial_y  : i16  (cm, LE)
//!    12      2    spatial_z  : i16  (cm, LE)
//!    14      2    _reserved1 : u16  (MUST be 0 on send)
//!    16      4    timestamp  : u32  (ms, LE)
//!    20      1    confidence : u8
//!    21      1    priority   : u8
//!    22      2    crc16      : u16  (CCITT 0x1021 init 0xFFFF
//!                                    over bytes 0..22, LE)
//!   ──────
//!    24
//! ```
//!
//! Both `encode` and `decode` are allocation-free, `no_std`, and
//! branch-light. Reserved bytes are emitted as zero on encode and
//! accepted as any value on decode per ADR-0011.

use crate::SEP_FRAME_BYTES;

// ── Flag layout (low nibble of byte 6) ───────────────────────────────────

/// `ttl_flags.low_nibble` bit 0 — urgent delivery hint.
pub const FLAG_URGENT: u8 = 0b0000_0001;
/// `ttl_flags.low_nibble` bit 1 — sign was produced by a composite rule.
pub const FLAG_COMPOSITE: u8 = 0b0000_0010;
/// Mask for all flag bits the decoder recognises. Bits 2–3 of the low
/// nibble are reserved and ignored.
pub const FLAGS_MASK: u8 = 0b0000_1111;

// ── Error type ─────────────────────────────────────────────────────────

/// Reasons [`decode`] can fail.
///
/// `no_std`-safe: plain-enum, `Copy`, no allocation, no payload other
/// than what the caller already has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SepError {
    /// CRC-16/CCITT over bytes 0..22 did not match the value stored at
    /// offset 22. The frame is corrupt and MUST be dropped.
    ///
    /// Carries `(computed, stored)` so callers can log or metric the
    /// mismatch without recomputing.
    CrcMismatch {
        /// CRC computed over the decoded bytes 0..22.
        computed: u16,
        /// CRC read from the frame at offset 22.
        stored: u16,
    },
}

// ── Frame struct ─────────────────────────────────────────────────────────

/// In-memory mirror of the ADR-0011 wire frame.
///
/// Field order intentionally follows the wire layout so a reader can
/// cross-reference against the spec table without jumping around.
/// All multi-byte fields are little-endian on the wire; the struct
/// holds their native representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SepFrame {
    /// CRC32 of the emitting agent's MAC address. See ADR-0011.
    pub agent_id: u32,
    /// Vocabulary index of the sign's type. See P0.5.11.
    pub sign_type: u8,
    /// Unsigned Q0.8 magnitude — how "strong" this sign fired.
    pub magnitude: u8,
    /// Hop counter, `0..=15`. A sender emits the initial TTL; every
    /// relay decrements. At zero the frame is dropped, not forwarded.
    pub ttl: u8,
    /// Low-nibble bitmask: [`FLAG_URGENT`], [`FLAG_COMPOSITE`].
    /// Unrecognised bits are preserved but have no semantics.
    pub flags: u8,
    /// Spatial reference in agent-local centimetres (`±327.67 m`).
    /// `[x, y, z]` matches the wire order.
    pub spatial: [i16; 3],
    /// Agent-local monotonic timestamp, milliseconds since agent epoch.
    /// Wraps every ~49.7 days.
    pub timestamp_ms: u32,
    /// Unsigned Q0.8 confidence the semiogenesis layer assigned.
    pub confidence: u8,
    /// Unsigned Q0.8 relay priority hint for the mesh.
    pub priority: u8,
}

impl SepFrame {
    /// All-zero frame. Useful as a builder base in tests.
    pub const ZERO: Self = Self {
        agent_id: 0,
        sign_type: 0,
        magnitude: 0,
        ttl: 0,
        flags: 0,
        spatial: [0, 0, 0],
        timestamp_ms: 0,
        confidence: 0,
        priority: 0,
    };
}

// ── CRC-16/CCITT ─────────────────────────────────────────────────────────

/// CRC-16/CCITT (poly 0x1021, init 0xFFFF, no reflect, no xor-out).
///
/// Bit-by-bit implementation — tiny footprint (<100 bytes compiled on
/// ARMv6-M), no table, no external deps. At 22 bytes per frame the
/// per-call cost is negligible compared to the transport layer.
#[inline]
pub fn crc16_ccitt(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in bytes {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

// ── Codec ────────────────────────────────────────────────────────────────

/// Encode `frame` into `out` per ADR-0011. Reserved bytes (offsets 7
/// and 14–15) are explicitly zeroed — the caller's buffer may hold
/// arbitrary prior content.
///
/// The `flags` field is masked against [`FLAGS_MASK`] so bits that do
/// not fit in the low nibble cannot corrupt the TTL nibble above them.
/// `ttl` is masked to 4 bits for the same reason.
#[inline]
pub fn encode(frame: &SepFrame, out: &mut [u8; SEP_FRAME_BYTES]) {
    // Bytes 0..4: agent_id (LE).
    out[0..4].copy_from_slice(&frame.agent_id.to_le_bytes());
    // Bytes 4, 5: sign_type, magnitude.
    out[4] = frame.sign_type;
    out[5] = frame.magnitude;
    // Byte 6: ttl_flags. High nibble = ttl (clamped to 4 bits), low
    // nibble = flags (clamped to 4 bits).
    let ttl_n = frame.ttl & 0x0F;
    let flags_n = frame.flags & FLAGS_MASK;
    out[6] = (ttl_n << 4) | flags_n;
    // Byte 7: reserved — MUST be zero.
    out[7] = 0;
    // Bytes 8..14: spatial x, y, z (i16 LE).
    out[8..10].copy_from_slice(&frame.spatial[0].to_le_bytes());
    out[10..12].copy_from_slice(&frame.spatial[1].to_le_bytes());
    out[12..14].copy_from_slice(&frame.spatial[2].to_le_bytes());
    // Bytes 14..16: reserved — MUST be zero.
    out[14] = 0;
    out[15] = 0;
    // Bytes 16..20: timestamp (u32 LE, ms).
    out[16..20].copy_from_slice(&frame.timestamp_ms.to_le_bytes());
    // Bytes 20, 21: confidence, priority.
    out[20] = frame.confidence;
    out[21] = frame.priority;
    // Bytes 22..24: CRC-16/CCITT over 0..22 (LE).
    let crc = crc16_ccitt(&out[0..22]);
    out[22..24].copy_from_slice(&crc.to_le_bytes());
}

/// Decode a 24-byte SEP frame. Verifies the trailing CRC and returns
/// the parsed [`SepFrame`] on match; otherwise [`SepError::CrcMismatch`].
///
/// Reserved bytes (offsets 7 and 14–15) are accepted at any value per
/// ADR-0011; non-zero values are not signalled as errors.
#[inline]
pub fn decode(bytes: &[u8; SEP_FRAME_BYTES]) -> Result<SepFrame, SepError> {
    let stored = u16::from_le_bytes([bytes[22], bytes[23]]);
    let computed = crc16_ccitt(&bytes[0..22]);
    if stored != computed {
        return Err(SepError::CrcMismatch { computed, stored });
    }
    let ttl_flags = bytes[6];
    Ok(SepFrame {
        agent_id: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        sign_type: bytes[4],
        magnitude: bytes[5],
        ttl: (ttl_flags >> 4) & 0x0F,
        flags: ttl_flags & 0x0F,
        spatial: [
            i16::from_le_bytes([bytes[8], bytes[9]]),
            i16::from_le_bytes([bytes[10], bytes[11]]),
            i16::from_le_bytes([bytes[12], bytes[13]]),
        ],
        timestamp_ms: u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]),
        confidence: bytes[20],
        priority: bytes[21],
    })
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SepFrame {
        SepFrame {
            agent_id: 0xDEAD_BEEF,
            sign_type: 42,
            magnitude: 200,
            ttl: 0xE, // 14 — close to drop
            flags: FLAG_URGENT | FLAG_COMPOSITE,
            spatial: [-100, 50, 1_234],
            timestamp_ms: 0x1234_5678,
            confidence: 180,
            priority: 64,
        }
    }

    #[test]
    fn roundtrip_smoke() {
        let f = sample();
        let mut buf = [0u8; SEP_FRAME_BYTES];
        encode(&f, &mut buf);
        let decoded = decode(&buf).unwrap();
        assert_eq!(decoded, f);
    }

    #[test]
    fn encode_zeroes_reserved_bytes() {
        // Pre-fill the buffer with garbage so we can prove `encode`
        // actively clears the reserved slots rather than relying on the
        // caller having zeroed the buffer first.
        let mut buf = [0xAAu8; SEP_FRAME_BYTES];
        encode(&sample(), &mut buf);
        assert_eq!(buf[7], 0, "byte 7 must be zero per ADR-0011");
        assert_eq!(buf[14], 0, "byte 14 must be zero per ADR-0011");
        assert_eq!(buf[15], 0, "byte 15 must be zero per ADR-0011");
    }

    #[test]
    fn encode_lays_out_fields_at_correct_offsets() {
        let f = sample();
        let mut buf = [0u8; SEP_FRAME_BYTES];
        encode(&f, &mut buf);
        // agent_id at 0..4 (LE).
        assert_eq!(&buf[0..4], &0xDEAD_BEEFu32.to_le_bytes());
        // sign_type, magnitude at 4, 5.
        assert_eq!(buf[4], 42);
        assert_eq!(buf[5], 200);
        // ttl_flags at 6: high nibble ttl=0xE, low nibble flags=0b11.
        assert_eq!(buf[6], 0xE3);
        // spatial x at 8..10 (LE).
        assert_eq!(&buf[8..10], &(-100i16).to_le_bytes());
        // timestamp at 16..20 (LE).
        assert_eq!(&buf[16..20], &0x1234_5678u32.to_le_bytes());
        // confidence, priority at 20, 21.
        assert_eq!(buf[20], 180);
        assert_eq!(buf[21], 64);
    }

    #[test]
    fn decode_accepts_nonzero_reserved_bytes() {
        // ADR-0011: reserved bytes are ignored on receive.
        let f = sample();
        let mut buf = [0u8; SEP_FRAME_BYTES];
        encode(&f, &mut buf);
        // Poke non-zero values into the reserved slots *before* the
        // CRC region — recompute CRC so the frame as a whole still
        // verifies.
        buf[7] = 0xFF;
        buf[14] = 0x12;
        buf[15] = 0x34;
        let crc = crc16_ccitt(&buf[0..22]);
        buf[22..24].copy_from_slice(&crc.to_le_bytes());
        let decoded = decode(&buf).unwrap();
        assert_eq!(decoded, f);
    }

    #[test]
    fn decode_rejects_bit_flip_via_crc() {
        let f = sample();
        let mut buf = [0u8; SEP_FRAME_BYTES];
        encode(&f, &mut buf);
        // Flip a bit in a semantically-loaded byte.
        buf[0] ^= 0x01;
        let err = decode(&buf).unwrap_err();
        match err {
            SepError::CrcMismatch { computed, stored } => {
                assert_ne!(
                    computed, stored,
                    "CrcMismatch must carry the observed delta"
                );
            }
        }
    }

    #[test]
    fn decode_rejects_crc_tail_flip() {
        // Flip a bit inside the CRC tail itself — the computed CRC
        // over 0..22 will not change, but the stored value will.
        let f = sample();
        let mut buf = [0u8; SEP_FRAME_BYTES];
        encode(&f, &mut buf);
        buf[23] ^= 0x01;
        assert!(matches!(decode(&buf), Err(SepError::CrcMismatch { .. })));
    }

    #[test]
    fn flags_mask_drops_unknown_bits_on_encode() {
        // Sender passes garbage in the upper bits of `flags`. The
        // encoder must mask them down to the low nibble so they cannot
        // spill into the TTL nibble above.
        let f = SepFrame {
            flags: 0b1111_0011,
            ttl: 0x5,
            ..SepFrame::ZERO
        };
        let mut buf = [0u8; SEP_FRAME_BYTES];
        encode(&f, &mut buf);
        // High nibble stays at ttl=5, low nibble keeps only 0b0011.
        assert_eq!(buf[6], 0x53);
    }

    #[test]
    fn ttl_mask_keeps_high_nibble_contained() {
        // Garbage in the high bits of `ttl` must not bleed across byte 6.
        let f = SepFrame {
            ttl: 0xFF,
            flags: 0x01,
            ..SepFrame::ZERO
        };
        let mut buf = [0u8; SEP_FRAME_BYTES];
        encode(&f, &mut buf);
        assert_eq!(buf[6], 0xF1, "ttl clamped to 4 bits, flags untouched");
    }

    #[test]
    fn zero_frame_roundtrip() {
        let mut buf = [0u8; SEP_FRAME_BYTES];
        encode(&SepFrame::ZERO, &mut buf);
        let decoded = decode(&buf).unwrap();
        assert_eq!(decoded, SepFrame::ZERO);
    }

    // ── CRC-16/CCITT against a known vector ───────────────────────────

    #[test]
    fn crc16_ccitt_known_vector_empty() {
        // CCITT-FALSE: CRC of the empty string is the initial value.
        assert_eq!(crc16_ccitt(&[]), 0xFFFF);
    }

    #[test]
    fn crc16_ccitt_known_vector_ascii_a() {
        // CRC-16/CCITT-FALSE("A") = 0xB915 (well-known test vector).
        assert_eq!(crc16_ccitt(b"A"), 0xB915);
    }

    #[test]
    fn crc16_ccitt_known_vector_123456789() {
        // CRC-16/CCITT-FALSE("123456789") = 0x29B1 (canonical vector).
        assert_eq!(crc16_ccitt(b"123456789"), 0x29B1);
    }

    // ── P2.2: round-trip property tests ───────────────────────────────────

    use proptest::prelude::*;

    /// Generate a `SepFrame` whose every field already sits inside the
    /// wire-format surface (ttl ≤ 15, flags ≤ 15). The codec also
    /// masks these on encode, but generating in-range values lets us
    /// assert `decode(encode(f)) == f` without first normalising `f`.
    fn arb_inrange_frame() -> impl Strategy<Value = SepFrame> {
        (
            any::<u32>(), // agent_id
            any::<u8>(),  // sign_type
            any::<u8>(),  // magnitude
            0u8..=15,     // ttl (4-bit)
            0u8..=15,     // flags (4-bit, incl. reserved bits 2–3)
            any::<i16>(),
            any::<i16>(),
            any::<i16>(), // spatial
            any::<u32>(), // timestamp_ms
            any::<u8>(),  // confidence
            any::<u8>(),  // priority
        )
            .prop_map(
                |(agent_id, sign_type, magnitude, ttl, flags, sx, sy, sz, ts, conf, pri)| {
                    SepFrame {
                        agent_id,
                        sign_type,
                        magnitude,
                        ttl,
                        flags,
                        spatial: [sx, sy, sz],
                        timestamp_ms: ts,
                        confidence: conf,
                        priority: pri,
                    }
                },
            )
    }

    proptest! {
        /// Invariant: for any in-range frame, encode then decode returns
        /// the same frame. The core correctness guarantee of the codec.
        #[test]
        fn roundtrip_inrange(f in arb_inrange_frame()) {
            let mut buf = [0u8; SEP_FRAME_BYTES];
            encode(&f, &mut buf);
            let got = decode(&buf).expect("CRC must verify on self-produced frame");
            prop_assert_eq!(got, f);
        }

        /// Invariant: `encode` always produces a frame that verifies.
        /// This is a weaker sibling of `roundtrip_inrange` — it covers
        /// the buffer-sanitisation path by starting from non-zero
        /// garbage in `buf`, so we also prove encode cleared reserved
        /// bytes even when the caller's memory was dirty.
        #[test]
        fn encode_produces_valid_crc_over_dirty_buffer(
            f in arb_inrange_frame(),
            garbage in any::<[u8; SEP_FRAME_BYTES]>(),
        ) {
            let mut buf = garbage;
            encode(&f, &mut buf);
            let decoded = decode(&buf).expect("encode must always yield a valid CRC");
            prop_assert_eq!(decoded, f);
            // Reserved bytes are always zero post-encode, regardless of
            // prior buffer content.
            prop_assert_eq!(buf[7], 0);
            prop_assert_eq!(buf[14], 0);
            prop_assert_eq!(buf[15], 0);
        }

        /// Invariant: garbage bits in the high nibble of `ttl` or the
        /// high nibble of `flags` are masked off, not propagated.
        /// Gives the encoder a no-panic / no-overflow contract on
        /// callers that don't pre-mask.
        #[test]
        fn encode_masks_out_of_range_ttl_and_flags(
            mut f in arb_inrange_frame(),
            ttl_high in any::<u8>(),
            flags_high in any::<u8>(),
        ) {
            // Smear arbitrary bits into the high nibbles.
            f.ttl = (f.ttl & 0x0F) | (ttl_high & 0xF0);
            f.flags = (f.flags & 0x0F) | (flags_high & 0xF0);
            let mut buf = [0u8; SEP_FRAME_BYTES];
            encode(&f, &mut buf);
            // Byte 6 must still parse as [ttl_low | flags_low].
            let decoded = decode(&buf).unwrap();
            prop_assert_eq!(decoded.ttl, f.ttl & 0x0F);
            prop_assert_eq!(decoded.flags, f.flags & 0x0F);
        }

        /// Invariant: `decode` never panics — on arbitrary 24-byte
        /// input it either succeeds with a well-formed `SepFrame` or
        /// returns `SepError::CrcMismatch`. Crucial for fuzz safety
        /// (P2.6) and for hostile-network robustness generally.
        #[test]
        fn decode_never_panics_on_arbitrary_input(
            bytes in any::<[u8; SEP_FRAME_BYTES]>(),
        ) {
            match decode(&bytes) {
                Ok(_) | Err(SepError::CrcMismatch { .. }) => {}
            }
        }

        /// Invariant: every single-bit flip in the 24-byte frame is
        /// caught by the CRC. CCITT's Hamming distance at this length
        /// guarantees it; this test is the empirical confirmation.
        #[test]
        fn every_single_bit_flip_is_caught(
            f in arb_inrange_frame(),
            bit in 0usize..(SEP_FRAME_BYTES * 8),
        ) {
            let mut buf = [0u8; SEP_FRAME_BYTES];
            encode(&f, &mut buf);
            let (byte_idx, bit_idx) = (bit / 8, bit % 8);
            buf[byte_idx] ^= 1 << bit_idx;
            // Either the flipped CRC no longer matches (bytes 22–23
            // region), or the flipped payload no longer matches the
            // stored CRC — in both cases decode returns CrcMismatch.
            let is_err = matches!(decode(&buf), Err(SepError::CrcMismatch { .. }));
            prop_assert!(is_err, "single-bit flip at bit {} not detected", bit);
        }

        /// Invariant: non-zero reserved bytes never cause decode to
        /// reject a frame whose CRC still verifies — ADR-0011 says
        /// reserved-byte values are ignored on receive.
        #[test]
        fn reserved_bytes_do_not_affect_decode(
            f in arb_inrange_frame(),
            r0 in any::<u8>(),
            r1 in any::<u8>(),
            r2 in any::<u8>(),
        ) {
            let mut buf = [0u8; SEP_FRAME_BYTES];
            encode(&f, &mut buf);
            buf[7] = r0;
            buf[14] = r1;
            buf[15] = r2;
            // Recompute CRC so the frame remains self-consistent.
            let crc = crc16_ccitt(&buf[0..22]);
            buf[22..24].copy_from_slice(&crc.to_le_bytes());
            let decoded = decode(&buf).unwrap();
            prop_assert_eq!(decoded, f);
        }
    }
}

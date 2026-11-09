//! Sign Exchange Protocol.
//!
//! Peer-to-peer sign propagation. Hop-resilient, partition-tolerant,
//! transport-agnostic. Wire format is pinned by ADR-0011 (24-byte
//! inner frame) and ADR-0011a (32-byte authenticated envelope).

#![cfg_attr(not(feature = "std"), no_std)]
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod frame;
pub mod relay;
pub mod replay;

pub use frame::{
    crc16_ccitt, decode, encode, SepError, SepFrame, FLAGS_MASK, FLAG_COMPOSITE, FLAG_URGENT,
};
pub use relay::{relay as relay_frame, seen_key, RelayAction, Seen, SeenKey, SeenSet};
pub use replay::{ReplayGuard, ReplayVerdict};

/// Protocol version. Bump on any wire-format change.
pub const SEP_VERSION: u8 = 1;

/// Length of the canonical SEP inner frame in bytes.
///
/// See ADR-0011. The inner frame carries one interpreted sign per
/// transmission, fixed-width, little-endian.
pub const SEP_FRAME_BYTES: usize = 24;

/// Length of the SEP authenticated envelope in bytes.
///
/// See ADR-0011a. The envelope is the 24-byte inner frame followed by
/// an 8-byte truncated BLAKE3-keyed MAC (see ADR-0012). Transports
/// that run on untrusted networks carry envelopes; inner-only
/// transports (loopback, tests) carry the inner frame directly.
pub const SEP_ENVELOPE_BYTES: usize = 32;

/// Placeholder — real frame layout comes in P2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Protocol version; always `SEP_VERSION` at encode time.
    pub version: u8,
    /// Source node id.
    pub src: u32,
    /// Hop counter; increment on relay, drop at `MAX_HOPS`.
    pub hops: u8,
}

// ── Transport trait (P0.5.12) ───────────────────────────────────────────────

/// Errors raised by a [`SepTransport`].
///
/// `no_std`-safe. Transport implementations map their native errors
/// into this enum; no `std::io::Error` variant is carried, since the
/// trait lives in a `no_std` crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SepTransportError {
    /// The supplied buffer is too small for one SEP frame in the form
    /// this transport carries.
    BufferTooSmall,
    /// No frame is available right now. Non-fatal; the caller may
    /// retry later.
    WouldBlock,
    /// The transport has been shut down or its peer has disconnected.
    Closed,
    /// The frame length is not a valid SEP size (neither
    /// [`SEP_FRAME_BYTES`] nor [`SEP_ENVELOPE_BYTES`]) or does not
    /// match the form this transport was constructed for.
    InvalidFrameLen,
    /// Transport-specific I/O failure. Carries no payload to stay
    /// `no_std`-compatible.
    Io,
}

/// Which wire form a [`SepTransport`] carries.
///
/// Per ADR-0011a, a transport declares inner-only or envelope at
/// construction; the two are not mixed on one wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameForm {
    /// 24-byte inner frames only. Used for in-process loopback and
    /// unit-test harnesses where no authentication is required.
    Inner,
    /// 32-byte authenticated envelopes. Used for any shared-medium
    /// transport (UDP, LoRa, ESP-NOW).
    Envelope,
}

impl FrameForm {
    /// The exact byte length of one frame in this form.
    #[inline]
    pub const fn bytes(self) -> usize {
        match self {
            FrameForm::Inner => SEP_FRAME_BYTES,
            FrameForm::Envelope => SEP_ENVELOPE_BYTES,
        }
    }
}

/// A bidirectional byte-slice transport for SEP frames.
///
/// One call, one frame. Implementations are responsible for buffering
/// (for queue-like transports) or for bounding retransmission / loss
/// (for lossy transports). This trait does not mandate either.
pub trait SepTransport {
    /// The form (inner / envelope) this transport carries. Fixed at
    /// construction time.
    fn form(&self) -> FrameForm;

    /// Send one SEP frame. `frame` must be exactly
    /// [`FrameForm::bytes`] of [`Self::form`]; other lengths return
    /// [`SepTransportError::InvalidFrameLen`].
    fn send(&mut self, frame: &[u8]) -> Result<(), SepTransportError>;

    /// Receive one SEP frame into `buf`, returning how many bytes
    /// were written. `buf` must have room for at least
    /// [`FrameForm::bytes`] of [`Self::form`]; smaller buffers return
    /// [`SepTransportError::BufferTooSmall`] without consuming a
    /// frame. If the transport has no frame queued, returns
    /// [`SepTransportError::WouldBlock`].
    fn recv(&mut self, buf: &mut [u8]) -> Result<usize, SepTransportError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_form_bytes_match_constants() {
        assert_eq!(FrameForm::Inner.bytes(), SEP_FRAME_BYTES);
        assert_eq!(FrameForm::Envelope.bytes(), SEP_ENVELOPE_BYTES);
        assert_eq!(SEP_FRAME_BYTES, 24);
        assert_eq!(SEP_ENVELOPE_BYTES, 32);
    }

    #[test]
    fn sep_transport_error_is_copy() {
        let e = SepTransportError::WouldBlock;
        let _f = e; // copy
        assert_eq!(e, SepTransportError::WouldBlock);
    }
}

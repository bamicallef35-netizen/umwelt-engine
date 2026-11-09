//! Bounded in-memory loopback transport.
//!
//! Frames sent via [`LoopbackTransport::send`] are buffered; calls to
//! [`LoopbackTransport::recv`] return them FIFO. Used for intra-process
//! test harnesses and the unit-test surface.
//!
//! Per ADR-0011a the transport declares its [`FrameForm`] at
//! construction; mixing inner and envelope frames on one instance is a
//! configuration error and is rejected by `send`.

use heapless::Deque;

use umwelt_sep::{FrameForm, SepTransport, SepTransportError, SEP_ENVELOPE_BYTES};

/// In-memory loopback transport with a bounded frame queue of capacity `N`.
///
/// Each entry is a fixed 32-byte buffer (envelope size) alongside the
/// actual frame length — inner frames are padded on send, and `recv`
/// returns only the real length.
pub struct LoopbackTransport<const N: usize> {
    form: FrameForm,
    queue: Deque<(u8, [u8; SEP_ENVELOPE_BYTES]), N>,
}

impl<const N: usize> LoopbackTransport<N> {
    /// Construct a loopback that carries 24-byte inner frames only.
    pub const fn new_inner() -> Self {
        Self {
            form: FrameForm::Inner,
            queue: Deque::new(),
        }
    }

    /// Construct a loopback that carries 32-byte envelopes only.
    pub const fn new_envelope() -> Self {
        Self {
            form: FrameForm::Envelope,
            queue: Deque::new(),
        }
    }

    /// Number of queued frames.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// `true` if no frames are queued.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Capacity bound of the queue (the `N` type parameter).
    pub const fn capacity(&self) -> usize {
        N
    }
}

impl<const N: usize> SepTransport for LoopbackTransport<N> {
    fn form(&self) -> FrameForm {
        self.form
    }

    fn send(&mut self, frame: &[u8]) -> Result<(), SepTransportError> {
        if frame.len() != self.form.bytes() {
            return Err(SepTransportError::InvalidFrameLen);
        }
        let mut buf = [0u8; SEP_ENVELOPE_BYTES];
        buf[..frame.len()].copy_from_slice(frame);
        self.queue
            .push_back((frame.len() as u8, buf))
            .map_err(|_| SepTransportError::Io)
    }

    fn recv(&mut self, out: &mut [u8]) -> Result<usize, SepTransportError> {
        // Reject small buffers *before* consuming a frame so callers
        // can retry with a bigger buffer without losing data.
        if out.len() < self.form.bytes() {
            return Err(SepTransportError::BufferTooSmall);
        }
        let (len, buf) = self
            .queue
            .pop_front()
            .ok_or(SepTransportError::WouldBlock)?;
        let len = len as usize;
        out[..len].copy_from_slice(&buf[..len]);
        Ok(len)
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use umwelt_sep::SEP_FRAME_BYTES;

    fn sample_inner() -> [u8; SEP_FRAME_BYTES] {
        let mut f = [0u8; SEP_FRAME_BYTES];
        for (i, b) in f.iter_mut().enumerate() {
            *b = i as u8;
        }
        f
    }

    fn sample_envelope() -> [u8; SEP_ENVELOPE_BYTES] {
        let mut f = [0u8; SEP_ENVELOPE_BYTES];
        for (i, b) in f.iter_mut().enumerate() {
            *b = (i as u8).wrapping_add(0x80);
        }
        f
    }

    #[test]
    fn inner_form_round_trip_preserves_bytes() {
        let mut t: LoopbackTransport<4> = LoopbackTransport::new_inner();
        let frame = sample_inner();
        t.send(&frame).unwrap();
        let mut out = [0u8; SEP_FRAME_BYTES];
        let n = t.recv(&mut out).unwrap();
        assert_eq!(n, SEP_FRAME_BYTES);
        assert_eq!(out, frame);
    }

    #[test]
    fn envelope_form_round_trip_preserves_bytes() {
        let mut t: LoopbackTransport<4> = LoopbackTransport::new_envelope();
        let frame = sample_envelope();
        t.send(&frame).unwrap();
        let mut out = [0u8; SEP_ENVELOPE_BYTES];
        let n = t.recv(&mut out).unwrap();
        assert_eq!(n, SEP_ENVELOPE_BYTES);
        assert_eq!(out, frame);
    }

    #[test]
    fn fifo_order_preserved() {
        let mut t: LoopbackTransport<4> = LoopbackTransport::new_inner();
        let mut a = [0u8; SEP_FRAME_BYTES];
        a[0] = 0xAA;
        let mut b = [0u8; SEP_FRAME_BYTES];
        b[0] = 0xBB;
        let mut c = [0u8; SEP_FRAME_BYTES];
        c[0] = 0xCC;
        t.send(&a).unwrap();
        t.send(&b).unwrap();
        t.send(&c).unwrap();

        let mut out = [0u8; SEP_FRAME_BYTES];
        t.recv(&mut out).unwrap();
        assert_eq!(out[0], 0xAA);
        t.recv(&mut out).unwrap();
        assert_eq!(out[0], 0xBB);
        t.recv(&mut out).unwrap();
        assert_eq!(out[0], 0xCC);
    }

    #[test]
    fn wrong_length_rejected_on_send() {
        let mut t: LoopbackTransport<4> = LoopbackTransport::new_inner();
        let too_short = [0u8; 16];
        assert_eq!(t.send(&too_short), Err(SepTransportError::InvalidFrameLen));
        let envelope_len = [0u8; SEP_ENVELOPE_BYTES];
        assert_eq!(
            t.send(&envelope_len),
            Err(SepTransportError::InvalidFrameLen)
        );
    }

    #[test]
    fn mixing_forms_rejected() {
        let mut inner: LoopbackTransport<4> = LoopbackTransport::new_inner();
        // An envelope-sized frame pushed into an inner-only transport
        // must be rejected.
        let env = sample_envelope();
        assert_eq!(inner.send(&env), Err(SepTransportError::InvalidFrameLen));

        let mut env_t: LoopbackTransport<4> = LoopbackTransport::new_envelope();
        // An inner-sized frame pushed into an envelope transport must
        // likewise be rejected.
        let inner_frame = sample_inner();
        assert_eq!(
            env_t.send(&inner_frame),
            Err(SepTransportError::InvalidFrameLen)
        );
    }

    #[test]
    fn recv_empty_returns_would_block() {
        let mut t: LoopbackTransport<4> = LoopbackTransport::new_inner();
        let mut out = [0u8; SEP_FRAME_BYTES];
        assert_eq!(t.recv(&mut out), Err(SepTransportError::WouldBlock));
    }

    #[test]
    fn recv_into_small_buffer_preserves_frame() {
        let mut t: LoopbackTransport<4> = LoopbackTransport::new_inner();
        let frame = sample_inner();
        t.send(&frame).unwrap();

        let mut small = [0u8; 8];
        assert_eq!(t.recv(&mut small), Err(SepTransportError::BufferTooSmall));
        // The frame must still be queued.
        let mut out = [0u8; SEP_FRAME_BYTES];
        let n = t.recv(&mut out).unwrap();
        assert_eq!(n, SEP_FRAME_BYTES);
        assert_eq!(out, frame);
    }

    #[test]
    fn capacity_overflow_returns_io() {
        let mut t: LoopbackTransport<2> = LoopbackTransport::new_inner();
        let frame = sample_inner();
        t.send(&frame).unwrap();
        t.send(&frame).unwrap();
        // Third push exceeds capacity.
        assert_eq!(t.send(&frame), Err(SepTransportError::Io));
    }

    #[test]
    fn form_reports_construction_choice() {
        let a: LoopbackTransport<2> = LoopbackTransport::new_inner();
        assert_eq!(a.form(), FrameForm::Inner);
        let b: LoopbackTransport<2> = LoopbackTransport::new_envelope();
        assert_eq!(b.form(), FrameForm::Envelope);
    }
}

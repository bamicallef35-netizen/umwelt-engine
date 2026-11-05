//! N-agent loopback fabric (P4.1).
//!
//! [`LoopbackMesh`] is a bounded in-memory multi-agent transport. Each
//! agent owns a FIFO inbox of capacity `Q`; sends enqueue into either
//! a specific other agent's inbox (unicast) or every other agent's
//! inbox (broadcast). Frame form is fixed at construction — mixing
//! inner (24 B) and envelope (32 B) frames on one mesh is rejected.
//!
//! The mesh is the primitive used by P4.4 swarm tests. Unlike the
//! single-endpoint [`crate::loopback::LoopbackTransport`] (which pairs
//! one process's send and recv queues), the mesh reflects a fabric:
//! what agent 0 sends goes to agents 1..N, not back to itself.
//!
//! ## Why not a `SepTransport` per endpoint
//!
//! The transport trait borrows `&mut self`, which would require each
//! endpoint to carry a mutable handle back to shared fabric state.
//! `#![deny(unsafe_code)]` plus `no_std` rules out `Rc<RefCell<_>>`,
//! and interior mutability via `&mut` through a trait object pollutes
//! the API. The mesh exposes plain methods instead — tests drive the
//! fabric directly with `send(from, to, frame)` and `recv(agent,
//! buf)`, which is both clearer and zero-cost.

use heapless::Deque;
use umwelt_sep::{FrameForm, SepTransportError, SEP_ENVELOPE_BYTES};

/// One slot in an agent's inbox. Fixed 32-byte backing storage (the
/// envelope size) plus the real length — inner frames are padded on
/// enqueue and the true length is returned on recv.
type Slot = (u8, [u8; SEP_ENVELOPE_BYTES]);

/// Bounded in-memory mesh of `A` agents, each with a `Q`-slot inbox.
///
/// Agent IDs are slot indices in `0..A`. Unicast routes into a single
/// inbox; broadcast routes into every inbox except the sender's.
pub struct LoopbackMesh<const A: usize, const Q: usize> {
    form: FrameForm,
    /// Per-agent inboxes, indexed by agent id.
    inboxes: [Deque<Slot, Q>; A],
    /// Counts of dropped frames per agent (inbox-full). Useful for
    /// proving partition-free behaviour in swarm tests; exposed via
    /// [`LoopbackMesh::dropped`].
    drops: [u32; A],
}

impl<const A: usize, const Q: usize> LoopbackMesh<A, Q> {
    /// Construct a mesh carrying inner (24 B) SEP frames only.
    pub fn new_inner() -> Self {
        Self {
            form: FrameForm::Inner,
            inboxes: core::array::from_fn(|_| Deque::new()),
            drops: [0u32; A],
        }
    }

    /// Construct a mesh carrying 32 B SEP envelopes only.
    pub fn new_envelope() -> Self {
        Self {
            form: FrameForm::Envelope,
            inboxes: core::array::from_fn(|_| Deque::new()),
            drops: [0u32; A],
        }
    }

    /// Fixed frame shape for this fabric.
    #[inline]
    pub fn form(&self) -> FrameForm {
        self.form
    }

    /// Number of agents on this fabric.
    #[inline]
    pub const fn agents(&self) -> usize {
        A
    }

    /// Queued-frame count in `agent`'s inbox. Panics in debug builds if
    /// `agent >= A`.
    #[inline]
    pub fn queued(&self, agent: usize) -> usize {
        self.inboxes[agent].len()
    }

    /// Count of frames that were dropped trying to reach `agent`
    /// because its inbox was full. Non-decreasing across the mesh's
    /// lifetime.
    #[inline]
    pub fn dropped(&self, agent: usize) -> u32 {
        self.drops[agent]
    }

    /// Unicast: `from` sends a single frame to `to`.
    ///
    /// Returns [`SepTransportError::InvalidFrameLen`] if `frame.len()`
    /// doesn't match the configured form. Returns
    /// [`SepTransportError::Io`] if `from == to` (self-send is never
    /// meaningful on a mesh; the [`LoopbackTransport`] covers the
    /// A-sends-to-A case).
    ///
    /// Inbox-full is **silently dropped** and counted via
    /// [`Self::dropped`] — we model a lossy fabric, mirroring P4.3's
    /// simulated-LoRa semantics.
    ///
    /// [`LoopbackTransport`]: crate::loopback::LoopbackTransport
    pub fn send(&mut self, from: usize, to: usize, frame: &[u8]) -> Result<(), SepTransportError> {
        self.validate_frame(frame)?;
        if from == to || from >= A || to >= A {
            return Err(SepTransportError::Io);
        }
        self.enqueue(to, frame);
        Ok(())
    }

    /// Broadcast: `from` sends the same frame to every other agent.
    /// Self-inbox is skipped. Drops are counted per receiver.
    pub fn broadcast(&mut self, from: usize, frame: &[u8]) -> Result<(), SepTransportError> {
        self.validate_frame(frame)?;
        if from >= A {
            return Err(SepTransportError::Io);
        }
        for to in 0..A {
            if to == from {
                continue;
            }
            self.enqueue(to, frame);
        }
        Ok(())
    }

    /// Drain one frame from `agent`'s inbox into `out`.
    ///
    /// Returns [`SepTransportError::BufferTooSmall`] if `out` is shorter
    /// than the configured form; the frame stays queued so callers can
    /// retry with a larger buffer.
    ///
    /// Returns [`SepTransportError::WouldBlock`] if the inbox is empty.
    pub fn recv(&mut self, agent: usize, out: &mut [u8]) -> Result<usize, SepTransportError> {
        if agent >= A {
            return Err(SepTransportError::Io);
        }
        if out.len() < self.form.bytes() {
            return Err(SepTransportError::BufferTooSmall);
        }
        let (len, buf) = self.inboxes[agent]
            .pop_front()
            .ok_or(SepTransportError::WouldBlock)?;
        let n = len as usize;
        out[..n].copy_from_slice(&buf[..n]);
        Ok(n)
    }

    // ── internals ──────────────────────────────────────────────────────────

    fn validate_frame(&self, frame: &[u8]) -> Result<(), SepTransportError> {
        if frame.len() != self.form.bytes() {
            Err(SepTransportError::InvalidFrameLen)
        } else {
            Ok(())
        }
    }

    fn enqueue(&mut self, to: usize, frame: &[u8]) {
        let mut buf = [0u8; SEP_ENVELOPE_BYTES];
        buf[..frame.len()].copy_from_slice(frame);
        if self.inboxes[to]
            .push_back((frame.len() as u8, buf))
            .is_err()
        {
            self.drops[to] = self.drops[to].saturating_add(1);
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

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

    #[test]
    fn new_mesh_is_empty() {
        let m: LoopbackMesh<4, 8> = LoopbackMesh::new_inner();
        for a in 0..m.agents() {
            assert_eq!(m.queued(a), 0);
            assert_eq!(m.dropped(a), 0);
        }
    }

    #[test]
    fn unicast_puts_frame_in_receiver_inbox_only() {
        let mut m: LoopbackMesh<4, 8> = LoopbackMesh::new_inner();
        let f = sample_inner();
        m.send(0, 2, &f).unwrap();
        assert_eq!(m.queued(2), 1);
        assert_eq!(m.queued(0), 0);
        assert_eq!(m.queued(1), 0);
        assert_eq!(m.queued(3), 0);
    }

    #[test]
    fn unicast_self_send_rejected() {
        let mut m: LoopbackMesh<4, 8> = LoopbackMesh::new_inner();
        let f = sample_inner();
        let err = m.send(1, 1, &f);
        assert_eq!(err, Err(SepTransportError::Io));
    }

    #[test]
    fn unicast_invalid_length_rejected() {
        let mut m: LoopbackMesh<4, 8> = LoopbackMesh::new_inner();
        let short = [0u8; 10];
        let err = m.send(0, 1, &short);
        assert_eq!(err, Err(SepTransportError::InvalidFrameLen));
    }

    #[test]
    fn broadcast_fills_every_other_inbox() {
        let mut m: LoopbackMesh<4, 8> = LoopbackMesh::new_inner();
        let f = sample_inner();
        m.broadcast(0, &f).unwrap();
        assert_eq!(m.queued(0), 0);
        for a in 1..4 {
            assert_eq!(m.queued(a), 1);
        }
    }

    #[test]
    fn recv_drains_fifo() {
        let mut m: LoopbackMesh<4, 8> = LoopbackMesh::new_inner();
        let mut f1 = sample_inner();
        f1[0] = 0xAA;
        let mut f2 = sample_inner();
        f2[0] = 0xBB;
        m.send(0, 1, &f1).unwrap();
        m.send(0, 1, &f2).unwrap();
        let mut out = [0u8; SEP_FRAME_BYTES];
        let n1 = m.recv(1, &mut out).unwrap();
        assert_eq!(n1, SEP_FRAME_BYTES);
        assert_eq!(out[0], 0xAA);
        let n2 = m.recv(1, &mut out).unwrap();
        assert_eq!(n2, SEP_FRAME_BYTES);
        assert_eq!(out[0], 0xBB);
    }

    #[test]
    fn recv_empty_returns_would_block() {
        let mut m: LoopbackMesh<2, 4> = LoopbackMesh::new_inner();
        let mut out = [0u8; SEP_FRAME_BYTES];
        let err = m.recv(0, &mut out);
        assert_eq!(err, Err(SepTransportError::WouldBlock));
    }

    #[test]
    fn recv_buffer_too_small_preserves_frame() {
        let mut m: LoopbackMesh<2, 4> = LoopbackMesh::new_inner();
        let f = sample_inner();
        m.send(1, 0, &f).unwrap();
        let mut small = [0u8; 10];
        let err = m.recv(0, &mut small);
        assert_eq!(err, Err(SepTransportError::BufferTooSmall));
        // Frame is still queued.
        assert_eq!(m.queued(0), 1);
    }

    #[test]
    fn inbox_full_counts_drops() {
        let mut m: LoopbackMesh<2, 2> = LoopbackMesh::new_inner();
        let f = sample_inner();
        // Capacity 2 — two sends succeed, third silently drops.
        m.send(0, 1, &f).unwrap();
        m.send(0, 1, &f).unwrap();
        assert_eq!(m.dropped(1), 0);
        m.send(0, 1, &f).unwrap();
        assert_eq!(m.dropped(1), 1);
        m.send(0, 1, &f).unwrap();
        assert_eq!(m.dropped(1), 2);
    }

    #[test]
    fn six_agent_broadcast_mesh_no_cross_talk_self() {
        // Simulates P4.4's planned swarm shape.
        let mut m: LoopbackMesh<6, 16> = LoopbackMesh::new_inner();
        let f = sample_inner();
        for src in 0..6 {
            m.broadcast(src, &f).unwrap();
        }
        // Each agent should have 5 frames queued (one from every peer).
        for a in 0..6 {
            assert_eq!(m.queued(a), 5);
        }
    }

    #[test]
    fn envelope_form_fabric_rejects_inner_length() {
        let mut m: LoopbackMesh<2, 4> = LoopbackMesh::new_envelope();
        let inner = sample_inner();
        let err = m.send(0, 1, &inner);
        assert_eq!(err, Err(SepTransportError::InvalidFrameLen));
    }
}

//! Blocking UDP SEP transport (P4.2).
//!
//! Wraps [`std::net::UdpSocket`] and implements [`SepTransport`] with
//! the socket configured non-blocking, so `recv` returns
//! [`SepTransportError::WouldBlock`] when the receive buffer is empty
//! (matching the loopback transport's semantics).
//!
//! UDP was chosen over TCP because SEP frames are atomic, loss-
//! tolerant, and order-independent — a single frame is a single
//! datagram. The per-frame overhead is a single syscall; latency on a
//! loopback interface is well below the 10 Hz sensor tick.
//!
//! ## Host-only, std-only
//!
//! The embedded transport for real mesh deployments is a separate
//! crate; this module is only compiled in under the `udp` feature and
//! `std`. The companion simulated-LoRa wrapper (P4.3) is layered on
//! top of `UdpTransport` — loss, jitter, and partition are all
//! injected in the `send` / `recv` path, not at the socket layer.

use std::net::{SocketAddr, UdpSocket};

use umwelt_sep::{FrameForm, SepTransport, SepTransportError};

/// Blocking-but-non-blocking-socket UDP transport.
///
/// One instance carries a single conversation: the bound local address
/// plus a fixed peer address. Multi-peer fabrics should layer
/// [`crate::mesh::LoopbackMesh`]-style routing on top of this.
pub struct UdpTransport {
    socket: UdpSocket,
    peer: SocketAddr,
    form: FrameForm,
}

impl UdpTransport {
    /// Bind `local` and set `peer` as the fixed send destination.
    ///
    /// The socket is put into non-blocking mode so [`Self::recv`]
    /// immediately returns [`SepTransportError::WouldBlock`] when no
    /// datagram is pending. Callers drive the pump from their own
    /// scheduler.
    pub fn bind(local: SocketAddr, peer: SocketAddr, form: FrameForm) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(local)?;
        socket.set_nonblocking(true)?;
        Ok(Self { socket, peer, form })
    }

    /// The local address the socket is bound to. Useful when `bind`
    /// was called with `:0` and the OS picked the port.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// The configured peer address.
    #[inline]
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }
}

impl SepTransport for UdpTransport {
    fn form(&self) -> FrameForm {
        self.form
    }

    fn send(&mut self, frame: &[u8]) -> Result<(), SepTransportError> {
        if frame.len() != self.form.bytes() {
            return Err(SepTransportError::InvalidFrameLen);
        }
        self.socket
            .send_to(frame, self.peer)
            .map_err(|_| SepTransportError::Io)?;
        Ok(())
    }

    fn recv(&mut self, out: &mut [u8]) -> Result<usize, SepTransportError> {
        if out.len() < self.form.bytes() {
            return Err(SepTransportError::BufferTooSmall);
        }
        match self.socket.recv_from(out) {
            Ok((n, _)) => {
                if n != self.form.bytes() {
                    return Err(SepTransportError::InvalidFrameLen);
                }
                Ok(n)
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                Err(SepTransportError::WouldBlock)
            }
            Err(_) => Err(SepTransportError::Io),
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};
    use std::thread;
    use std::time::Duration;
    use umwelt_sep::SEP_FRAME_BYTES;

    fn free_loopback() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0)
    }

    fn sample_inner() -> [u8; SEP_FRAME_BYTES] {
        let mut f = [0u8; SEP_FRAME_BYTES];
        for (i, b) in f.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(17);
        }
        f
    }

    /// Spin-wait up to `deadline` for `recv` to return non-wouldblock.
    /// Non-blocking sockets are unavoidable in SepTransport's shape, so
    /// tests poll rather than introduce a thread-runtime dependency.
    fn poll_recv(
        t: &mut UdpTransport,
        out: &mut [u8],
        deadline: Duration,
    ) -> Result<usize, SepTransportError> {
        let start = std::time::Instant::now();
        loop {
            match t.recv(out) {
                Err(SepTransportError::WouldBlock) => {
                    if start.elapsed() > deadline {
                        return Err(SepTransportError::WouldBlock);
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                other => return other,
            }
        }
    }

    #[test]
    fn round_trip_inner_frame() {
        // Bind socket A first so its final address is known, then
        // bind B pointing at A, and rebind a fresh A pointing at B.
        let a_addr = {
            let tmp = UdpSocket::bind(free_loopback()).unwrap();
            tmp.local_addr().unwrap()
        };
        let b_addr = {
            let tmp = UdpSocket::bind(free_loopback()).unwrap();
            tmp.local_addr().unwrap()
        };
        let mut a = UdpTransport::bind(a_addr, b_addr, FrameForm::Inner).unwrap();
        let mut b = UdpTransport::bind(b_addr, a_addr, FrameForm::Inner).unwrap();

        let frame = sample_inner();
        a.send(&frame).unwrap();

        let mut out = [0u8; SEP_FRAME_BYTES];
        let n = poll_recv(&mut b, &mut out, Duration::from_millis(500)).unwrap();
        assert_eq!(n, SEP_FRAME_BYTES);
        assert_eq!(out, frame);
    }

    #[test]
    fn recv_empty_returns_would_block() {
        let addr = {
            let tmp = UdpSocket::bind(free_loopback()).unwrap();
            tmp.local_addr().unwrap()
        };
        let mut t = UdpTransport::bind(addr, addr, FrameForm::Inner).unwrap();
        let mut out = [0u8; SEP_FRAME_BYTES];
        let err = t.recv(&mut out);
        assert_eq!(err, Err(SepTransportError::WouldBlock));
    }

    #[test]
    fn wrong_length_rejected_on_send() {
        let addr = {
            let tmp = UdpSocket::bind(free_loopback()).unwrap();
            tmp.local_addr().unwrap()
        };
        let mut t = UdpTransport::bind(addr, addr, FrameForm::Inner).unwrap();
        let short = [0u8; 10];
        let err = t.send(&short);
        assert_eq!(err, Err(SepTransportError::InvalidFrameLen));
    }

    #[test]
    fn recv_small_buffer_returns_buffer_too_small() {
        let addr = {
            let tmp = UdpSocket::bind(free_loopback()).unwrap();
            tmp.local_addr().unwrap()
        };
        let mut t = UdpTransport::bind(addr, addr, FrameForm::Inner).unwrap();
        let mut small = [0u8; 8];
        let err = t.recv(&mut small);
        assert_eq!(err, Err(SepTransportError::BufferTooSmall));
    }

    #[test]
    fn form_reports_configured_shape() {
        let addr = {
            let tmp = UdpSocket::bind(free_loopback()).unwrap();
            tmp.local_addr().unwrap()
        };
        let inner = UdpTransport::bind(addr, addr, FrameForm::Inner).unwrap();
        assert_eq!(inner.form(), FrameForm::Inner);
        let addr2 = {
            let tmp = UdpSocket::bind(free_loopback()).unwrap();
            tmp.local_addr().unwrap()
        };
        let envelope = UdpTransport::bind(addr2, addr2, FrameForm::Envelope).unwrap();
        assert_eq!(envelope.form(), FrameForm::Envelope);
    }
}

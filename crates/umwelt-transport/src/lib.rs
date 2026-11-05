//! SEP transport adapters. Host-only; embedded ports live in separate crates.

#![deny(unsafe_code)]
#![warn(missing_docs)]

/// Transport error surface.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// Underlying I/O failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Peer closed the channel unexpectedly.
    #[error("peer closed")]
    PeerClosed,
}

#[cfg(feature = "loopback")]
pub mod loopback;

#[cfg(feature = "loopback")]
pub mod mesh;

#[cfg(feature = "udp")]
pub mod udp;

#[cfg(feature = "sim-lora")]
pub mod sim_lora;

//! Network backends for virtio-net.
//!
//! - [`nat::Nat`]: userspace NAT, unprivileged, in-process.
//! - [`socket::FrameSocket`]: raw Ethernet frames over a unix datagram
//!   socket, connected to the privileged `bulwark-netd` helper which owns
//!   bridged / host-only interfaces. The VMM never needs root.
#![forbid(unsafe_code)]

pub mod nat;
pub mod packet;
pub mod socket;

use std::io;
use std::time::Duration;

use mio::Registry;

/// First mio token a backend may use (matches the virtio worker's base).
pub const TOKEN_BASE: usize = 64;
/// Delivered after a pause: re-check every source (edge-triggered polling).
pub const RESUME_TOKEN: usize = usize::MAX - 1;

/// Where a NIC's frames go.
pub trait NetBackend: Send + std::fmt::Debug {
    /// Guest -> outside.
    fn send_frame(&mut self, frame: &[u8]);
    /// Outside -> guest. None when nothing is queued.
    fn recv_frame(&mut self) -> Option<Vec<u8>>;
    /// Register host sources with tokens >= TOKEN_BASE.
    fn register(&mut self, reg: &Registry) -> io::Result<()>;
    /// A registered source is ready (or RESUME_TOKEN).
    fn ready(&mut self, token: usize);
    /// Next housekeeping deadline.
    fn poll_delay(&mut self) -> Option<Duration> {
        None
    }
    fn tick(&mut self) {}
}

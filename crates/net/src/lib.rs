//! Network backends for virtio-net.
//!
//! - [`nat::Nat`]: userspace NAT, unprivileged, in-process.
//! - [`socket::FrameSocket`]: raw Ethernet frames over a unix datagram
//!   socket, connected to the privileged `vroom-netd` helper which owns
//!   bridged / host-only interfaces. The VMM never needs root.
#![forbid(unsafe_code)]

pub mod mac;
pub mod nat;
pub mod netd;
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
    /// Several frames the guest sent at once. Backends that can, handle
    /// them as one batch (NAT: one pass of its TCP/IP stack and one write
    /// per connection, instead of one per frame).
    fn send_frames(&mut self, frames: &[Vec<u8>]) {
        for f in frames {
            self.send_frame(f);
        }
    }
    /// Outside -> guest. None when nothing is queued.
    fn recv_frame(&mut self) -> Option<Vec<u8>>;
    /// Register host sources with tokens >= TOKEN_BASE.
    fn register(&mut self, reg: &Registry) -> io::Result<()>;
    /// The worker is going away (device reset): undo `register` so the next
    /// activation can register again.
    fn deregister(&mut self, _reg: &Registry) {}
    /// A registered source is ready (or RESUME_TOKEN).
    fn ready(&mut self, token: usize);
    /// Next housekeeping deadline.
    fn poll_delay(&mut self) -> Option<Duration> {
        None
    }
    fn tick(&mut self) {}
    /// NAT: where new connections may go from now on. False for backends
    /// that aren't a NAT.
    fn set_nat_policy(&mut self, _p: &nat::NatPolicy) -> bool {
        false
    }
}

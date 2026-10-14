//! Display channel between the (sandboxed) VMM and the viewer process.
//!
//! - Pixels: one shared-memory framebuffer, created by the VMM before it
//!   sandboxes itself, passed once to the viewer with SCM_RIGHTS.
//!   Format: 32-bit little endian XRGB (bytes B,G,R,X).
//! - Everything else: small framed messages on a unix stream.
//!
//! The viewer treats the VMM as untrusted: sizes are validated against the
//! mapping, strings are capped and UTF-8 checked.

mod fb;
mod proto;

pub use fb::{SharedFb, MAX_HEIGHT, MAX_WIDTH};
/// fd passing helpers; errors mapped to this crate's error type.
pub fn send_fd(sock: &std::os::unix::net::UnixStream, fd: std::os::fd::RawFd) -> Result<()> {
    bulwark_ipc::send_fd(sock, fd).map_err(ipc_err)
}

pub fn recv_fd(sock: &std::os::unix::net::UnixStream) -> Result<std::os::fd::OwnedFd> {
    bulwark_ipc::recv_fd(sock).map_err(ipc_err)
}

fn ipc_err(e: bulwark_ipc::Error) -> Error {
    match e {
        bulwark_ipc::Error::Io(e) => Error::Io(e),
        bulwark_ipc::Error::Proto(m) => Error::Proto(m),
    }
}
pub use proto::{Command, Decoder, ToViewer, ToVmm, POINTER_MAX};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Proto(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

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
mod fdpass;
mod proto;

pub use fb::{SharedFb, MAX_HEIGHT, MAX_WIDTH};
pub use fdpass::{recv_fd, send_fd};
pub use proto::{Command, Decoder, ToViewer, ToVmm, POINTER_MAX};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Proto(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

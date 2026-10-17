//! Guest physical memory.
//!
//! Guest RAM is shared with a running, untrusted guest. We never hand out
//! `&[u8]` / `&mut [u8]` into it, because the guest can change bytes under
//! our feet (that would be a data race and UB). All access goes through
//! copying helpers that are bounds-checked against the region table.

mod address;
mod bytes;
mod guest;
mod mmap;

pub use address::GuestAddress;
pub use bytes::ByteValued;
pub use guest::{GuestMemory, GuestRegion};
pub use mmap::MmapRegion;

/// Errors from guest memory access.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("guest address {0:#x} (+{1:#x}) is not backed by RAM")]
    InvalidRange(u64, u64),
    #[error("address arithmetic overflow")]
    Overflow,
    #[error("regions overlap at {0:#x}")]
    Overlap(u64),
    #[error("region size or base not aligned to host page size {0:#x}")]
    Unaligned(usize),
    #[error("mmap failed: {0}")]
    Mmap(i32),
    #[error("i/o error: {0}")]
    Io(String),
    #[cfg(feature = "research")]
    #[error("copy-on-write snapshot: {0}")]
    Cow(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Host page size.
pub fn page_size() -> usize {
    // SAFETY: sysconf has no memory safety preconditions.
    let sz = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if sz <= 0 {
        4096
    } else {
        sz as usize
    }
}

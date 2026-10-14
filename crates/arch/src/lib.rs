//! Board definitions: where things live in guest physical space, how the
//! firmware tables describe them, and how a kernel is put in RAM.
#![forbid(unsafe_code)]

pub mod acpi;
pub mod aarch64;
mod image;

pub use image::{decompress_if_needed, MAX_KERNEL_SIZE};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("fdt: {0}")]
    Fdt(String),
    #[error("kernel image: {0}")]
    Kernel(String),
    #[error("initrd: {0}")]
    Initrd(String),
    #[error("layout: {0}")]
    Layout(String),
    #[error("memory: {0}")]
    Memory(#[from] bulwark_memory::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Fill `buf` from the host CSPRNG.
pub fn host_random(buf: &mut [u8]) {
    if getrandom::getrandom(buf).is_err() {
        // Never boot with a predictable seed silently.
        log::warn!("host RNG unavailable; guest gets no entropy seed");
        buf.fill(0);
    }
}

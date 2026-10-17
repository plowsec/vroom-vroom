//! Windows-specific policy stays outside the production VMM dependency tree.
pub mod arm64;
pub mod binary;
pub mod cache;
pub mod control;
pub mod process;
pub mod symbols;
pub mod unwind;
pub mod windows;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("guest read at {address:#x}: {message}")]
    Memory { address: u64, message: String },
}
pub type Result<T> = std::result::Result<T, Error>;

/// All live implementations must hold an acknowledged VM pause.
pub trait PhysicalMemory {
    fn read_physical(&self, address: u64, data: &mut [u8]) -> Result<()>;
}

impl PhysicalMemory for vroom_vmm::Vmm {
    fn read_physical(&self, address: u64, data: &mut [u8]) -> Result<()> {
        self.debug_read_physical(address, data)
            .map_err(|e| Error::Memory {
                address,
                message: e.to_string(),
            })
    }
}

//! Virtio devices and transports.
//!
//! Layering:
//! - [`queue`] / [`chain`]: ring parsing, hardened against hostile drivers.
//! - [`VirtioDevice`]: what a device implements (config space, activate).
//! - [`worker`]: per-device thread that processes queues, pausable for snapshots.
//! - [`mmio`]: virtio-mmio transport (Linux direct boot).
//! - [`devices`]: rng, block, net, vsock, console, gpu, input...
#![forbid(unsafe_code)]

pub mod chain;
mod device;
pub mod devices;
pub mod mmio;
pub mod queue;
pub mod worker;

pub use chain::{DescriptorChain, Reader, Writer};
pub use device::{ActivateContext, DeviceState, VirtioDevice, VirtioInterrupt};
pub use queue::Queue;

/// Feature bits common to all devices.
pub mod features {
    pub const RING_INDIRECT_DESC: u64 = 1 << 28;
    pub const RING_EVENT_IDX: u64 = 1 << 29;
    pub const VERSION_1: u64 = 1 << 32;
}

/// Device type ids.
pub mod types {
    pub const NET: u32 = 1;
    pub const BLOCK: u32 = 2;
    pub const CONSOLE: u32 = 3;
    pub const RNG: u32 = 4;
    pub const GPU: u32 = 16;
    pub const INPUT: u32 = 18;
    pub const VSOCK: u32 = 19;
    pub const SOUND: u32 = 25;
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("queue: {0}")]
    Queue(String),
    #[error("descriptor chain: {0}")]
    Chain(String),
    #[error("guest memory: {0}")]
    Memory(#[from] bulwark_memory::Error),
    #[error("device: {0}")]
    Device(String),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("snapshot: {0}")]
    Snapshot(#[from] bulwark_snapshot::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

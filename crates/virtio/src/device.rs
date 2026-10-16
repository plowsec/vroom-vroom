use std::sync::Arc;

use vroom_memory::GuestMemory;
use serde::{Deserialize, Serialize};

use crate::{Queue, Result};

/// Transport-provided way to interrupt the driver.
pub trait VirtioInterrupt: Send + Sync {
    fn signal_used_queue(&self, queue: u16);
    fn signal_config_changed(&self);
}

/// Everything a device needs once the driver says DRIVER_OK.
#[derive(Clone)]
pub struct ActivateContext {
    pub mem: GuestMemory,
    pub queues: Vec<Queue>,
    pub irq: Arc<dyn VirtioInterrupt>,
    /// Features the driver accepted.
    pub features: u64,
}

impl std::fmt::Debug for ActivateContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivateContext").field("queues", &self.queues).finish_non_exhaustive()
    }
}

/// Snapshot of an active device: its queues plus device-specific bytes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceState {
    pub queues: Vec<Queue>,
    #[serde(with = "vroom_snapshot::bytes")]
    pub data: Vec<u8>,
}

/// A virtio device, independent of transport (MMIO or PCI).
pub trait VirtioDevice: Send {
    fn device_type(&self) -> u32;
    /// Short stable name, used in snapshot ids ("blk", "net0"...).
    fn name(&self) -> String;
    fn queue_max_sizes(&self) -> Vec<u16>;
    /// Offered features. VERSION_1 is added by the transport.
    fn device_features(&self) -> u64;
    fn read_config(&self, offset: u64, data: &mut [u8]);
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

    /// Driver is ready: start processing. Queues are already validated.
    fn activate(&mut self, ctx: ActivateContext) -> Result<()>;
    /// Shared memory regions `(id, length)` (virtio 1.2 §2.10), e.g. the
    /// virtio-gpu host-visible window.
    fn shm_regions(&self) -> Vec<(u8, u64)> {
        Vec::new()
    }
    /// The transport placed region `id` at guest physical `gpa`.
    fn set_shm_base(&mut self, _id: u8, _gpa: u64) {}
    /// PCI subsystem id. Devices of a kind that come several times may
    /// number themselves (the Windows guest agent tells GPUs apart so).
    fn pci_subsystem(&self) -> u16 {
        0x1100
    }
    /// Driver kicked `queue`. Called on a vCPU thread: keep it cheap.
    fn queue_notify(&mut self, queue: u16);
    /// Device reset by the driver: stop everything, forget queues.
    fn reset(&mut self);

    fn pause(&mut self) {}
    fn resume(&mut self) {}
    /// Called while paused and active.
    fn save_active(&mut self) -> Result<DeviceState>;
    /// Re-activate from a snapshot. `ctx.queues` come from the snapshot.
    fn restore_active(&mut self, ctx: ActivateContext, data: &[u8]) -> Result<()>;
}

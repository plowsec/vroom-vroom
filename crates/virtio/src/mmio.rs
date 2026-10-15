//! virtio-mmio transport, version 2 (virtio 1.x, section 4.2).

use std::sync::{Arc, Mutex};

use vroom_devices::{get_le, put_le, BusDevice, IrqLine};
use vroom_memory::GuestMemory;
use vroom_snapshot::{decode, encode, Snapshottable};
use serde::{Deserialize, Serialize};

use crate::core::{CoreState, VirtioCore};
use crate::{VirtioDevice, VirtioInterrupt};

pub use crate::core::status;

const MAGIC: u32 = 0x7472_6976; // "virt"
const VENDOR: u32 = 0x4b57_4c42; // "BLWK"

mod reg {
    pub const MAGIC: u64 = 0x000;
    pub const VERSION: u64 = 0x004;
    pub const DEVICE_ID: u64 = 0x008;
    pub const VENDOR_ID: u64 = 0x00c;
    pub const DEVICE_FEATURES: u64 = 0x010;
    pub const DEVICE_FEATURES_SEL: u64 = 0x014;
    pub const DRIVER_FEATURES: u64 = 0x020;
    pub const DRIVER_FEATURES_SEL: u64 = 0x024;
    pub const QUEUE_SEL: u64 = 0x030;
    pub const QUEUE_NUM_MAX: u64 = 0x034;
    pub const QUEUE_NUM: u64 = 0x038;
    pub const QUEUE_READY: u64 = 0x044;
    pub const QUEUE_NOTIFY: u64 = 0x050;
    pub const INTERRUPT_STATUS: u64 = 0x060;
    pub const INTERRUPT_ACK: u64 = 0x064;
    pub const STATUS: u64 = 0x070;
    pub const QUEUE_DESC_LOW: u64 = 0x080;
    pub const QUEUE_DESC_HIGH: u64 = 0x084;
    pub const QUEUE_DRIVER_LOW: u64 = 0x090;
    pub const QUEUE_DRIVER_HIGH: u64 = 0x094;
    pub const QUEUE_DEVICE_LOW: u64 = 0x0a0;
    pub const QUEUE_DEVICE_HIGH: u64 = 0x0a4;
    pub const SHM_SEL: u64 = 0x0ac;
    pub const SHM_LEN_LOW: u64 = 0x0b0;
    pub const SHM_LEN_HIGH: u64 = 0x0b4;
    pub const SHM_BASE_LOW: u64 = 0x0b8;
    pub const SHM_BASE_HIGH: u64 = 0x0bc;
    pub const CONFIG_GENERATION: u64 = 0x0fc;
    pub const CONFIG: u64 = 0x100;
}

const INT_VRING: u32 = 1;
const INT_CONFIG: u32 = 2;

/// Interrupt status register + line, updated atomically together so a
/// raise can't be lost against a concurrent ACK.
pub struct MmioInterrupt {
    status: Mutex<u32>,
    line: Arc<dyn IrqLine>,
}

impl std::fmt::Debug for MmioInterrupt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MmioInterrupt").finish_non_exhaustive()
    }
}

impl MmioInterrupt {
    fn raise(&self, bit: u32) {
        let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        *s |= bit;
        self.line.set_level(true);
    }

    fn ack(&self, bits: u32) {
        let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        *s &= !bits;
        if *s == 0 {
            self.line.set_level(false);
        }
    }

    fn get(&self) -> u32 {
        *self.status.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set(&self, v: u32) {
        let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        *s = v;
        self.line.set_level(v != 0);
    }
}

impl VirtioInterrupt for MmioInterrupt {
    fn signal_used_queue(&self, _queue: u16) {
        self.raise(INT_VRING);
    }
    fn signal_config_changed(&self) {
        self.raise(INT_CONFIG);
    }
}

/// Selector registers (transport-specific state).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Sel {
    device_features: u32,
    driver_features: u32,
    queue: u32,
    shm: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct MmioSnapshot {
    sel: Sel,
    interrupt_status: u32,
    core: CoreState,
}

/// Shared memory regions a device exposes (virtio-gpu host-visible memory).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShmRegion {
    pub id: u8,
    pub base: u64,
    pub len: u64,
}

pub struct VirtioMmio {
    base: u64,
    core: VirtioCore,
    irq: Arc<MmioInterrupt>,
    sel: Sel,
    shm: Vec<ShmRegion>,
}

impl std::fmt::Debug for VirtioMmio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtioMmio").field("base", &self.base).field("dev", &self.core.name()).finish()
    }
}

impl VirtioMmio {
    pub fn new(base: u64, dev: Box<dyn VirtioDevice>, mem: GuestMemory, line: Arc<dyn IrqLine>) -> Self {
        VirtioMmio {
            base,
            core: VirtioCore::new(dev, mem),
            irq: Arc::new(MmioInterrupt { status: Mutex::new(0), line }),
            sel: Sel::default(),
            shm: Vec::new(),
        }
    }

    pub fn with_shm(mut self, shm: Vec<ShmRegion>) -> Self {
        for r in &shm {
            self.core.set_shm_base(r.id, r.base);
        }
        self.shm = shm;
        self
    }

    /// Regions the device wants (id, len); the board picks the bases.
    pub fn shm_wanted(&self) -> Vec<(u8, u64)> {
        self.core.shm_regions()
    }

    fn irq_dyn(&self) -> Arc<dyn VirtioInterrupt> {
        self.irq.clone()
    }

    fn shm_region(&self) -> Option<&ShmRegion> {
        self.shm.iter().find(|r| r.id as u32 == self.sel.shm)
    }

    fn queue_set(&mut self, f: impl FnOnce(&mut crate::Queue)) {
        if let Some(q) = self.core.queue_mut(self.sel.queue as usize) {
            f(q);
        }
    }
}

fn set_lo(v: &mut u64, lo: u32) {
    *v = (*v & !0xffff_ffff) | lo as u64;
}
fn set_hi(v: &mut u64, hi: u32) {
    *v = (*v & 0xffff_ffff) | ((hi as u64) << 32);
}

impl BusDevice for VirtioMmio {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        if offset >= reg::CONFIG {
            self.core.read_config(offset - reg::CONFIG, data);
            return;
        }
        if data.len() != 4 {
            put_le(data, 0);
            return;
        }
        let q = self.core.queue(self.sel.queue as usize);
        let v: u32 = match offset {
            reg::MAGIC => MAGIC,
            reg::VERSION => 2,
            reg::DEVICE_ID => self.core.device_type(),
            reg::VENDOR_ID => VENDOR,
            reg::DEVICE_FEATURES => self.core.device_features_word(self.sel.device_features),
            reg::QUEUE_NUM_MAX => q.map(|q| q.max_size as u32).unwrap_or(0),
            reg::QUEUE_READY => q.map(|q| q.ready as u32).unwrap_or(0),
            reg::INTERRUPT_STATUS => self.irq.get(),
            reg::STATUS => self.core.status,
            reg::CONFIG_GENERATION => self.core.config_generation,
            reg::SHM_LEN_LOW => self.shm_region().map(|s| s.len as u32).unwrap_or(u32::MAX),
            reg::SHM_LEN_HIGH => self.shm_region().map(|s| (s.len >> 32) as u32).unwrap_or(u32::MAX),
            reg::SHM_BASE_LOW => self.shm_region().map(|s| s.base as u32).unwrap_or(u32::MAX),
            reg::SHM_BASE_HIGH => self.shm_region().map(|s| (s.base >> 32) as u32).unwrap_or(u32::MAX),
            _ => 0,
        };
        put_le(data, v as u64);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        if offset >= reg::CONFIG {
            self.core.write_config(offset - reg::CONFIG, data);
            return;
        }
        if data.len() != 4 {
            return;
        }
        let v = get_le(data) as u32;
        match offset {
            reg::DEVICE_FEATURES_SEL => self.sel.device_features = v,
            reg::DRIVER_FEATURES_SEL => self.sel.driver_features = v,
            reg::DRIVER_FEATURES => self.core.write_driver_features(self.sel.driver_features, v),
            reg::QUEUE_SEL => self.sel.queue = v,
            reg::QUEUE_NUM => self.queue_set(|q| q.size = v.min(u16::MAX as u32) as u16),
            reg::QUEUE_READY => self.queue_set(|q| q.ready = v == 1),
            reg::QUEUE_DESC_LOW => self.queue_set(|q| set_lo(&mut q.desc_table, v)),
            reg::QUEUE_DESC_HIGH => self.queue_set(|q| set_hi(&mut q.desc_table, v)),
            reg::QUEUE_DRIVER_LOW => self.queue_set(|q| set_lo(&mut q.avail_ring, v)),
            reg::QUEUE_DRIVER_HIGH => self.queue_set(|q| set_hi(&mut q.avail_ring, v)),
            reg::QUEUE_DEVICE_LOW => self.queue_set(|q| set_lo(&mut q.used_ring, v)),
            reg::QUEUE_DEVICE_HIGH => self.queue_set(|q| set_hi(&mut q.used_ring, v)),
            reg::QUEUE_NOTIFY => self.core.notify(v.min(u16::MAX as u32) as u16),
            reg::INTERRUPT_ACK => self.irq.ack(v),
            reg::STATUS => {
                let irq = self.irq_dyn();
                if self.core.set_status(v, &irq) {
                    self.sel = Sel::default();
                    self.irq.set(0);
                }
            }
            reg::SHM_SEL => self.sel.shm = v,
            _ => {}
        }
    }
}

impl Snapshottable for VirtioMmio {
    fn snapshot_id(&self) -> String {
        format!("virtio-{}@{:x}", self.core.name(), self.base)
    }

    fn pause(&mut self) {
        self.core.pause();
    }

    fn resume(&mut self) {
        self.core.resume();
    }

    fn save(&mut self) -> vroom_snapshot::Result<Vec<u8>> {
        let core = self.core.save().map_err(|e| vroom_snapshot::Error::Encode(e.to_string()))?;
        encode(&MmioSnapshot { sel: self.sel.clone(), interrupt_status: self.irq.get(), core })
    }

    fn restore(&mut self, data: &[u8]) -> vroom_snapshot::Result<()> {
        let s: MmioSnapshot = decode(data)?;
        let irq = self.irq_dyn();
        self.core.restore(s.core, &irq).map_err(|e| vroom_snapshot::Error::Invalid(e.to_string()))?;
        self.sel = s.sel;
        self.irq.set(s.interrupt_status);
        Ok(())
    }
}

//! virtio-mmio transport, version 2 (virtio 1.x, section 4.2).

use std::sync::{Arc, Mutex};

use bulwark_devices::{get_le, put_le, BusDevice, IrqLine};
use bulwark_memory::GuestMemory;
use bulwark_snapshot::{decode, encode, Snapshottable};
use serde::{Deserialize, Serialize};

use crate::features::{RING_EVENT_IDX, RING_INDIRECT_DESC, VERSION_1};
use crate::{ActivateContext, DeviceState, Queue, VirtioDevice, VirtioInterrupt};

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

pub mod status {
    pub const ACKNOWLEDGE: u32 = 1;
    pub const DRIVER: u32 = 2;
    pub const DRIVER_OK: u32 = 4;
    pub const FEATURES_OK: u32 = 8;
    pub const NEEDS_RESET: u32 = 64;
    pub const FAILED: u32 = 128;
}

const INT_VRING: u32 = 1;
const INT_CONFIG: u32 = 2;

/// Interrupt status register + line, updated atomically together so a
/// raise can't be lost against a concurrent ACK.
impl std::fmt::Debug for MmioInterrupt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MmioInterrupt").finish_non_exhaustive()
    }
}

pub struct MmioInterrupt {
    status: Mutex<u32>,
    line: Arc<dyn IrqLine>,
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Regs {
    status: u32,
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    queue_sel: u32,
    shm_sel: u32,
    config_generation: u32,
    interrupt_status: u32,
    queues: Vec<Queue>,
}

#[derive(Debug, Serialize, Deserialize)]
struct MmioSnapshot {
    regs: Regs,
    active: Option<DeviceState>,
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
    dev: Box<dyn VirtioDevice>,
    mem: GuestMemory,
    irq: Arc<MmioInterrupt>,
    regs: Regs,
    active: bool,
    shm: Vec<ShmRegion>,
}

impl std::fmt::Debug for VirtioMmio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtioMmio").field("base", &self.base).field("dev", &self.dev.name()).finish()
    }
}

impl VirtioMmio {
    pub fn new(base: u64, dev: Box<dyn VirtioDevice>, mem: GuestMemory, line: Arc<dyn IrqLine>) -> Self {
        let queues = dev.queue_max_sizes().into_iter().map(Queue::new).collect();
        VirtioMmio {
            base,
            dev,
            mem,
            irq: Arc::new(MmioInterrupt { status: Mutex::new(0), line }),
            regs: Regs { queues, ..Default::default() },
            active: false,
            shm: Vec::new(),
        }
    }

    pub fn with_shm(mut self, shm: Vec<ShmRegion>) -> Self {
        self.shm = shm;
        self
    }

    fn device_features(&self) -> u64 {
        self.dev.device_features() | VERSION_1
    }

    fn queue(&mut self) -> Option<&mut Queue> {
        // Queue config is only writable between FEATURES_OK and DRIVER_OK.
        let st = self.regs.status;
        if st & status::FEATURES_OK == 0 || st & status::DRIVER_OK != 0 {
            return None;
        }
        let q = self.regs.queues.get_mut(self.regs.queue_sel as usize)?;
        (!q.ready).then_some(q)
    }

    fn reset(&mut self) {
        if self.active {
            self.dev.reset();
            self.active = false;
        }
        for q in &mut self.regs.queues {
            q.reset();
        }
        self.regs.status = 0;
        self.regs.driver_features = 0;
        self.regs.queue_sel = 0;
        self.irq.set(0);
    }

    fn ctx(&self, queues: Vec<Queue>) -> ActivateContext {
        ActivateContext {
            mem: self.mem.clone(),
            queues,
            irq: self.irq.clone(),
            features: self.regs.driver_features,
        }
    }

    fn set_status(&mut self, v: u32) {
        if v == 0 {
            self.reset();
            return;
        }
        let old = self.regs.status;
        // Bits can only be added, never removed (except via reset).
        if v & old != old {
            log::debug!("virtio-mmio {}: status {old:#x} -> {v:#x} rejected", self.dev.name());
            return;
        }
        let mut v = v;
        if v & status::FEATURES_OK != 0 && old & status::FEATURES_OK == 0 {
            let f = self.regs.driver_features;
            if f & !self.device_features() != 0 || f & VERSION_1 == 0 {
                // Refuse: driver will see FEATURES_OK not set and give up.
                v &= !status::FEATURES_OK;
            }
        }
        self.regs.status = v;
        if v & status::DRIVER_OK != 0 && old & status::DRIVER_OK == 0 {
            self.activate();
        }
    }

    fn activate(&mut self) {
        let f = self.regs.driver_features;
        let mut queues = Vec::new();
        for (i, q) in self.regs.queues.iter_mut().enumerate() {
            q.event_idx = f & RING_EVENT_IDX != 0;
            q.indirect = f & RING_INDIRECT_DESC != 0;
            if q.ready && !q.is_valid(&self.mem) {
                log::warn!("virtio-mmio {}: queue {i} invalid", self.dev.name());
                self.regs.status |= status::NEEDS_RESET;
                self.irq.raise(INT_CONFIG);
                return;
            }
            queues.push(q.clone());
        }
        match self.dev.activate(self.ctx(queues)) {
            Ok(()) => self.active = true,
            Err(e) => {
                log::warn!("virtio-mmio {}: activate: {e}", self.dev.name());
                self.regs.status |= status::NEEDS_RESET;
                self.irq.raise(INT_CONFIG);
            }
        }
    }

    fn shm_region(&self) -> Option<&ShmRegion> {
        self.shm.iter().find(|r| r.id as u32 == self.regs.shm_sel)
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
            self.dev.read_config(offset - reg::CONFIG, data);
            return;
        }
        if data.len() != 4 {
            put_le(data, 0);
            return;
        }
        let r = &self.regs;
        let q = r.queues.get(r.queue_sel as usize);
        let v: u32 = match offset {
            reg::MAGIC => MAGIC,
            reg::VERSION => 2,
            reg::DEVICE_ID => self.dev.device_type(),
            reg::VENDOR_ID => VENDOR,
            reg::DEVICE_FEATURES => match r.device_features_sel {
                0 => self.device_features() as u32,
                1 => (self.device_features() >> 32) as u32,
                _ => 0,
            },
            reg::QUEUE_NUM_MAX => q.map(|q| q.max_size as u32).unwrap_or(0),
            reg::QUEUE_READY => q.map(|q| q.ready as u32).unwrap_or(0),
            reg::INTERRUPT_STATUS => self.irq.get(),
            reg::STATUS => r.status,
            reg::CONFIG_GENERATION => r.config_generation,
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
            if self.regs.status & status::DRIVER != 0 {
                self.dev.write_config(offset - reg::CONFIG, data);
            }
            return;
        }
        if data.len() != 4 {
            return;
        }
        let v = get_le(data) as u32;
        match offset {
            reg::DEVICE_FEATURES_SEL => self.regs.device_features_sel = v,
            reg::DRIVER_FEATURES_SEL => self.regs.driver_features_sel = v,
            reg::DRIVER_FEATURES => {
                if self.regs.status & status::FEATURES_OK == 0 {
                    match self.regs.driver_features_sel {
                        0 => set_lo(&mut self.regs.driver_features, v),
                        1 => set_hi(&mut self.regs.driver_features, v),
                        _ => {}
                    }
                }
            }
            reg::QUEUE_SEL => self.regs.queue_sel = v,
            reg::QUEUE_NUM => {
                if let Some(q) = self.queue() {
                    q.size = v.min(u16::MAX as u32) as u16;
                }
            }
            reg::QUEUE_READY => {
                if let Some(q) = self.queue() {
                    q.ready = v == 1;
                }
            }
            reg::QUEUE_DESC_LOW => {
                if let Some(q) = self.queue() {
                    set_lo(&mut q.desc_table, v)
                }
            }
            reg::QUEUE_DESC_HIGH => {
                if let Some(q) = self.queue() {
                    set_hi(&mut q.desc_table, v)
                }
            }
            reg::QUEUE_DRIVER_LOW => {
                if let Some(q) = self.queue() {
                    set_lo(&mut q.avail_ring, v)
                }
            }
            reg::QUEUE_DRIVER_HIGH => {
                if let Some(q) = self.queue() {
                    set_hi(&mut q.avail_ring, v)
                }
            }
            reg::QUEUE_DEVICE_LOW => {
                if let Some(q) = self.queue() {
                    set_lo(&mut q.used_ring, v)
                }
            }
            reg::QUEUE_DEVICE_HIGH => {
                if let Some(q) = self.queue() {
                    set_hi(&mut q.used_ring, v)
                }
            }
            reg::QUEUE_NOTIFY => {
                if self.active && (v as usize) < self.regs.queues.len() {
                    self.dev.queue_notify(v as u16);
                }
            }
            reg::INTERRUPT_ACK => self.irq.ack(v),
            reg::STATUS => self.set_status(v),
            reg::SHM_SEL => self.regs.shm_sel = v,
            _ => {}
        }
    }
}

impl Snapshottable for VirtioMmio {
    fn snapshot_id(&self) -> String {
        format!("virtio-{}@{:x}", self.dev.name(), self.base)
    }

    fn pause(&mut self) {
        if self.active {
            self.dev.pause();
        }
    }

    fn resume(&mut self) {
        if self.active {
            self.dev.resume();
        }
    }

    fn save(&mut self) -> bulwark_snapshot::Result<Vec<u8>> {
        let mut regs = self.regs.clone();
        regs.interrupt_status = self.irq.get();
        let active = if self.active {
            Some(self.dev.save_active().map_err(|e| bulwark_snapshot::Error::Encode(e.to_string()))?)
        } else {
            None
        };
        encode(&MmioSnapshot { regs, active })
    }

    fn restore(&mut self, data: &[u8]) -> bulwark_snapshot::Result<()> {
        let s: MmioSnapshot = decode(data)?;
        let name = self.dev.name();
        let bad = |m: &str| bulwark_snapshot::Error::Invalid(format!("virtio {name}: {m}"));
        let maxes = self.dev.queue_max_sizes();
        if s.regs.queues.len() != maxes.len() {
            return Err(bad("queue count mismatch"));
        }
        for (q, &m) in s.regs.queues.iter().zip(&maxes) {
            if q.max_size != m || q.size > m {
                return Err(bad("queue size mismatch"));
            }
        }
        self.reset();
        self.regs = s.regs;
        if let Some(st) = s.active {
            if st.queues.len() != maxes.len() {
                return Err(bad("active queue count mismatch"));
            }
            for q in &st.queues {
                if q.ready && !q.is_valid(&self.mem) {
                    return Err(bad("active queue invalid"));
                }
            }
            let ctx = self.ctx(st.queues);
            self.dev
                .restore_active(ctx, &st.data)
                .map_err(|e| bulwark_snapshot::Error::Invalid(e.to_string()))?;
            self.active = true;
        }
        self.irq.set(self.regs.interrupt_status);
        Ok(())
    }
}

//! Transport-independent virtio logic: device status state machine,
//! feature negotiation, queue validation, activation, snapshots.
//! The MMIO and PCI transports only map registers onto this.

use std::sync::Arc;

use bulwark_memory::GuestMemory;
use serde::{Deserialize, Serialize};

use crate::features::{RING_EVENT_IDX, RING_INDIRECT_DESC, VERSION_1};
use crate::{ActivateContext, DeviceState, Queue, VirtioDevice, VirtioInterrupt};

pub mod status {
    pub const ACKNOWLEDGE: u32 = 1;
    pub const DRIVER: u32 = 2;
    pub const DRIVER_OK: u32 = 4;
    pub const FEATURES_OK: u32 = 8;
    pub const NEEDS_RESET: u32 = 64;
    pub const FAILED: u32 = 128;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreState {
    status: u32,
    driver_features: u64,
    queues: Vec<Queue>,
    config_generation: u32,
    active: Option<DeviceState>,
}

pub struct VirtioCore {
    dev: Box<dyn VirtioDevice>,
    mem: GuestMemory,
    pub status: u32,
    pub driver_features: u64,
    pub queues: Vec<Queue>,
    pub config_generation: u32,
    active: bool,
}

impl std::fmt::Debug for VirtioCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtioCore").field("dev", &self.dev.name()).field("status", &self.status).finish()
    }
}

impl VirtioCore {
    pub fn new(dev: Box<dyn VirtioDevice>, mem: GuestMemory) -> Self {
        let queues = dev.queue_max_sizes().into_iter().map(Queue::new).collect();
        VirtioCore { dev, mem, status: 0, driver_features: 0, queues, config_generation: 0, active: false }
    }

    pub fn name(&self) -> String {
        self.dev.name()
    }

    pub fn device_type(&self) -> u32 {
        self.dev.device_type()
    }

    pub fn device_features(&self) -> u64 {
        self.dev.device_features() | VERSION_1
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn read_config(&self, off: u64, data: &mut [u8]) {
        self.dev.read_config(off, data);
    }

    pub fn write_config(&mut self, off: u64, data: &[u8]) {
        if self.status & status::DRIVER != 0 {
            self.dev.write_config(off, data);
        }
    }

    /// Write half of the driver feature word (`sel` 0 = low, 1 = high).
    pub fn write_driver_features(&mut self, sel: u32, v: u32) {
        if self.status & status::FEATURES_OK != 0 {
            return;
        }
        let f = &mut self.driver_features;
        match sel {
            0 => *f = (*f & !0xffff_ffff) | v as u64,
            1 => *f = (*f & 0xffff_ffff) | ((v as u64) << 32),
            _ => {}
        }
    }

    pub fn device_features_word(&self, sel: u32) -> u32 {
        match sel {
            0 => self.device_features() as u32,
            1 => (self.device_features() >> 32) as u32,
            _ => 0,
        }
    }

    pub fn queue(&self, sel: usize) -> Option<&Queue> {
        self.queues.get(sel)
    }

    /// Queue config is only writable between FEATURES_OK and DRIVER_OK,
    /// and only while the queue isn't enabled yet.
    pub fn queue_mut(&mut self, sel: usize) -> Option<&mut Queue> {
        let st = self.status;
        if st & status::FEATURES_OK == 0 || st & status::DRIVER_OK != 0 {
            return None;
        }
        self.queues.get_mut(sel).filter(|q| !q.ready)
    }

    pub fn reset(&mut self) {
        if self.active {
            self.dev.reset();
            self.active = false;
        }
        for q in &mut self.queues {
            q.reset();
        }
        self.status = 0;
        self.driver_features = 0;
    }

    /// Returns true if the transport must reset its own registers too.
    pub fn set_status(&mut self, v: u32, irq: &Arc<dyn VirtioInterrupt>) -> bool {
        if v == 0 {
            self.reset();
            return true;
        }
        let old = self.status;
        // Bits can only be added, never removed (except via reset).
        if v & old != old {
            log::debug!("virtio {}: status {old:#x} -> {v:#x} rejected", self.dev.name());
            return false;
        }
        let mut v = v;
        if v & status::FEATURES_OK != 0 && old & status::FEATURES_OK == 0 {
            let f = self.driver_features;
            if f & !self.device_features() != 0 || f & VERSION_1 == 0 {
                // Refuse: driver sees FEATURES_OK clear and gives up.
                v &= !status::FEATURES_OK;
            }
        }
        self.status = v;
        if v & status::DRIVER_OK != 0 && old & status::DRIVER_OK == 0 {
            self.activate(irq);
        }
        false
    }

    fn fail(&mut self, irq: &Arc<dyn VirtioInterrupt>) {
        self.status |= status::NEEDS_RESET;
        irq.signal_config_changed();
    }

    fn activate(&mut self, irq: &Arc<dyn VirtioInterrupt>) {
        let f = self.driver_features;
        let mut queues = Vec::new();
        for (i, q) in self.queues.iter_mut().enumerate() {
            q.event_idx = f & RING_EVENT_IDX != 0;
            q.indirect = f & RING_INDIRECT_DESC != 0;
            if q.ready && !q.is_valid(&self.mem) {
                log::warn!("virtio {}: queue {i} invalid", self.dev.name());
                return self.fail(irq);
            }
            queues.push(q.clone());
        }
        let ctx = ActivateContext { mem: self.mem.clone(), queues, irq: irq.clone(), features: f };
        match self.dev.activate(ctx) {
            Ok(()) => self.active = true,
            Err(e) => {
                log::warn!("virtio {}: activate: {e}", self.dev.name());
                self.fail(irq)
            }
        }
    }

    pub fn notify(&mut self, q: u16) {
        if self.active && (q as usize) < self.queues.len() {
            self.dev.queue_notify(q);
        }
    }

    pub fn pause(&mut self) {
        if self.active {
            self.dev.pause();
        }
    }

    pub fn resume(&mut self) {
        if self.active {
            self.dev.resume();
        }
    }

    pub fn save(&mut self) -> crate::Result<CoreState> {
        let active = if self.active { Some(self.dev.save_active()?) } else { None };
        Ok(CoreState {
            status: self.status,
            driver_features: self.driver_features,
            queues: self.queues.clone(),
            config_generation: self.config_generation,
            active,
        })
    }

    /// Restore from an untrusted snapshot: shapes are checked first.
    pub fn restore(&mut self, s: CoreState, irq: &Arc<dyn VirtioInterrupt>) -> crate::Result<()> {
        let name = self.dev.name();
        let bad = |m: &str| crate::Error::Device(format!("virtio {name}: {m}"));
        let maxes = self.dev.queue_max_sizes();
        let shape_ok = |qs: &[Queue]| qs.len() == maxes.len() && qs.iter().zip(&maxes).all(|(q, &m)| q.max_size == m && q.size <= m);
        if !shape_ok(&s.queues) {
            return Err(bad("queue layout mismatch"));
        }
        self.reset();
        self.status = s.status;
        self.driver_features = s.driver_features;
        self.queues = s.queues;
        self.config_generation = s.config_generation;
        if let Some(st) = s.active {
            if !shape_ok(&st.queues) || st.queues.iter().any(|q| q.ready && !q.is_valid(&self.mem)) {
                return Err(bad("active queues invalid"));
            }
            let ctx = ActivateContext { mem: self.mem.clone(), queues: st.queues, irq: irq.clone(), features: self.driver_features };
            self.dev.restore_active(ctx, &st.data)?;
            self.active = true;
        }
        Ok(())
    }
}

//! virtio-pci transport, modern (virtio 1.x, section 4.1).
//!
//! BAR0 (64-bit, 32 KiB):
//! ```text
//! 0x0000 common config   0x1000 ISR   0x2000 device config
//! 0x3000 notify (4 bytes per queue)   0x4000 MSI-X table   0x6000 PBA
//! ```

use std::sync::{Arc, Mutex};

use bulwark_devices::pci::{BarSpec, ConfigSpace, Intx, MsiSender, Msix, PciDevice, PciId};
use bulwark_devices::{get_le, put_le};
use bulwark_memory::GuestMemory;
use bulwark_snapshot::{decode, encode};
use serde::{Deserialize, Serialize};

use crate::core::{CoreState, VirtioCore};
use crate::{types, VirtioDevice, VirtioInterrupt};

const BAR_SIZE: u64 = 0x8000;
const COMMON: u64 = 0x0000;
const ISR: u64 = 0x1000;
const DEVICE: u64 = 0x2000;
const NOTIFY: u64 = 0x3000;
const MSIX_TABLE: u64 = 0x4000;
const MSIX_PBA: u64 = 0x6000;
const NOTIFY_MULT: u32 = 4;
const NO_VECTOR: u16 = 0xffff;

const CAP_VENDOR: u8 = 0x09;
const CFG_COMMON: u8 = 1;
const CFG_NOTIFY: u8 = 2;
const CFG_ISR: u8 = 3;
const CFG_DEVICE: u8 = 4;
const CFG_PCI: u8 = 5;
const CFG_SHARED_MEMORY: u8 = 8;
/// Shared memory regions (GPU host-visible memory) live in BAR 2.
const SHM_BAR: usize = 2;

fn class_for(device_type: u32) -> u32 {
    match device_type {
        types::NET => 0x02_00_00,
        types::BLOCK => 0x01_00_00,
        types::CONSOLE => 0x07_80_00,
        types::GPU => 0x03_80_00,
        types::INPUT => 0x09_80_00,
        _ => 0x00_ff_00,
    }
}

/// Interrupt routing state shared with worker threads.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IrqState {
    msix: Msix,
    isr: u8,
    queue_vec: Vec<u16>,
    config_vec: u16,
}

struct PciInterrupt {
    st: Arc<Mutex<IrqState>>,
    intx: Arc<Intx>,
}

impl PciInterrupt {
    fn fire(&self, vector: u16, isr_bit: u8) {
        let mut s = self.st.lock().unwrap_or_else(|e| e.into_inner());
        if s.msix.enabled() {
            if vector != NO_VECTOR {
                s.msix.notify(vector);
            }
        } else {
            s.isr |= isr_bit;
            self.intx.set(true);
        }
    }
}

impl VirtioInterrupt for PciInterrupt {
    fn signal_used_queue(&self, q: u16) {
        let v = self.st.lock().unwrap_or_else(|e| e.into_inner()).queue_vec.get(q as usize).copied().unwrap_or(NO_VECTOR);
        self.fire(v, 1);
    }

    fn signal_config_changed(&self) {
        let v = self.st.lock().unwrap_or_else(|e| e.into_inner()).config_vec;
        self.fire(v, 2);
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Sel {
    device_features: u32,
    driver_features: u32,
    queue: u16,
}

#[derive(Serialize, Deserialize)]
struct Snap {
    cfg: ConfigSpace,
    sel: Sel,
    irq: IrqState,
    core: CoreState,
}

pub struct VirtioPci {
    id: String,
    core: VirtioCore,
    cfg: ConfigSpace,
    sel: Sel,
    irq_state: Arc<Mutex<IrqState>>,
    irq: Arc<PciInterrupt>,
    /// Offset of the VIRTIO_PCI_CAP_PCI_CFG capability.
    pci_cfg_cap: usize,
    /// Shared memory region in BAR 2 (id) and where the guest put it.
    shm: Option<(u8, u64)>,
}

impl std::fmt::Debug for VirtioPci {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtioPci").field("id", &self.id).finish()
    }
}

/// Write a 64-bit queue address as one 8-byte or two 4-byte accesses.
fn set_half(field: &mut u64, v: u64, low: bool, len: usize) {
    *field = match (len, low) {
        (8, _) => v,
        (_, true) => (*field & !0xffff_ffff) | (v & 0xffff_ffff),
        (_, false) => (*field & 0xffff_ffff) | ((v & 0xffff_ffff) << 32),
    };
}

fn vendor_cap(cfg_type: u8, offset: u32, length: u32, extra: &[u8]) -> Vec<u8> {
    let len = 16 + extra.len() as u8;
    let mut b = vec![len, cfg_type, 0, 0, 0, 0];
    b.extend_from_slice(&offset.to_le_bytes());
    b.extend_from_slice(&length.to_le_bytes());
    b.extend_from_slice(extra);
    b
}

impl VirtioPci {
    /// `slot` only feeds the snapshot id; INTx comes pre-routed.
    pub fn new(slot: u8, dev: Box<dyn VirtioDevice>, mem: GuestMemory, intx: Arc<Intx>, msi: Arc<dyn MsiSender>) -> Self {
        let ty = dev.device_type();
        let nq = dev.queue_max_sizes().len();
        let mut cfg = ConfigSpace::new(
            PciId {
                vendor: 0x1af4,
                device: 0x1040 + ty as u16,
                class: class_for(ty),
                revision: 1,
                subsys_vendor: 0x1af4,
                subsys: 0x1100,
            },
            1,
        );
        cfg.add_bar(0, BarSpec { size: BAR_SIZE, is64: true, prefetch: false });
        let ro = |n: usize| vec![0u8; n];
        let c = vendor_cap(CFG_COMMON, COMMON as u32, 0x38, &[]);
        cfg.add_capability(CAP_VENDOR, &c, &ro(c.len()));
        let c = vendor_cap(CFG_NOTIFY, NOTIFY as u32, (nq as u32) * NOTIFY_MULT, &NOTIFY_MULT.to_le_bytes());
        cfg.add_capability(CAP_VENDOR, &c, &ro(c.len()));
        let c = vendor_cap(CFG_ISR, ISR as u32, 4, &[]);
        cfg.add_capability(CAP_VENDOR, &c, &ro(c.len()));
        let c = vendor_cap(CFG_DEVICE, DEVICE as u32, 0x1000, &[]);
        cfg.add_capability(CAP_VENDOR, &c, &ro(c.len()));
        // pci_cfg window: bar/offset/length/data are guest-writable.
        let c = vendor_cap(CFG_PCI, 0, 0, &[0; 4]);
        let mut w = ro(c.len());
        w[2] = 0xff; // bar
        for b in &mut w[6..18] {
            *b = 0xff; // offset, length, data
        }
        let pci_cfg_cap = cfg.add_capability(CAP_VENDOR, &c, &w);
        // One shared memory region at most (virtio-gpu), at offset 0 of BAR 2.
        let shm_id = dev.shm_regions().first().map(|&(id, len)| {
            let size = len.next_power_of_two().max(1 << 20);
            cfg.add_bar(SHM_BAR, BarSpec { size, is64: true, prefetch: true });
            // virtio_pci_cap64: cap + offset_hi + length_hi.
            let mut c = vec![24, CFG_SHARED_MEMORY, SHM_BAR as u8, id, 0, 0];
            c.extend_from_slice(&0u32.to_le_bytes());
            c.extend_from_slice(&(len as u32).to_le_bytes());
            c.extend_from_slice(&0u32.to_le_bytes());
            c.extend_from_slice(&((len >> 32) as u32).to_le_bytes());
            cfg.add_capability(CAP_VENDOR, &c, &ro(c.len()));
            id
        });
        let vectors = (nq + 1) as u16;
        let msix = Msix::new(&mut cfg, vectors, 0, MSIX_TABLE as u32, MSIX_PBA as u32, msi);
        let irq_state = Arc::new(Mutex::new(IrqState { msix, isr: 0, queue_vec: vec![NO_VECTOR; nq], config_vec: NO_VECTOR }));
        let irq = Arc::new(PciInterrupt { st: irq_state.clone(), intx });
        let id = format!("virtio-{}@{slot:02x}", dev.name());
        VirtioPci {
            id,
            core: VirtioCore::new(dev, mem),
            cfg,
            sel: Sel::default(),
            irq_state,
            irq,
            pci_cfg_cap,
            shm: shm_id.map(|id| (id, 0)),
        }
    }

    fn irq_dyn(&self) -> Arc<dyn VirtioInterrupt> {
        self.irq.clone()
    }

    fn st(&self) -> std::sync::MutexGuard<'_, IrqState> {
        self.irq_state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn common_read(&mut self, off: u64, data: &mut [u8]) {
        let q = self.core.queue(self.sel.queue as usize);
        let v: u64 = match off {
            0x00 => self.sel.device_features as u64,
            0x04 => self.core.device_features_word(self.sel.device_features) as u64,
            0x08 => self.sel.driver_features as u64,
            0x0c => match self.sel.driver_features {
                0 => self.core.driver_features & 0xffff_ffff,
                1 => self.core.driver_features >> 32,
                _ => 0,
            },
            0x10 => self.st().config_vec as u64,
            0x12 => self.core.queues.len() as u64,
            0x14 => self.core.status as u64,
            0x15 => self.core.config_generation as u64,
            0x16 => self.sel.queue as u64,
            0x18 => q.map(|q| q.size as u64).unwrap_or(0),
            0x1a => self.st().queue_vec.get(self.sel.queue as usize).copied().unwrap_or(NO_VECTOR) as u64,
            0x1c => q.map(|q| q.ready as u64).unwrap_or(0),
            0x1e => self.sel.queue as u64,
            0x20 => q.map(|q| q.desc_table).unwrap_or(0),
            0x24 => q.map(|q| q.desc_table >> 32).unwrap_or(0),
            0x28 => q.map(|q| q.avail_ring).unwrap_or(0),
            0x2c => q.map(|q| q.avail_ring >> 32).unwrap_or(0),
            0x30 => q.map(|q| q.used_ring).unwrap_or(0),
            0x34 => q.map(|q| q.used_ring >> 32).unwrap_or(0),
            _ => 0,
        };
        put_le(data, v);
    }

    fn set_vector(&mut self, v: u16, which: Option<usize>) -> u16 {
        let mut st = self.st();
        let v = if v != NO_VECTOR && v as usize >= st.msix.vectors() { NO_VECTOR } else { v };
        match which {
            None => st.config_vec = v,
            Some(q) => {
                if let Some(slot) = st.queue_vec.get_mut(q) {
                    *slot = v;
                }
            }
        }
        v
    }

    fn common_write(&mut self, off: u64, data: &[u8]) {
        let v = get_le(data);
        let sel = self.sel.queue as usize;
        match off {
            0x00 => self.sel.device_features = v as u32,
            0x08 => self.sel.driver_features = v as u32,
            0x0c => self.core.write_driver_features(self.sel.driver_features, v as u32),
            0x10 => {
                self.set_vector(v as u16, None);
            }
            0x14 => {
                let irq = self.irq_dyn();
                if self.core.set_status(v as u8 as u32, &irq) {
                    self.sel = Sel::default();
                    let mut st = self.st();
                    st.isr = 0;
                    st.config_vec = NO_VECTOR;
                    st.queue_vec.iter_mut().for_each(|x| *x = NO_VECTOR);
                    self.irq.intx.set(false);
                }
            }
            0x16 => self.sel.queue = v as u16,
            0x18 => {
                if let Some(q) = self.core.queue_mut(sel) {
                    q.size = v as u16;
                }
            }
            0x1a => {
                if self.core.queue(sel).is_some() {
                    self.set_vector(v as u16, Some(sel));
                }
            }
            0x1c => {
                if let Some(q) = self.core.queue_mut(sel) {
                    q.ready = v as u16 == 1;
                }
            }
            0x20 | 0x24 | 0x28 | 0x2c | 0x30 | 0x34 => {
                if let Some(q) = self.core.queue_mut(sel) {
                    let field = match off & !7 {
                        0x20 => &mut q.desc_table,
                        0x28 => &mut q.avail_ring,
                        _ => &mut q.used_ring,
                    };
                    set_half(field, v, off % 8 == 0, data.len());
                }
            }
            _ => {}
        }
    }

    fn bar0_read(&mut self, off: u64, data: &mut [u8]) {
        match off {
            o if o < ISR => self.common_read(o - COMMON, data),
            o if o < DEVICE => {
                // Reading ISR acknowledges it.
                let mut st = self.st();
                let v = st.isr;
                st.isr = 0;
                drop(st);
                self.irq.intx.set(false);
                put_le(data, v as u64);
            }
            o if o < NOTIFY => self.core.read_config(o - DEVICE, data),
            o if o < MSIX_TABLE => put_le(data, 0),
            o if o < MSIX_PBA => self.st().msix.table_read(o - MSIX_TABLE, data),
            o => self.st().msix.pba_read(o - MSIX_PBA, data),
        }
    }

    fn bar0_write(&mut self, off: u64, data: &[u8]) {
        match off {
            o if o < ISR => self.common_write(o - COMMON, data),
            o if o < DEVICE => {}
            o if o < NOTIFY => self.core.write_config(o - DEVICE, data),
            o if o < MSIX_TABLE => {
                let q = (o - NOTIFY) / NOTIFY_MULT as u64;
                self.core.notify(q.min(u16::MAX as u64) as u16);
            }
            o if o < MSIX_PBA => self.st().msix.table_write(o - MSIX_TABLE, data),
            _ => {}
        }
    }

    /// The pci_cfg capability: (bar, offset, length) from config space.
    fn pci_cfg_target(&self) -> (u8, u64, usize) {
        let r = self.cfg.raw();
        let c = self.pci_cfg_cap;
        let bar = r[c + 4];
        let off = u32::from_le_bytes([r[c + 8], r[c + 9], r[c + 10], r[c + 11]]) as u64;
        let len = u32::from_le_bytes([r[c + 12], r[c + 13], r[c + 14], r[c + 15]]) as usize;
        (bar, off, len)
    }
}

impl PciDevice for VirtioPci {
    fn config(&self) -> &ConfigSpace {
        &self.cfg
    }

    fn config_mut(&mut self) -> &mut ConfigSpace {
        &mut self.cfg
    }

    fn config_read(&mut self, off: usize, data: &mut [u8]) {
        let window = self.pci_cfg_cap + 16;
        if off == window {
            let (bar, boff, len) = self.pci_cfg_target();
            if bar == 0 && matches!(len, 1 | 2 | 4) && boff + len as u64 <= BAR_SIZE {
                let mut tmp = [0u8; 4];
                self.bar0_read(boff, &mut tmp[..len]);
                let n = data.len().min(4);
                data[..n].copy_from_slice(&tmp[..n]);
                return;
            }
        }
        self.cfg.read(off, data);
    }

    fn config_written(&mut self, off: usize, len: usize) {
        let window = self.pci_cfg_cap + 16;
        if off == window {
            let (bar, boff, l) = self.pci_cfg_target();
            if bar == 0 && matches!(l, 1 | 2 | 4) && boff + l as u64 <= BAR_SIZE {
                let mut d = [0u8; 4];
                d.copy_from_slice(&self.cfg.raw()[window..window + 4]);
                self.bar0_write(boff, &d[..l.min(len.max(1))]);
            }
        }
        if let Some((id, base)) = self.shm {
            let now = self.cfg.bar_addr(SHM_BAR).unwrap_or(0);
            if now != base && now != 0 {
                self.shm = Some((id, now));
                self.core.set_shm_base(id, now);
            }
        }
        let cfg = self.cfg.clone();
        self.st().msix.update(&cfg);
        if self.cfg.intx_disabled() {
            self.irq.intx.set(false);
        }
    }

    fn bar_read(&mut self, bar: usize, off: u64, data: &mut [u8]) {
        if bar == 0 {
            self.bar0_read(off, data);
        } else {
            data.fill(0);
        }
    }

    fn bar_write(&mut self, bar: usize, off: u64, data: &[u8]) {
        if bar == 0 {
            self.bar0_write(off, data);
        }
    }

    fn id(&self) -> String {
        self.id.clone()
    }

    fn pause(&mut self) {
        self.core.pause();
    }

    fn resume(&mut self) {
        self.core.resume();
    }

    fn save(&mut self) -> bulwark_snapshot::Result<Vec<u8>> {
        let core = self.core.save().map_err(|e| bulwark_snapshot::Error::Encode(e.to_string()))?;
        let irq = self.st().clone();
        encode(&Snap { cfg: self.cfg.clone(), sel: self.sel.clone(), irq, core })
    }

    fn restore(&mut self, data: &[u8]) -> bulwark_snapshot::Result<()> {
        let s: Snap = decode(data)?;
        let mut cfg = s.cfg;
        if !cfg.rebuild_masks(&self.cfg) {
            return Err(bulwark_snapshot::Error::Invalid(format!("{}: config layout", self.id)));
        }
        {
            let mut st = self.st();
            if s.irq.queue_vec.len() != st.queue_vec.len() || s.irq.msix.vectors() != st.msix.vectors() {
                return Err(bulwark_snapshot::Error::Invalid(format!("{}: irq layout", self.id)));
            }
            // The sender is runtime-only (not serialized): keep ours.
            let sender = st.msix.sender();
            *st = s.irq;
            if let Some(sn) = sender {
                st.msix.set_sender(sn);
            }
        }
        self.cfg = cfg;
        if let (Some((id, _)), Some(addr)) = (self.shm, self.cfg.bar_addr(SHM_BAR)) {
            self.shm = Some((id, addr));
            self.core.set_shm_base(id, addr);
        }
        let irq = self.irq_dyn();
        self.core.restore(s.core, &irq).map_err(|e| bulwark_snapshot::Error::Invalid(e.to_string()))?;
        self.sel = s.sel;
        let c = self.cfg.clone();
        self.st().msix.update(&c);
        Ok(())
    }
}

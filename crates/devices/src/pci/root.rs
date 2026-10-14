//! Root complex: ECAM config access, BAR windows, INTx routing.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};

use bulwark_snapshot::{decode, encode, Snapshottable};

use super::{ConfigSpace, PciId, SharedPciDevice};
use crate::{BusDevice, IrqLine};

/// Four shared level-triggered lines; a line is high while any device
/// on it asserts.
pub struct IntxRouter {
    counts: Mutex<[u32; 4]>,
    lines: Vec<Arc<dyn IrqLine>>,
}

impl std::fmt::Debug for IntxRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IntxRouter")
    }
}

impl IntxRouter {
    pub fn new(lines: Vec<Arc<dyn IrqLine>>) -> Arc<Self> {
        assert_eq!(lines.len(), 4);
        Arc::new(IntxRouter { counts: Mutex::new([0; 4]), lines })
    }

    /// Standard swizzle: slot/pin -> line (pin 1..=4).
    pub fn pin(self: &Arc<Self>, slot: u8, pin: u8) -> Arc<Intx> {
        let line = (slot as usize + pin.max(1) as usize - 1) % 4;
        Arc::new(Intx { router: self.clone(), line, asserted: Mutex::new(false) })
    }
}

#[derive(Debug)]
pub struct Intx {
    router: Arc<IntxRouter>,
    line: usize,
    asserted: Mutex<bool>,
}

impl Intx {
    pub fn set(&self, level: bool) {
        let mut a = self.asserted.lock().unwrap_or_else(|e| e.into_inner());
        if *a == level {
            return;
        }
        *a = level;
        let mut c = self.router.counts.lock().unwrap_or_else(|e| e.into_inner());
        if level {
            c[self.line] += 1;
        } else {
            c[self.line] = c[self.line].saturating_sub(1);
        }
        self.router.lines[self.line].set_level(c[self.line] > 0);
    }
}

#[derive(Debug, Clone, Copy)]
struct Mapping {
    start: u64,
    len: u64,
    dev: usize,
    bar: usize,
}

pub struct PciRoot {
    host: Mutex<ConfigSpace>,
    devices: Vec<(u8, SharedPciDevice)>,
    map: RwLock<Vec<Mapping>>,
}

impl std::fmt::Debug for PciRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PciRoot").field("devices", &self.devices.len()).finish()
    }
}

impl PciRoot {
    /// `devices`: (slot 1..=31, device). Slot 0 is the host bridge.
    pub fn new(devices: Vec<(u8, SharedPciDevice)>) -> Arc<Self> {
        let host = ConfigSpace::new(
            PciId { vendor: 0x1b36, device: 0x0008, class: 0x06_00_00, revision: 0, subsys_vendor: 0x1b36, subsys: 0 },
            0,
        );
        Arc::new(PciRoot { host: Mutex::new(host), devices, map: RwLock::new(Vec::new()) })
    }

    fn device(&self, slot: u8) -> Option<(usize, &SharedPciDevice)> {
        self.devices.iter().enumerate().find(|(_, (s, _))| *s == slot).map(|(i, (_, d))| (i, d))
    }

    fn remap(&self) {
        let mut m = Vec::new();
        for (i, (_, d)) in self.devices.iter().enumerate() {
            let d = d.lock().unwrap_or_else(|e| e.into_inner());
            let c = d.config();
            if !c.mem_enabled() {
                continue;
            }
            for bar in 0..6 {
                if let (Some(spec), Some(addr)) = (c.bar_spec(bar), c.bar_addr(bar)) {
                    m.push(Mapping { start: addr, len: spec.size, dev: i, bar });
                }
            }
        }
        *self.map.write().unwrap_or_else(|e| e.into_inner()) = m;
    }

    fn ecam(&self, off: u64, data: &mut [u8], write: Option<&[u8]>) {
        let bus = off >> 20;
        let slot = ((off >> 15) & 31) as u8;
        let func = (off >> 12) & 7;
        let reg = (off & 0xfff) as usize;
        if bus != 0 || func != 0 {
            data.fill(0xff);
            return;
        }
        if slot == 0 {
            let mut h = self.host.lock().unwrap_or_else(|e| e.into_inner());
            match write {
                Some(w) => h.write(reg, w),
                None => h.read(reg, data),
            }
            return;
        }
        let Some((_, d)) = self.device(slot) else {
            data.fill(0xff);
            return;
        };
        let mut d = d.lock().unwrap_or_else(|e| e.into_inner());
        match write {
            Some(w) => {
                d.config_mut().write(reg, w);
                d.config_written(reg, w.len());
            }
            None => d.config_read(reg, data),
        }
        drop(d);
        if write.is_some() {
            self.remap();
        }
    }

    fn mmio(&self, addr: u64, data: &mut [u8], write: Option<&[u8]>) -> bool {
        let hit = self
            .map
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|m| addr >= m.start && addr - m.start < m.len)
            .copied();
        let Some(m) = hit else { return false };
        let Some((_, d)) = self.devices.get(m.dev) else { return false };
        let mut d = d.lock().unwrap_or_else(|e| e.into_inner());
        match write {
            Some(w) => d.bar_write(m.bar, addr - m.start, w),
            None => d.bar_read(m.bar, addr - m.start, data),
        }
        true
    }

    /// Bus port for the ECAM region (also the snapshot owner).
    pub fn ecam_port(self: &Arc<Self>) -> EcamPort {
        EcamPort(self.clone())
    }

    /// Bus port for a BAR window starting at `base`.
    pub fn window_port(self: &Arc<Self>, base: u64) -> WindowPort {
        WindowPort { root: self.clone(), base }
    }
}

#[derive(Debug)]
pub struct EcamPort(Arc<PciRoot>);

impl BusDevice for EcamPort {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        self.0.ecam(offset, data, None);
    }
    fn write(&mut self, offset: u64, data: &[u8]) {
        let mut scratch = [0u8; 8];
        self.0.ecam(offset, &mut scratch[..data.len().min(8)], Some(data));
    }
}

#[derive(Debug)]
pub struct WindowPort {
    root: Arc<PciRoot>,
    base: u64,
}

impl BusDevice for WindowPort {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        if !self.root.mmio(self.base + offset, data, None) {
            data.fill(0xff);
        }
    }
    fn write(&mut self, offset: u64, data: &[u8]) {
        let mut scratch = [0u8; 8];
        self.root.mmio(self.base + offset, &mut scratch[..data.len().min(8)], Some(data));
    }
}

impl Snapshottable for EcamPort {
    fn snapshot_id(&self) -> String {
        "pci-root".into()
    }

    fn pause(&mut self) {
        for (_, d) in &self.0.devices {
            d.lock().unwrap_or_else(|e| e.into_inner()).pause();
        }
    }

    fn resume(&mut self) {
        for (_, d) in &self.0.devices {
            d.lock().unwrap_or_else(|e| e.into_inner()).resume();
        }
    }

    fn save(&mut self) -> bulwark_snapshot::Result<Vec<u8>> {
        let host = self.0.host.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut devs = BTreeMap::new();
        for (_, d) in &self.0.devices {
            let mut d = d.lock().unwrap_or_else(|e| e.into_inner());
            devs.insert(d.id(), d.save()?);
        }
        encode(&(host, devs))
    }

    fn restore(&mut self, data: &[u8]) -> bulwark_snapshot::Result<()> {
        let (host, mut devs): (ConfigSpace, BTreeMap<String, Vec<u8>>) = decode(data)?;
        {
            let mut h = self.0.host.lock().unwrap_or_else(|e| e.into_inner());
            let mut host = host;
            if !host.rebuild_masks(&h) {
                return Err(bulwark_snapshot::Error::Invalid("host bridge layout".into()));
            }
            *h = host;
        }
        for (_, d) in &self.0.devices {
            let mut d = d.lock().unwrap_or_else(|e| e.into_inner());
            let id = d.id();
            let blob = devs.remove(&id).ok_or_else(|| bulwark_snapshot::Error::Invalid(format!("missing {id}")))?;
            d.restore(&blob)?;
        }
        self.0.remap();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pci::{BarSpec, PciDevice};

    struct Dummy {
        cfg: ConfigSpace,
        last: u64,
    }

    impl PciDevice for Dummy {
        fn config(&self) -> &ConfigSpace {
            &self.cfg
        }
        fn config_mut(&mut self) -> &mut ConfigSpace {
            &mut self.cfg
        }
        fn bar_read(&mut self, bar: usize, off: u64, data: &mut [u8]) {
            crate::put_le(data, (bar as u64) << 32 | off);
        }
        fn bar_write(&mut self, _bar: usize, off: u64, _data: &[u8]) {
            self.last = off;
        }
        fn id(&self) -> String {
            "dummy".into()
        }
        fn save(&mut self) -> bulwark_snapshot::Result<Vec<u8>> {
            encode(&self.cfg)
        }
        fn restore(&mut self, d: &[u8]) -> bulwark_snapshot::Result<()> {
            let mut c: ConfigSpace = decode(d)?;
            c.rebuild_masks(&self.cfg);
            self.cfg = c;
            Ok(())
        }
    }

    #[test]
    fn ecam_and_bars() {
        let mut cfg = ConfigSpace::new(
            PciId { vendor: 0x1234, device: 0x5678, class: 0x010802, revision: 0, subsys_vendor: 0, subsys: 0 },
            1,
        );
        cfg.add_bar(0, BarSpec { size: 0x4000, is64: true, prefetch: false });
        let dev: SharedPciDevice = Arc::new(Mutex::new(Dummy { cfg, last: 0 }));
        let root = PciRoot::new(vec![(1, dev)]);
        let mut ecam = root.ecam_port();
        let mut win = root.window_port(0x1000_0000);
        let slot1 = 1u64 << 15;
        let mut d = [0u8; 4];
        ecam.read(slot1, &mut d);
        assert_eq!(u32::from_le_bytes(d), 0x5678_1234);
        ecam.read(2 << 15, &mut d);
        assert_eq!(d, [0xff; 4]); // empty slot
        // Program BAR0 = 0x1000_8000, enable memory.
        ecam.write(slot1 + 0x10, &0x1000_8000u32.to_le_bytes());
        ecam.write(slot1 + 0x14, &0u32.to_le_bytes());
        let mut q = [0u8; 8];
        win.read(0x8010, &mut q);
        assert_eq!(q, [0xff; 8]); // decoding off
        ecam.write(slot1 + 4, &2u16.to_le_bytes());
        win.read(0x8010, &mut q);
        assert_eq!(u64::from_le_bytes(q), 0x10);
        // Snapshot roundtrip keeps the mapping.
        let s = ecam.save().unwrap();
        ecam.write(slot1 + 4, &0u16.to_le_bytes());
        ecam.restore(&s).unwrap();
        win.read(0x8020, &mut q);
        assert_eq!(u64::from_le_bytes(q), 0x20);
    }
}

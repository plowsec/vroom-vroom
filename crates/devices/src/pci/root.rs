//! Root complex: shared PCI config access, BAR windows, INTx routing, and the
//! hot plug root ports' buses.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use vroom_snapshot::{decode, encode, Snapshottable};

use super::{ConfigSpace, PciId, RootPort, SharedPciDevice, VendorIds};
use crate::{BusDevice, ConcurrentBusDevice, IrqLine};
use super::mch::{MchConfig, MchError, PciConfigError};

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

#[derive(Clone)]
struct Mapping {
    start: u64,
    len: u64,
    dev: SharedPciDevice,
    bar: usize,
}

pub struct PciRoot {
    host: Mutex<HostBridge>,
    devices: Vec<(u8, SharedPciDevice)>,
    /// Hot plug root ports (also in `devices`, in their slots).
    ports: Vec<(u8, Arc<Mutex<RootPort>>)>,
    map: RwLock<Vec<Mapping>>,
    /// The VM is paused: a device plugged now starts paused.
    paused: AtomicBool,
}

enum HostBridge {
    Legacy(ConfigSpace),
    Mch(MchConfig),
}

impl HostBridge {
    fn legacy(&self) -> vroom_snapshot::Result<&ConfigSpace> {
        match self {
            Self::Legacy(config) => Ok(config),
            Self::Mch(_) => Err(vroom_snapshot::Error::Invalid("MCH PCI is cold-boot only".into())),
        }
    }
}

impl std::fmt::Debug for PciRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PciRoot").field("devices", &self.devices.len()).field("ports", &self.ports.len()).finish()
    }
}

fn lock<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl PciRoot {
    /// `devices`: (slot 1..=31, device). Slot 0 is the host bridge.
    pub fn new(devices: Vec<(u8, SharedPciDevice)>) -> Arc<Self> {
        Self::with_host_ids(devices, VendorIds { vendor: 0x1b36, device: 0x0008, subsys_vendor: 0x1b36, subsys: 0 })
    }

    /// Same, with the host bridge's IDs (from the VM's `[identity]`).
    pub fn with_host_ids(devices: Vec<(u8, SharedPciDevice)>, ids: VendorIds) -> Arc<Self> {
        Self::with_ports(devices, ids, Vec::new())
    }

    /// Same, plus hot plug root ports in their slots.
    pub fn with_ports(mut devices: Vec<(u8, SharedPciDevice)>, ids: VendorIds, ports: Vec<(u8, Arc<Mutex<RootPort>>)>) -> Arc<Self> {
        let host = ConfigSpace::new(
            PciId {
                vendor: ids.vendor,
                device: ids.device,
                class: 0x06_00_00,
                revision: 0,
                subsys_vendor: ids.subsys_vendor,
                subsys: ids.subsys,
            },
            0,
        );
        for (slot, p) in &ports {
            devices.push((*slot, p.clone() as SharedPciDevice));
        }
        Arc::new(PciRoot { host: Mutex::new(HostBridge::Legacy(host)), devices, ports, map: RwLock::new(Vec::new()), paused: AtomicBool::new(false) })
    }

    // Only the checked MCH router receives this root: no generic ECAM port or
    // independently writable host-bridge handle escapes its exclusive owner.
    pub(super) fn with_mch(devices: Vec<(u8, SharedPciDevice)>, host: MchConfig) -> Arc<Self> {
        Arc::new(Self { host: Mutex::new(HostBridge::Mch(host)), devices, ports: Vec::new(),
            map: RwLock::new(Vec::new()), paused: AtomicBool::new(false) })
    }

    pub(super) fn mch_check(&self) -> Result<(), PciConfigError> {
        match &*lock(&self.host) {
            HostBridge::Mch(host) => host.check(),
            HostBridge::Legacy(_) => Ok(()),
        }
    }

    pub(super) fn mch_window(&self) -> Option<crate::BusRange> {
        match &*lock(&self.host) {
            HostBridge::Mch(host) => host.window(),
            HostBridge::Legacy(_) => None,
        }
    }

    fn device(&self, slot: u8) -> Option<&SharedPciDevice> {
        self.devices.iter().find(|(s, _)| *s == slot).map(|(_, d)| d)
    }

    /// Every function that may decode memory: the root bus's and those
    /// in the ports' slots.
    fn functions(&self) -> Vec<SharedPciDevice> {
        let mut v: Vec<SharedPciDevice> = self.devices.iter().map(|(_, d)| d.clone()).collect();
        v.extend(self.ports.iter().filter_map(|(_, p)| lock(p).target()));
        v
    }

    fn remap(&self) {
        let mut m = Vec::new();
        for d in self.functions() {
            let g = lock(&d);
            let c = g.config();
            if !c.mem_enabled() {
                continue;
            }
            for bar in 0..6 {
                if let (Some(spec), Some(addr)) = (c.bar_spec(bar), c.bar_addr(bar)) {
                    m.push(Mapping { start: addr, len: spec.size, dev: d.clone(), bar });
                }
            }
        }
        *self.map.write().unwrap_or_else(|e| e.into_inner()) = m;
    }

    /// The function a config cycle reaches: on the root bus, or the
    /// device behind the port whose secondary bus it is.
    fn target(&self, bus: u64, slot: u8, func: u64) -> Option<SharedPciDevice> {
        if func != 0 {
            return None;
        }
        if bus == 0 {
            return self.device(slot).cloned();
        }
        // A port's link has one device: number 0.
        if slot != 0 {
            return None;
        }
        self.ports.iter().find_map(|(_, p)| {
            let p = lock(p);
            (p.buses().0 as u64 == bus).then(|| p.target()).flatten()
        })
    }

    /// Shared ECAM-encoded config cycle. Validate the whole span before any
    /// callback, read-buffer mutation or BAR/hotplug side effect. A valid cycle
    /// to an absent function still succeeds. Chipset failures remain fallible.
    /// Keep ECAM's existing byte/unaligned/bulk access behavior within one
    /// 4 KiB function. The legacy adapter imposes its narrower scalar rules.
    pub(super) fn config_access(&self, off: u64, data: &mut [u8], write: Option<&[u8]>) -> Result<(), PciConfigError> {
        self.config_access_with_smram(off, data, write, None)
    }

    pub(super) fn config_access_with_smram(&self, off: u64, data: &mut [u8], write: Option<&[u8]>,
        apply: Option<super::smram::Apply<'_>>) -> Result<(), PciConfigError> {
        let reg = (off & 0xfff) as usize;
        let len = write.map_or(data.len(), |w| w.len());
        if off >= 1 << 28 || len == 0 || reg.checked_add(len).is_none_or(|end| end > super::CFG_SIZE) {
            return Err(PciConfigError::Span);
        }
        let bus = off >> 20;
        let slot = ((off >> 15) & 31) as u8;
        let func = (off >> 12) & 7;
        if bus == 0 && slot == 0 && func == 0 {
            let mut h = lock(&self.host);
            match &mut *h {
                HostBridge::Legacy(config) => match write {
                    Some(w) => config.write(reg, w),
                    None => config.read(reg, data),
                },
                HostBridge::Mch(host) => return host.access(reg, data, write, apply),
            }
            return Ok(());
        }
        let Some(d) = self.target(bus, slot, func) else {
            data.fill(0xff);
            return Ok(());
        };
        let mut d = lock(&d);
        match write {
            Some(w) => d.config_write(reg, w),
            None => d.config_read(reg, data),
        }
        drop(d);
        if write.is_some() {
            // A port may have let its device go.
            let gone = self.take_gone();
            self.remap();
            drop(gone);
        }
        Ok(())
    }

    /// Devices the ports let go of: the caller drops them once it holds
    /// no lock (their workers stop then).
    fn take_gone(&self) -> Vec<SharedPciDevice> {
        self.ports.iter().flat_map(|(_, p)| lock(p).take_gone()).collect()
    }

    fn mmio(&self, addr: u64, data: &mut [u8], write: Option<&[u8]>) -> bool {
        let hit = self
            .map
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|m| addr >= m.start && addr - m.start < m.len)
            .cloned();
        let Some(m) = hit else { return false };
        let mut d = lock(&m.dev);
        match write {
            Some(w) => d.bar_write(m.bar, addr - m.start, w),
            None => d.bar_read(m.bar, addr - m.start, data),
        }
        true
    }

    /// The opt-in cold MCH route. Do not reuse the legacy cached first-hit map:
    /// validate every currently enabled memory BAR, under retained endpoint
    /// guards, before dispatching one callback. Config writes are not rolled
    /// back: arbitrary config callbacks may already have had side effects.
    /// Callbacks must not reenter endpoint/root locks while these guards live.
    pub(super) fn checked_bar_mmio(&self, addr: u64, data: &mut [u8], write: Option<&[u8]>,
                                   windows: &[crate::BusRange], excluded: &[crate::BusRange]) -> Result<bool, MchError> {
        let len = write.map_or(data.len(), |bytes| bytes.len());
        let finish = addr.checked_add(len as u64).filter(|_| len != 0).ok_or(MchError::Boundary)?;
        let mut endpoints: Vec<_> = self.devices.iter().collect();
        endpoints.sort_by_key(|(slot, _)| *slot);
        let mut guards = Vec::with_capacity(endpoints.len());
        for (_, device) in &endpoints {
            guards.push(device.try_lock().map_err(|error| match error {
                std::sync::TryLockError::WouldBlock => MchError::BarBusy,
                std::sync::TryLockError::Poisoned(_) => MchError::BarPoisoned,
            })?);
        }
        // (base, exclusive end, endpoint guard index, BAR index)
        let mut map: Vec<(u64, u64, usize, usize)> = Vec::new();
        for (index, device) in guards.iter().enumerate() {
            let config = device.config();
            if !config.mem_enabled() { continue; }
            let mut high_half = false;
            for bar in 0..6 {
                let spec = config.bar_spec(bar);
                if high_half {
                    if spec.is_some() { return Err(MchError::BarMap); }
                    high_half = false;
                    continue;
                }
                let Some(spec) = spec else { continue };
                if !spec.size.is_power_of_two() || spec.size < 4096 || (spec.is64 && bar == 5) {
                    return Err(MchError::BarMap);
                }
                high_half = spec.is64;
                let Some(base) = config.bar_addr(bar) else { continue }; // Zero is unassigned.
                let limit = base.checked_add(spec.size).ok_or(MchError::BarMap)?;
                if base % spec.size != 0 || (!spec.is64 && limit > 1 << 32)
                    || !windows.iter().any(|r| r.base <= base && r.base.checked_add(r.len).is_some_and(|end| limit <= end))
                    || excluded.iter().any(|r| r.base < limit && r.base.checked_add(r.len).is_none_or(|end| base < end))
                    || map.iter().any(|(start, end, _, _)| base < *end && *start < limit) {
                    return Err(MchError::BarMap);
                }
                map.push((base, limit, index, bar));
            }
        }
        let hit = map.iter().find(|(base, end, _, _)| addr < *end && *base < finish);
        let Some(&(base, end, index, bar)) = hit else { return Ok(false) };
        if addr < base || finish > end { return Err(MchError::Boundary); }
        match write {
            Some(bytes) => guards[index].bar_write(bar, addr - base, bytes),
            None => guards[index].bar_read(bar, addr - base, data),
        }
        // All guards remain owned until after the callback, including guards
        // for other endpoints whose BARs were part of the overlap proof.
        Ok(true)
    }

    /// How many hot plug ports there are.
    pub fn ports(&self) -> usize {
        self.ports.len()
    }

    /// The root bus slot of port `i` (its INTx wiring).
    pub fn port_slot(&self, i: usize) -> Option<u8> {
        self.ports.get(i).map(|(s, _)| *s)
    }

    /// The first port with nothing in it (nor a device on its way out).
    pub fn free_port(&self) -> Option<usize> {
        self.ports.iter().position(|(_, p)| lock(p).is_empty())
    }

    /// Plug `dev` into port `i` while the guest runs. The guest is told;
    /// its hot plug driver brings the device up.
    pub fn plug(&self, i: usize, dev: SharedPciDevice) -> Result<(), String> {
        let (_, p) = self.ports.get(i).ok_or("no such port")?;
        let pristine = {
            let mut d = lock(&dev);
            if self.paused.load(Ordering::SeqCst) {
                d.pause();
            }
            d.save().map_err(|e| e.to_string())?
        };
        lock(p).plug(dev, pristine, false).map_err(str::to_string)
    }

    /// Ask the guest to give back the device in port `i`. False if
    /// there is none.
    pub fn unplug(&self, i: usize) -> bool {
        let Some((_, p)) = self.ports.get(i) else { return false };
        let r = lock(p).request_unplug();
        let gone = self.take_gone();
        self.remap();
        drop(gone);
        r
    }

    /// Port `i`'s device is still on its way out.
    pub fn removing(&self, i: usize) -> bool {
        self.ports.get(i).is_some_and(|(_, p)| lock(p).removing())
    }

    /// Take port `i`'s device out now, if the guest still hasn't let go.
    pub fn force_unplug(&self, i: usize) {
        let Some((_, p)) = self.ports.get(i) else { return };
        {
            let mut p = lock(p);
            if !p.removing() {
                return;
            }
            p.surprise_remove();
        }
        let gone = self.take_gone();
        self.remap();
        drop(gone);
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
        if self.0.config_access(offset, data, None).is_err() { data.fill(0xff); }
    }
    fn write(&mut self, offset: u64, data: &[u8]) {
        let _ = self.0.config_access(offset, &mut [], Some(data));
    }
}

#[derive(Debug)]
pub struct WindowPort {
    root: Arc<PciRoot>,
    base: u64,
}

impl ConcurrentBusDevice for WindowPort {
    fn read(&self, offset: u64, data: &mut [u8]) {
        if !self.root.mmio(self.base + offset, data, None) {
            data.fill(0xff);
        }
    }
    fn write(&self, offset: u64, data: &[u8]) {
        let mut scratch = [0u8; 8];
        self.root.mmio(self.base + offset, &mut scratch[..data.len().min(8)], Some(data));
    }
}

type Blobs = BTreeMap<String, vroom_snapshot::bytes::Bytes>;

impl EcamPort {
    /// Restore (or reset) every function from `data`. A hot plug port
    /// missing from it (a snapshot from before they existed) starts
    /// empty, as at power-on.
    fn load(&mut self, data: &[u8], reset: bool) -> vroom_snapshot::Result<()> {
        // Reject cold-only roots before even decoding bytes or touching a
        // device. The legacy serialized tuple deliberately remains unchanged.
        lock(&self.0.host).legacy()?;
        let (host, mut devs): (ConfigSpace, Blobs) = decode(data)?;
        {
            let mut h = lock(&self.0.host);
            let mut host = host;
            if !host.rebuild_masks(h.legacy()?) {
                return Err(vroom_snapshot::Error::Invalid("host bridge layout".into()));
            }
            *h = HostBridge::Legacy(host);
        }
        for (slot, d) in &self.0.devices {
            let mut d = lock(d);
            let id = d.id();
            let is_port = self.0.ports.iter().any(|(s, _)| s == slot);
            match (devs.remove(&id), reset) {
                (Some(blob), false) => d.restore(&blob.0)?,
                (Some(blob), true) => d.reset(&blob.0)?,
                (None, _) if is_port => {}
                // A VM saved before it had a balloon: its guest never saw one.
                (None, _) if id.starts_with("virtio-balloon@") => {}
                (None, _) => return Err(vroom_snapshot::Error::Invalid(format!("missing {id}"))),
            }
        }
        let gone = self.0.take_gone();
        self.0.remap();
        drop(gone);
        Ok(())
    }
}

impl Snapshottable for EcamPort {
    fn snapshot_id(&self) -> String {
        "pci-root".into()
    }

    fn pause(&mut self) {
        self.0.paused.store(true, Ordering::SeqCst);
        for (_, d) in &self.0.devices {
            lock(d).pause();
        }
    }

    fn resume(&mut self) {
        self.0.paused.store(false, Ordering::SeqCst);
        for (_, d) in &self.0.devices {
            lock(d).resume();
        }
    }

    fn save(&mut self) -> vroom_snapshot::Result<Vec<u8>> {
        let host = lock(&self.0.host).legacy()?.clone();
        let mut devs = BTreeMap::new();
        for (_, d) in &self.0.devices {
            let mut d = lock(d);
            devs.insert(d.id(), vroom_snapshot::bytes::Bytes(d.save()?));
        }
        encode(&(host, devs))
    }

    fn restore(&mut self, data: &[u8]) -> vroom_snapshot::Result<()> {
        self.load(data, false)
    }

    /// Ports keep the devices plugged into them since (see
    /// [`RootPort`]'s reset).
    fn reset(&mut self, pristine: &[u8]) -> vroom_snapshot::Result<()> {
        self.load(pristine, true)
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
        fn save(&mut self) -> vroom_snapshot::Result<Vec<u8>> {
            encode(&self.cfg)
        }
        fn restore(&mut self, d: &[u8]) -> vroom_snapshot::Result<()> {
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
        let win = root.window_port(0x1000_0000);
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

    #[test]
    fn host_bridge_ids() {
        let mut d = [0u8; 4];
        PciRoot::new(vec![]).ecam_port().read(0, &mut d);
        assert_eq!(u32::from_le_bytes(d), 0x0008_1b36);
        let ids = VendorIds { vendor: 0x8086, device: 0x9a14, subsys_vendor: 0x17aa, subsys: 0x22d8 };
        let mut ecam = PciRoot::with_host_ids(vec![], ids).ecam_port();
        ecam.read(0, &mut d);
        assert_eq!(u32::from_le_bytes(d), 0x9a14_8086);
        ecam.read(0x2c, &mut d);
        assert_eq!(u32::from_le_bytes(d), 0x22d8_17aa);
    }

    struct NoMsi;
    impl crate::pci::MsiSender for NoMsi {
        fn send(&self, _a: u64, _d: u32) {}
    }

    /// A device plugged into a port: reached on the port's secondary bus
    /// once the slot has power, its BAR mapped, gone after the guest
    /// lets go; a snapshot without the ports still restores.
    #[test]
    fn hot_plug_through_a_port() {
        let r = IntxRouter::new((0..4).map(|_| Arc::new(crate::NoIrq) as Arc<dyn IrqLine>).collect());
        let port = Arc::new(Mutex::new(RootPort::new(5, 0, VendorIds { vendor: 0x1b36, device: 0x000c, subsys_vendor: 0, subsys: 0 }, r.pin(5, 1), Arc::new(NoMsi))));
        let root = PciRoot::with_ports(vec![], VendorIds { vendor: 0x1b36, device: 8, subsys_vendor: 0, subsys: 0 }, vec![(5, port)]);
        let old = encode(&(lock(&root.host).legacy().unwrap().clone(), Blobs::new())).unwrap(); // from before ports
        let mut ecam = root.ecam_port();
        let win = root.window_port(0x1000_0000);
        let p = 5u64 << 15;
        let mut d = [0u8; 4];
        ecam.read(p, &mut d);
        assert_eq!(u32::from_le_bytes(d), 0x000c_1b36, "the port");
        // The guest gives it bus 1, turns on hot plug interrupts.
        ecam.write(p + 0x18, &[0, 1, 1]);
        let exp = 0x40;
        assert_eq!(root.free_port(), Some(0));
        let mut cfg = ConfigSpace::new(PciId { vendor: 0x1af4, device: 0x1041, class: 0x020000, revision: 1, subsys_vendor: 0, subsys: 0 }, 1);
        cfg.add_bar(0, BarSpec { size: 0x4000, is64: true, prefetch: false });
        root.plug(0, Arc::new(Mutex::new(Dummy { cfg, last: 0 }))).unwrap();
        assert_eq!(root.free_port(), None);
        let bus1 = 1u64 << 20;
        ecam.read(bus1, &mut d);
        assert_eq!(d, [0xff; 4], "slot not powered yet");
        ecam.write(p + exp + 0x18, &(1u16 << 5 | 1 << 8).to_le_bytes()); // HPIE, power on
        ecam.read(bus1, &mut d);
        assert_eq!(u32::from_le_bytes(d), 0x1041_1af4);
        ecam.read(bus1 | 1 << 15, &mut d);
        assert_eq!(d, [0xff; 4], "only device 0 behind a port");
        ecam.read(2 << 20, &mut d);
        assert_eq!(d, [0xff; 4]);
        ecam.write(bus1 + 0x10, &0x1000_8000u32.to_le_bytes());
        ecam.write(bus1 + 0x14, &0u32.to_le_bytes());
        ecam.write(bus1 + 4, &2u16.to_le_bytes());
        let mut q = [0u8; 8];
        win.read(0x8010, &mut q);
        assert_eq!(u64::from_le_bytes(q), 0x10);
        // A snapshot from before there were ports leaves them be; one
        // with the device restores it.
        let s = ecam.save().unwrap();
        assert!(ecam.restore(&old).is_ok());
        ecam.restore(&s).unwrap();
        // Out: the button, then the guest powers the slot off.
        assert!(root.unplug(0));
        assert!(root.removing(0));
        ecam.write(p + exp + 0x18, &(1u16 << 5 | 3 << 8 | 1 << 10).to_le_bytes());
        assert!(!root.removing(0));
        assert_eq!(root.free_port(), Some(0));
        win.read(0x8010, &mut q);
        assert_eq!(q, [0xff; 8], "unmapped");
        ecam.read(bus1, &mut d);
        assert_eq!(d, [0xff; 4]);
        assert!(!root.unplug(0));
    }
}

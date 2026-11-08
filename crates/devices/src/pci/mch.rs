//! Cold-only Q35 MCH configuration and checked ECAM routing.
//!
//! PCIEXBAR changes real absolute-GPA decoding inside one reserved aperture.
//! This is not a complete Q35 board: PAM, legacy SMRAM, reset and snapshots
//! remain unsupported. Fixed SMRAM controls are opt-in and require a successful
//! synchronous backend transition before register readback is committed.
//!
//! The exclusive router owns every route to its private MCH state. Each
//! SMRAM write prepares state, applies a fallible mapping transition on the
//! exiting vCPU, then commits readback; it must not use infallible BusDevice.
//!
//! Register definitions and the pinned firmware's disable/high/enable sequence:
//! https://github.com/qemu/qemu/blob/v11.1.2/include/hw/pci-host/q35.h
//! https://github.com/tianocore/edk2/blob/4dfdca63a93497203f197ec98ba20e2327e4afe4/OvmfPkg/Library/PlatformInitLib/Platform.c

use std::sync::Arc;
use vroom_memory::GuestMemory;
use crate::{Bus, BusRange};
use super::smram::{Apply, SmramConfig};
use super::lpc::Ich9Lpc;
use super::{LpcError, LpcPioLayout};
use super::{MchSmramState, ConfigSpace, PciCf8, PciCf8Error, PciId, PciRoot, SharedPciDevice};

const PCIEXBAR: usize = 0x60;
const APERTURE_SIZE: u64 = 256 << 20;
const ADDRESS_LIMIT: u64 = 1 << 36;
const ADDRESS_BITS: u64 = 0x0000_000f_fc00_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PciConfigError {
    #[error("configuration access crosses its function")]
    Span,
    #[error("unsupported configuration access width")]
    Width,
    #[error("unaligned configuration access")]
    Alignment,
    #[error("unimplemented MCH register or register boundary")]
    UnsupportedRegister,
    #[error("unsupported PCIEXBAR reserved bits")]
    ReservedBits,
    #[error("reserved PCIEXBAR length")]
    Length,
    #[error("PCIEXBAR base is not aligned to its window")]
    BaseAlignment,
    #[error("enabled PCIEXBAR window is outside its reserved aperture")]
    Aperture,
    #[error("unsupported fixed-Q35 SMRAM configuration")]
    SmramProfile,
    #[error("SMRAM write requires the exiting vCPU mapping callback")]
    SmramBackendRequired,
    #[error("SMRAM mapping callback failed or returned a different state")]
    SmramBackend,
    #[error("MCH router is poisoned after a failed SMRAM transition")]
    SmramPoisoned,
    #[error("LPC configuration access: {0}")]
    Lpc(#[from] LpcError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MchError {
    #[error("invalid or overlapping reserved ECAM aperture")]
    Layout,
    #[error("duplicate or invalid PCI endpoint slot")]
    Slot,
    #[error("invalid or overlapping BAR windows or reserved-range inventory")]
    BarLayout,
    #[error("the same PCI endpoint occurs in more than one slot")]
    EndpointAlias,
    #[error("a PCI endpoint is busy; no BAR callback was invoked")]
    BarBusy,
    #[error("a PCI endpoint mutex is poisoned")]
    BarPoisoned,
    #[error("the complete enabled memory-BAR map is invalid")]
    BarMap,
    #[error("unsupported MMIO access width")]
    Width,
    #[error("unaligned MMIO access")]
    Alignment,
    #[error("MMIO access overflows or crosses a routing boundary")]
    Boundary,
    #[error("PCI port access: {0}")]
    Port(#[from] PciCf8Error),
    #[error("PCI configuration access: {0}")]
    Config(#[from] PciConfigError),
    #[error("LPC port access: {0}")]
    Lpc(#[from] LpcError),
}

fn end(range: BusRange) -> Option<u64> {
    (range.len != 0).then(|| range.base.checked_add(range.len)).flatten()
}

fn contains(range: BusRange, start: u64, finish: u64) -> bool {
    end(range).is_some_and(|limit| start >= range.base && finish <= limit)
}

fn overlaps(range: BusRange, start: u64, finish: u64) -> bool {
    end(range).is_some_and(|limit| start < limit && range.base < finish)
}

/// Private, single source of truth for the host function and active ECAM map.
/// No mapping callback can get out of sync with the accepted register bytes.
pub(super) struct MchConfig {
    config: ConfigSpace,
    aperture: BusRange,
    smram: Option<SmramConfig>,
}

impl MchConfig {
    fn new(aperture: BusRange) -> Self {
        let mut config = ConfigSpace::new(PciId { vendor: 0x8086, device: 0x29c0,
            class: 0x06_00_00, revision: 0, subsys_vendor: 0, subsys: 0 }, 0);
        config.raw_mut()[PCIEXBAR..PCIEXBAR + 8].copy_from_slice(&0xb000_0000u64.to_le_bytes());
        Self { config, aperture, smram: None }
    }

    fn value(&self) -> u64 {
        u64::from_le_bytes(self.config.raw()[PCIEXBAR..PCIEXBAR + 8].try_into().unwrap())
    }

    fn decode(&self, value: u64) -> Result<Option<BusRange>, PciConfigError> {
        if value & !(ADDRESS_BITS | 7) != 0 { return Err(PciConfigError::ReservedBits); }
        let len = match (value >> 1) & 3 {
            0 => 256 << 20,
            1 => 128 << 20,
            2 => 64 << 20,
            _ => return Err(PciConfigError::Length),
        };
        let base = value & ADDRESS_BITS;
        if base % len != 0 { return Err(PciConfigError::BaseAlignment); }
        if value & 1 == 0 { return Ok(None); }
        let finish = base.checked_add(len).ok_or(PciConfigError::Aperture)?;
        if !contains(self.aperture, base, finish) { return Err(PciConfigError::Aperture); }
        Ok(Some(BusRange { base, len }))
    }

    pub(super) fn window(&self) -> Option<BusRange> {
        // Construction and every accepted write validate the complete value.
        self.decode(self.value()).expect("validated private PCIEXBAR")
    }

    pub(super) fn check(&self) -> Result<(), PciConfigError> {
        self.smram.as_ref().map_or(Ok(()), SmramConfig::check)
    }

    pub(super) fn access(&mut self, off: usize, data: &mut [u8], write: Option<&[u8]>, apply: Option<Apply<'_>>) -> Result<(), PciConfigError> {
        self.check()?;
        let len = write.map_or(data.len(), |bytes| bytes.len());
        if !matches!(len, 1 | 2 | 4 | 8) { return Err(PciConfigError::Width); }
        if off % len != 0 { return Err(PciConfigError::Alignment); }
        let finish = off.checked_add(len).ok_or(PciConfigError::Span)?;
        if finish <= 0x40 {
            match write {
                Some(bytes) => self.config.write(off, bytes),
                None => self.config.read(off, data),
            }
            return Ok(());
        }
        if off < PCIEXBAR || finish > PCIEXBAR + 8 {
            return self.smram.as_mut().ok_or(PciConfigError::UnsupportedRegister)?.access(off, data, write, apply);
        }
        match write {
            Some(bytes) => {
                let mut candidate = self.value().to_le_bytes();
                candidate[off - PCIEXBAR..finish - PCIEXBAR].copy_from_slice(bytes);
                self.decode(u64::from_le_bytes(candidate))?;
                self.config.raw_mut()[PCIEXBAR..PCIEXBAR + 8].copy_from_slice(&candidate);
            }
            None => self.config.read(off, data),
        }
        Ok(())
    }
}

/// Checked configuration router for an explicitly cold-only x64 board slice.
///
/// No root or generic ECAM handle escapes: CF8 and ECAM share one MCH state and
/// `&mut self` serializes decoding with register changes. The entire aperture
/// stays reserved when disabled/shrunk. BAR MMIO is opt-in with explicit windows
/// and an immutable private-range inventory. Invalid BAR config stays readable
/// and repairable: validation precedes BAR callbacks, not config callbacks.
/// The caller must also exclude the aperture from hidden HV RAM/ROM mappings;
/// this device layer can verify only supplied GuestMemory and the fixed bus.
#[derive(Debug)]
pub struct MchPci {
    root: Arc<PciRoot>,
    cf8: PciCf8,
    fixed: Bus,
    aperture: BusRange,
    bar_windows: Vec<BusRange>,
    bar_exclusions: Vec<BusRange>,
}

impl MchPci {
    pub fn new(devices: Vec<(u8, SharedPciDevice)>, fixed_mmio: Bus, ram: &GuestMemory,
               ecam_aperture: BusRange) -> Result<Self, MchError> {
        Self::with_bar_windows(devices, fixed_mmio, ram, ecam_aperture, Vec::new(), Vec::new())
    }

    /// Enable checked BAR MMIO in at most two disjoint, page-aligned windows.
    /// `reserved_ranges` must be the board's COMPLETE private/ROM/MMIO inventory,
    /// bound to its actual HV mappings (including both SMRAM ranges and KVM's
    /// private pages). GuestMemory cannot reveal those hidden mappings. We can
    /// check the supplied inventory, but cannot prove that it is complete.
    ///
    /// Every BAR transaction try-locks all distinct endpoints in slot order and
    /// keeps the guards through validation and dispatch. Busy/poison fails before
    /// callbacks. Endpoint and IRQ callbacks must not synchronously reenter any
    /// PCI endpoint/root lock. No hotplug ports or snapshot path are exposed.
    pub fn with_bar_windows(devices: Vec<(u8, SharedPciDevice)>, fixed_mmio: Bus,
                            ram: &GuestMemory, ecam_aperture: BusRange,
                            bar_windows: Vec<BusRange>, reserved_ranges: Vec<BusRange>) -> Result<Self, MchError> {
        Self::build(devices, fixed_mmio, ram, ecam_aperture, bar_windows, reserved_ranges, false, None)
    }

    /// Bind fixed 128 KiB default-SMBASE and 8 MiB TSEG controls. The caller
    /// MUST first configure that exact backend profile. Inventory checks do not
    /// prove hidden HV ownership or that a full 512 MiB board has been populated.
    /// Private ranges stay excluded from GuestMemory/DMA even while CPU-open.
    pub fn with_q35_smram(devices: Vec<(u8, SharedPciDevice)>, fixed_mmio: Bus,
                         ram: &GuestMemory, ecam_aperture: BusRange,
                         bar_windows: Vec<BusRange>, reserved_ranges: Vec<BusRange>) -> Result<Self, MchError> {
        Self::q35_layout(&fixed_mmio, ram, ecam_aperture, &bar_windows, &reserved_ranges)?;
        Self::build(devices, fixed_mmio, ram, ecam_aperture, bar_windows, reserved_ranges, true, None)
    }

    /// Add the checked LPC PM timer at slot 31.0 to the fixed-Q35 SMRAM model.
    /// All `with_q35_smram` backend/inventory obligations still apply. The LPC
    /// owns no generic endpoint handle. APM SMI needs its stopped-CPU callback;
    /// normal S5 needs its typed terminal consumer. SCI and RTC are separate.
    /// This is a cold-only device composition, not an OVMF/Windows VMM profile.
    pub fn with_q35_lpc(devices: Vec<(u8, SharedPciDevice)>, fixed_mmio: Bus,
                       ram: &GuestMemory, ecam_aperture: BusRange,
                       bar_windows: Vec<BusRange>, reserved_ranges: Vec<BusRange>,
                       lpc_pio_layout: LpcPioLayout) -> Result<Self, MchError> {
        Self::q35_layout(&fixed_mmio, ram, ecam_aperture, &bar_windows, &reserved_ranges)?;
        Self::build(devices, fixed_mmio, ram, ecam_aperture, bar_windows, reserved_ranges,
            true, Some(Ich9Lpc::new(lpc_pio_layout)))
    }

    fn q35_layout(fixed_mmio: &Bus, ram: &GuestMemory, ecam_aperture: BusRange,
                  bar_windows: &[BusRange], reserved_ranges: &[BusRange]) -> Result<(), MchError> {
        for private in [BusRange { base: 0x30000, len: 0x20000 }, BusRange { base: 0x1f800000, len: 0x800000 }] {
            let limit = private.base + private.len;
            if !reserved_ranges.iter().any(|r| contains(*r, private.base, limit))
                || overlaps(ecam_aperture, private.base, limit)
                || bar_windows.iter().any(|r| overlaps(*r, private.base, limit))
                || fixed_mmio.ranges().any(|r| overlaps(r, private.base, limit))
                || ram.regions().any(|r| overlaps(BusRange { base: r.start().0, len: r.len() }, private.base, limit)) {
                return Err(MchError::Layout);
            }
        }
        Ok(())
    }

    fn build(mut devices: Vec<(u8, SharedPciDevice)>, fixed_mmio: Bus,
             ram: &GuestMemory, ecam_aperture: BusRange,
             bar_windows: Vec<BusRange>, reserved_ranges: Vec<BusRange>, smram: bool, lpc: Option<Ich9Lpc>) -> Result<Self, MchError> {
        let finish = end(ecam_aperture).ok_or(MchError::Layout)?;
        if ecam_aperture.len != APERTURE_SIZE || ecam_aperture.base % APERTURE_SIZE != 0
            || finish > ADDRESS_LIMIT
            || fixed_mmio.ranges().any(|r| overlaps(r, ecam_aperture.base, finish))
            || ram.regions().any(|r| overlaps(BusRange { base: r.start().0, len: r.len() }, ecam_aperture.base, finish)) {
            return Err(MchError::Layout);
        }
        let mut slots = 0u32;
        for (slot, _) in &devices {
            if !(1..=31).contains(slot) || slots & (1u32 << slot) != 0 || (lpc.is_some() && *slot == 31) {
                return Err(MchError::Slot);
            }
            slots |= 1u32 << slot;
        }
        if bar_windows.len() > 2 || reserved_ranges.len() > 64
            || (!bar_windows.is_empty() && reserved_ranges.is_empty())
            || reserved_ranges.iter().any(|r| end(*r).is_none_or(|limit| limit > ADDRESS_LIMIT)) {
            return Err(MchError::BarLayout);
        }
        let mut bar_exclusions = reserved_ranges;
        bar_exclusions.push(ecam_aperture);
        bar_exclusions.extend(fixed_mmio.ranges());
        bar_exclusions.extend(ram.regions().map(|r| BusRange { base: r.start().0, len: r.len() }));
        for (i, window) in bar_windows.iter().enumerate() {
            let limit = end(*window).ok_or(MchError::BarLayout)?;
            if window.base % 4096 != 0 || window.len % 4096 != 0 || limit > ADDRESS_LIMIT
                || bar_exclusions.iter().any(|r| overlaps(*r, window.base, limit))
                || bar_windows[..i].iter().any(|r| overlaps(*r, window.base, limit)) {
                return Err(MchError::BarLayout);
            }
        }
        if !bar_windows.is_empty() {
            for (i, (_, device)) in devices.iter().enumerate() {
                if devices[..i].iter().any(|(_, other)| Arc::ptr_eq(device, other)) {
                    return Err(MchError::EndpointAlias);
                }
            }
            devices.sort_by_key(|(slot, _)| *slot);
        }
        let mut host = MchConfig::new(ecam_aperture);
        if smram { host.smram = Some(SmramConfig::new()); }
        let root = PciRoot::with_mch(devices, host, lpc);
        let cf8 = PciCf8::new(root.clone());
        Ok(Self { root, cf8, fixed: fixed_mmio, aperture: ecam_aperture, bar_windows, bar_exclusions })
    }

    pub fn pio_read(&mut self, port: u16, data: &mut [u8]) -> Result<(), MchError> {
        self.root.mch_check()?;
        Ok(self.cf8.read(port, data)?)
    }

    pub fn pio_write(&mut self, port: u16, data: &[u8]) -> Result<(), MchError> {
        self.root.mch_check()?;
        Ok(self.cf8.write(port, data)?)
    }

    /// Absolute LPC port access. The owner supplies nanoseconds since one
    /// monotonic cold epoch, not `Vm::clock` (currently zero on KVM). False
    /// means no decode. Invalid/unmapped reads leave the buffer unchanged.
    /// Disable/rebase does not restart the timer. Pause, reset and migration
    /// are not supported; the future board must bind its clock consistently.
    pub fn lpc_pio_read(&mut self, port: u16, data: &mut [u8], elapsed_ns: u64) -> Result<bool, MchError> {
        self.root.mch_check()?;
        Ok(self.root.lpc_pio(port, data, None, elapsed_ns)?)
    }

    pub fn lpc_pio_write(&mut self, port: u16, data: &[u8], elapsed_ns: u64) -> Result<bool, MchError> {
        self.root.mch_check()?;
        Ok(self.root.lpc_pio(port, &mut [], Some(data), elapsed_ns)?)
    }

    /// Bind THIS exit's context to an actual stopped-CPU SMI request. The
    /// callback must not RUN, retry, complete another exit or reenter any
    /// router/endpoint lock. It must independently validate the actual CPU.
    /// Ok means request accepted, not delivered: observe the handler and RSM
    /// separately. Err/panic permanently poisons every route before readback
    /// commits; the owner must stop the VM. No callback is stored.
    pub fn lpc_pio_write_with_smi(&mut self, port: u16, data: &[u8], elapsed_ns: u64,
        context: super::LpcIoContext, inject: &mut dyn FnMut(super::LpcSmiRequest) -> Result<(), LpcError>) -> Result<bool, MchError> {
        self.root.mch_check()?;
        Ok(self.root.lpc_pio_write_with_smi(port, data, elapsed_ns, context, inject)?)
    }

    /// Consume AcpiS5 as terminal on the actual exiting CPU: no more RUN or
    /// read completion, then drop the owned VM/CPU state. Context describes
    /// THIS exit, not cached mode. The router latches terminal before returning
    /// the outcome; it does not secretly stop an independently owned CPU.
    /// Existing bool APIs reject power effects instead of discarding them.
    pub fn lpc_pio_write_with_power(&mut self, port: u16, data: &[u8], elapsed_ns: u64,
        context: super::LpcIoContext) -> Result<super::LpcPioWriteOutcome, MchError> {
        self.root.mch_check()?;
        Ok(self.root.lpc_pio_write_with_power(port, data, elapsed_ns, context)?)
    }

    /// The nonstored callback must apply the requested state on the sole vCPU
    /// stopped on THIS write exit, and return the actual result. It must not
    /// reenter any PCI/router/endpoint lock. Err, mismatch or panic permanently
    /// poisons every route; the caller must then stop the VM.
    pub fn pio_write_with_smram(&mut self, port: u16, data: &[u8],
        apply: &mut dyn FnMut(MchSmramState) -> Result<MchSmramState, PciConfigError>) -> Result<(), MchError> {
        self.root.mch_check()?;
        Ok(self.cf8.write_with_smram(port, data, Some(apply))?)
    }

    /// Same callback contract as `pio_write_with_smram`, through active ECAM.
    pub fn mmio_write_with_smram(&mut self, address: u64, data: &[u8],
        apply: &mut dyn FnMut(MchSmramState) -> Result<MchSmramState, PciConfigError>) -> Result<bool, MchError> {
        self.mmio(address, &mut [], Some(data), Some(apply))
    }

    /// Absolute-GPA access. False is unmapped; errors and unmapped reads leave
    /// the buffer unchanged. Every full span is checked before any callback.
    pub fn mmio_read(&mut self, address: u64, data: &mut [u8]) -> Result<bool, MchError> {
        self.mmio(address, data, None, None)
    }

    pub fn mmio_write(&mut self, address: u64, data: &[u8]) -> Result<bool, MchError> {
        self.mmio(address, &mut [], Some(data), None)
    }

    fn mmio(&mut self, address: u64, data: &mut [u8], write: Option<&[u8]>, apply: Option<Apply<'_>>) -> Result<bool, MchError> {
        self.root.mch_check()?;
        let len = write.map_or(data.len(), |bytes| bytes.len());
        if !matches!(len, 1 | 2 | 4 | 8) { return Err(MchError::Width); }
        if address % len as u64 != 0 { return Err(MchError::Alignment); }
        let finish = address.checked_add(len as u64).ok_or(MchError::Boundary)?;
        if overlaps(self.aperture, address, finish) {
            if !contains(self.aperture, address, finish) { return Err(MchError::Boundary); }
            let Some(window) = self.root.mch_window() else { return Ok(false) };
            if !overlaps(window, address, finish) { return Ok(false); }
            if !contains(window, address, finish) { return Err(MchError::Boundary); }
            self.root.config_access_with_smram(address - window.base, data, write, apply)?;
            return Ok(true);
        }
        if let Some(window) = self.bar_windows.iter().find(|r| overlaps(**r, address, finish)) {
            if !contains(*window, address, finish) { return Err(MchError::Boundary); }
            return self.root.checked_bar_mmio(address, data, write, &self.bar_windows, &self.bar_exclusions);
        }
        let hit = self.fixed.ranges().find(|range| overlaps(*range, address, finish));
        let Some(range) = hit else { return Ok(false) };
        if !contains(range, address, finish) { return Err(MchError::Boundary); }
        Ok(match write {
            Some(bytes) => self.fixed.write(address, bytes),
            None => self.fixed.read(address, data),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use crate::{BusBuilder, BusDevice};
    use crate::pci::{BarSpec, PciDevice};
    use vroom_memory::{GuestAddress, page_size};
    use vroom_snapshot::Snapshottable;

    const BASE: u64 = 0xe000_0000;
    const APERTURE: BusRange = BusRange { base: BASE, len: APERTURE_SIZE };
    const SLOT1: u64 = 1 << 15;

    #[derive(Default, Debug, Clone, PartialEq, Eq)]
    struct Counts { reads: usize, writes: usize, saves: usize, restores: usize, fixed: usize,
                    bar_reads: usize, bar_writes: usize, last_bar: Option<(usize, u64)> }
    struct Endpoint { config: ConfigSpace, counts: Arc<Mutex<Counts>> }
    impl PciDevice for Endpoint {
        fn config(&self) -> &ConfigSpace { &self.config }
        fn config_mut(&mut self) -> &mut ConfigSpace { &mut self.config }
        fn config_read(&mut self, off: usize, data: &mut [u8]) {
            self.counts.lock().unwrap().reads += 1;
            self.config.read(off, data);
        }
        fn config_write(&mut self, off: usize, data: &[u8]) {
            self.counts.lock().unwrap().writes += 1;
            self.config.write(off, data);
        }
        fn bar_read(&mut self, bar: usize, off: u64, data: &mut [u8]) {
            let mut counts = self.counts.lock().unwrap();
            counts.bar_reads += 1; counts.last_bar = Some((bar, off));
            crate::put_le(data, 0x1122_3344 + off);
        }
        fn bar_write(&mut self, bar: usize, off: u64, _: &[u8]) {
            let mut counts = self.counts.lock().unwrap();
            counts.bar_writes += 1; counts.last_bar = Some((bar, off));
        }
        fn id(&self) -> String { "mch-test-endpoint".into() }
        fn save(&mut self) -> vroom_snapshot::Result<Vec<u8>> {
            self.counts.lock().unwrap().saves += 1;
            Ok(vec![1, 2, 3])
        }
        fn restore(&mut self, _: &[u8]) -> vroom_snapshot::Result<()> {
            self.counts.lock().unwrap().restores += 1;
            Ok(())
        }
    }
    struct Fixed(Arc<Mutex<Counts>>);
    impl BusDevice for Fixed {
        fn read(&mut self, _: u64, data: &mut [u8]) { self.0.lock().unwrap().fixed += 1; data.fill(0x44); }
        fn write(&mut self, _: u64, _: &[u8]) { self.0.lock().unwrap().fixed += 1; }
    }

    fn endpoint() -> (SharedPciDevice, Arc<Mutex<Counts>>) {
        let counts = Arc::new(Mutex::new(Counts::default()));
        let mut config = ConfigSpace::new(PciId { vendor: 0x1234, device: 0x5678,
            class: 0x01_08_02, revision: 1, subsys_vendor: 0, subsys: 0 }, 0);
        config.set_wmask(0x80, &[0xff; 8]);
        config.add_bar(0, BarSpec { size: 4096, is64: false, prefetch: false });
        (Arc::new(Mutex::new(Endpoint { config, counts: counts.clone() })), counts)
    }
    fn fixture() -> (MchPci, Arc<Mutex<Counts>>) {
        let (endpoint, counts) = endpoint();
        let mut fixed = BusBuilder::new();
        fixed.insert(0x1000, 6, Arc::new(Mutex::new(Fixed(counts.clone())))).unwrap();
        let router = MchPci::new(vec![(1, endpoint)], fixed.build(), &GuestMemory::default(), APERTURE).unwrap();
        (router, counts)
    }
    fn select(router: &mut MchPci, slot: u8, reg: u8) {
        router.pio_write(0xcf8, &(0x8000_0000u32 | (u32::from(slot) << 11) | u32::from(reg & !3)).to_le_bytes()).unwrap();
    }
    fn config_write(router: &mut MchPci, slot: u8, reg: u8, bytes: &[u8]) -> Result<(), MchError> {
        select(router, slot, reg);
        router.pio_write(0xcfc + u16::from(reg & 3), bytes)
    }
    fn value(router: &mut MchPci) -> u64 {
        let mut low = [0; 4]; let mut high = [0; 4];
        select(router, 0, 0x60); router.pio_read(0xcfc, &mut low).unwrap();
        select(router, 0, 0x64); router.pio_read(0xcfc, &mut high).unwrap();
        u64::from(u32::from_le_bytes(low)) | (u64::from(u32::from_le_bytes(high)) << 32)
    }
    fn enable(router: &mut MchPci) {
        // Pinned OVMF's actual CF8 initialization order, not a direct setter.
        config_write(router, 0, 0x60, &0u32.to_le_bytes()).unwrap();
        config_write(router, 0, 0x64, &0u32.to_le_bytes()).unwrap();
        config_write(router, 0, 0x60, &(BASE as u32 | 1).to_le_bytes()).unwrap();
    }

    #[test]
    fn ovmf_cf8_sequence_enables_real_shared_config_dispatch() {
        let (mut router, counts) = fixture();
        assert_eq!(value(&mut router), 0xb000_0000);
        let mut data = [0xa5; 4];
        assert_eq!(router.mmio_read(BASE + SLOT1, &mut data), Ok(false));
        assert_eq!(data, [0xa5; 4]);
        assert_eq!(counts.lock().unwrap().reads, 0);
        enable(&mut router);
        assert_eq!(value(&mut router), BASE | 1);
        assert_eq!(router.mmio_read(BASE + SLOT1, &mut data), Ok(true));
        assert_eq!(data, [0x34, 0x12, 0x78, 0x56]);
        assert_eq!(counts.lock().unwrap().reads, 1);
        router.mmio_write(BASE + SLOT1 + 0x80, &[1, 2, 3, 4]).unwrap();
        select(&mut router, 1, 0x80); router.pio_read(0xcfc, &mut data).unwrap();
        assert_eq!(data, [1, 2, 3, 4]);
        config_write(&mut router, 1, 0x80, &[9, 8, 7, 6]).unwrap();
        router.mmio_read(BASE + SLOT1 + 0x80, &mut data).unwrap();
        assert_eq!(data, [9, 8, 7, 6]);
        let mut host = [0; 4]; router.mmio_read(BASE, &mut host).unwrap();
        assert_eq!(host, [0x86, 0x80, 0xc0, 0x29]);
    }

    #[test]
    fn live_rebase_shrink_disable_and_ecam_self_write_are_atomic() {
        let (mut router, _) = fixture(); enable(&mut router);
        let moved = BASE + (64 << 20);
        router.mmio_write(BASE + 0x60, &(moved | 5).to_le_bytes()).unwrap();
        assert_eq!(value(&mut router), moved | 5);
        let mut data = [0xa5; 4];
        assert_eq!(router.mmio_read(BASE + SLOT1, &mut data), Ok(false));
        assert_eq!(data, [0xa5; 4]);
        assert_eq!(router.mmio_read(moved + SLOT1, &mut data), Ok(true));
        assert_eq!(data, [0x34, 0x12, 0x78, 0x56]);
        router.mmio_write(moved + 0x60, &(moved | 4).to_le_bytes()).unwrap();
        assert_eq!(value(&mut router), moved | 4);
        assert_eq!(router.mmio_read(moved, &mut data), Ok(false));
        config_write(&mut router, 0, 0x60, &(BASE as u32 | 3).to_le_bytes()).unwrap();
        assert_eq!(router.root.mch_window(), Some(BusRange { base: BASE, len: 128 << 20 }));
        for bits in [0u64, 2, 4] {
            config_write(&mut router, 0, 0x60, &(BASE as u32 | bits as u32 | 1).to_le_bytes()).unwrap();
            let length = APERTURE_SIZE >> (bits / 2);
            assert_eq!(router.mmio_read(BASE + length - 4, &mut data), Ok(true));
            assert_eq!(data, [0xff; 4]); // Last absent function, still decoded.
            assert_eq!(router.mmio_read(BASE + length, &mut data), Ok(false));
        }
    }

    #[test]
    fn byte_word_high_and_disabled_programming_preserve_one_register() {
        let (mut router, _) = fixture(); enable(&mut router);
        config_write(&mut router, 0, 0x60, &[0]).unwrap();
        assert_eq!(value(&mut router), BASE);
        config_write(&mut router, 0, 0x60, &0u32.to_le_bytes()).unwrap();
        config_write(&mut router, 0, 0x64, &1u32.to_le_bytes()).unwrap();
        assert_eq!(value(&mut router), 1 << 32); // Staging is allowed while disabled.
        assert!(config_write(&mut router, 0, 0x60, &[1]).is_err());
        assert_eq!(value(&mut router), 1 << 32);
        config_write(&mut router, 0, 0x64, &0u32.to_le_bytes()).unwrap();
        config_write(&mut router, 0, 0x62, &0xe000u16.to_le_bytes()).unwrap();
        config_write(&mut router, 0, 0x60, &[1]).unwrap();
        assert_eq!(value(&mut router), BASE | 1);
    }

    #[test]
    fn rejected_pciexbar_writes_preserve_decode_and_cf8_latch() {
        let (mut router, counts) = fixture(); enable(&mut router);
        for invalid in [BASE | 7, BASE | 9, BASE | (1 << 36) | 1,
                        BASE | (1 << 26) | 1, BASE - APERTURE_SIZE | 1] {
            assert!(router.mmio_write(BASE + 0x60, &invalid.to_le_bytes()).is_err());
            assert_eq!(value(&mut router), BASE | 1);
            assert_eq!(router.root.mch_window(), Some(APERTURE));
        }
        select(&mut router, 0, 0x60);
        let mut before = [0; 4]; router.pio_read(0xcf8, &mut before).unwrap();
        assert_eq!(router.pio_write(0xcfc, &(BASE as u32 | 7).to_le_bytes()),
            Err(MchError::Port(PciCf8Error::Host(PciConfigError::Length))));
        let mut after = [0; 4]; router.pio_read(0xcf8, &mut after).unwrap();
        assert_eq!(before, after);
        assert_eq!(counts.lock().unwrap().reads, 0);
        assert_eq!(counts.lock().unwrap().writes, 0);
    }

    #[test]
    fn malformed_spans_do_not_mutate_buffers_or_reach_devices() {
        let (mut router, counts) = fixture(); enable(&mut router);
        for (address, size) in [(BASE + SLOT1, 3), (BASE + SLOT1 + 0xfff, 4),
                                (BASE + APERTURE_SIZE - 1, 2), (u64::MAX - 7, 8)] {
            let mut data = vec![0xa5; size];
            assert!(router.mmio_read(address, &mut data).is_err());
            assert!(router.mmio_write(address, &data).is_err());
            assert_eq!(data, vec![0xa5; size]);
        }
        assert_eq!(counts.lock().unwrap().reads, 0);
        assert_eq!(counts.lock().unwrap().writes, 0);
        assert_eq!(value(&mut router), BASE | 1);
    }

    #[test]
    fn deferred_chipset_registers_fail_without_fake_readback() {
        let (mut router, _) = fixture(); enable(&mut router);
        for offset in [0x50, 0x90, 0x9c, 0x9d, 0x9e, 0xac, 0xb0, 0x100] {
            let mut data = [0xa5];
            assert_eq!(router.mmio_read(BASE + offset, &mut data),
                Err(MchError::Config(PciConfigError::UnsupportedRegister)));
            assert_eq!(data, [0xa5]);
            assert!(router.mmio_write(BASE + offset, &[0xff]).is_err());
        }
        assert_eq!(value(&mut router), BASE | 1);
        // A write spanning the normal header and unsupported registers is rejected whole.
        assert!(router.root.config_access(0x3c, &mut [], Some(&[0xff; 8])).is_err());
    }

    #[test]
    fn fixed_bus_full_spans_checked_before_forwarding() {
        let (mut router, counts) = fixture();
        let mut data = [0xa5; 4];
        assert_eq!(router.mmio_read(0x1000, &mut data), Ok(true));
        assert_eq!(data, [0x44; 4]);
        data.fill(0xa5);
        assert_eq!(router.mmio_read(0x1004, &mut data), Err(MchError::Boundary));
        assert_eq!(router.mmio_write(0x1004, &[0; 4]), Err(MchError::Boundary));
        assert_eq!(data, [0xa5; 4]);
        assert_eq!(counts.lock().unwrap().fixed, 1);
        assert_eq!(router.mmio_read(0x2000, &mut data), Ok(false));
        assert_eq!(data, [0xa5; 4]);
    }

    #[test]
    fn reserved_aperture_rejects_actual_ram_bus_and_bad_layouts() {
        let ram = GuestMemory::new(&[(GuestAddress(BASE), page_size() as u64)]).unwrap();
        assert!(matches!(MchPci::new(vec![], Bus::default(), &ram, APERTURE), Err(MchError::Layout)));
        let mut bus = BusBuilder::new();
        bus.insert(BASE + 0x1000, 4, Arc::new(Mutex::new(Fixed(Arc::default())))).unwrap();
        assert!(matches!(MchPci::new(vec![], bus.build(), &GuestMemory::default(), APERTURE), Err(MchError::Layout)));
        for aperture in [BusRange { base: BASE + 1, len: APERTURE_SIZE },
                         BusRange { base: BASE, len: 0 }, BusRange { base: BASE, len: 64 << 20 },
                         BusRange { base: ADDRESS_LIMIT, len: APERTURE_SIZE },
                         BusRange { base: u64::MAX, len: APERTURE_SIZE }] {
            assert!(matches!(MchPci::new(vec![], Bus::default(), &GuestMemory::default(), aperture), Err(MchError::Layout)));
        }
    }

    #[test]
    fn invalid_slots_fail_before_endpoint_callbacks() {
        let (endpoint, counts) = endpoint();
        for devices in [vec![(0, endpoint.clone())], vec![(32, endpoint.clone())],
                        vec![(1, endpoint.clone()), (1, endpoint.clone())]] {
            assert!(matches!(MchPci::new(devices, Bus::default(), &GuestMemory::default(), APERTURE), Err(MchError::Slot)));
        }
        let counts = counts.lock().unwrap();
        assert_eq!((counts.reads, counts.writes, counts.saves, counts.restores), (0, 0, 0, 0));
    }

    #[test]
    fn cold_root_snapshot_escape_fails_before_callbacks_or_decode() {
        let (mut router, counts) = fixture(); enable(&mut router);
        // Not a public route: exercise the internal defense in depth.
        let mut forbidden = router.root.ecam_port();
        assert!(forbidden.save().unwrap_err().to_string().contains("cold-boot only"));
        assert!(forbidden.restore(b"not JSON").unwrap_err().to_string().contains("cold-boot only"));
        assert!(forbidden.reset(b"not JSON").unwrap_err().to_string().contains("cold-boot only"));
        assert_eq!(value(&mut router), BASE | 1);
        assert_eq!(counts.lock().unwrap().saves, 0);
        assert_eq!(counts.lock().unwrap().restores, 0);
    }

    #[test]
    fn legacy_snapshot_matches_pre_change_golden_bytes() {
        let mut ecam = PciRoot::new(vec![]).ecam_port();
        let original = ecam.save().unwrap();
        // Captured before the HostBridge change: 8277 JSON bytes, SHA256
        // 2eb94c1e70ec2f342ecb0ebe31e40e0df84225a53f22888ad739cdf7fb3cc1c8.
        // FNV is a compact regression fingerprint, not a security check.
        let fingerprint = original.iter().fold(0xcbf29ce484222325u64,
            |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3));
        assert_eq!(original.len(), 8277);
        assert_eq!(fingerprint, 0x3405b83fbf8b3d5f);
        ecam.write(4, &2u16.to_le_bytes());
        assert_ne!(ecam.save().unwrap(), original);
        ecam.restore(&original).unwrap();
        assert_eq!(ecam.save().unwrap(), original);
        ecam.reset(&original).unwrap();
        assert_eq!(ecam.save().unwrap(), original);
    }

    #[test]
    fn endpoint_bar_config_is_preserved_but_bar_mmio_is_not_claimed() {
        let (mut router, _) = fixture(); enable(&mut router);
        config_write(&mut router, 1, 0x10, &0x4000u32.to_le_bytes()).unwrap();
        config_write(&mut router, 1, 4, &2u16.to_le_bytes()).unwrap();
        let mut data = [0xa5; 4];
        assert_eq!(router.mmio_read(0x4000, &mut data), Ok(false));
        assert_eq!(data, [0xa5; 4]);
        router.mmio_read(BASE + SLOT1 + 0x10, &mut data).unwrap();
        assert_eq!(u32::from_le_bytes(data), 0x4000);
    }
    const BAR_LOW: BusRange = BusRange { base: 0x4000_0000, len: 0x1000_0000 };
    const BAR_HIGH: BusRange = BusRange { base: 0x8_0000_0000, len: 0x1_0000_0000 };

    fn private_ranges() -> Vec<BusRange> {
        vec![BusRange { base: 0x30000, len: 0x20000 },
             BusRange { base: 0x1f80_0000, len: 0x80_0000 },
             BusRange { base: 0xffbf_c000, len: 0x4000 },
             BusRange { base: 0xffc0_0000, len: 0x40_0000 },
             BusRange { base: 0xfec0_0000, len: 0x1000 },
             BusRange { base: 0xfee0_0000, len: 0x1000 }]
    }
    fn smram_router() -> (MchPci, Arc<Mutex<Counts>>) {
        let (device, counts) = endpoint();
        let mut fixed = BusBuilder::new();
        fixed.insert(0x1000, 8, Arc::new(Mutex::new(Fixed(counts.clone())))).unwrap();
        let router = MchPci::with_q35_smram(vec![(1, device)], fixed.build(), &GuestMemory::default(),
            APERTURE, vec![BAR_LOW], private_ranges()).unwrap();
        (router, counts)
    }

    fn lpc_layout() -> LpcPioLayout {
        LpcPioLayout::new(vec![BusRange { base: 0x600, len: 0x80 }, BusRange { base: 0x800, len: 0x80 }],
            vec![BusRange { base: 0x700, len: 0x20 }]).unwrap()
    }
    fn lpc_router() -> MchPci {
        MchPci::with_q35_lpc(vec![], Bus::default(), &GuestMemory::default(), APERTURE,
            vec![], private_ranges(), lpc_layout()).unwrap()
    }

    #[test]
    fn lpc_cf8_and_ecam_share_checked_config_and_actual_port_decoding() {
        let mut router = lpc_router(); enable(&mut router);
        let slot = BASE + (31 << 15);
        let mut data = [0; 4];
        select(&mut router, 31, 0);
        router.pio_read(0xcfc, &mut data).unwrap(); assert_eq!(u32::from_le_bytes(data), 0x2918_8086);
        router.mmio_read(slot + 8, &mut data).unwrap(); assert_eq!(u32::from_le_bytes(data), 0x0601_0002);
        router.mmio_read(slot + 4, &mut data).unwrap(); assert_eq!(data, [0; 4]);
        select(&mut router, 31, 0x40); router.pio_write(0xcfc, &0x600u32.to_le_bytes()).unwrap();
        router.mmio_read(slot + 0x40, &mut data).unwrap(); assert_eq!(u32::from_le_bytes(data), 0x601);
        router.mmio_write(slot + 0x44, &[0x80]).unwrap();
        assert!(router.lpc_pio_read(0x608, &mut data, 1_000_000_000).unwrap());
        assert_eq!(u32::from_le_bytes(data), 3_579_545);
        let mut callbacks = 0;
        router.mmio_write_with_smram(slot + 0x41, &[8], &mut |s| { callbacks += 1; Ok(s) }).unwrap();
        assert_eq!(callbacks, 0, "LPC config never invokes the SMRAM mapping backend");
        select(&mut router, 31, 0x40); router.pio_read(0xcfc, &mut data).unwrap();
        assert_eq!(u32::from_le_bytes(data), 0x801);
        let before = data;
        assert!(!router.lpc_pio_read(0x608, &mut data, 2_000_000_000).unwrap()); assert_eq!(data, before);
        assert!(router.lpc_pio_read(0x808, &mut data, 2_000_000_000).unwrap());
        assert_eq!(u32::from_le_bytes(data), 7_159_090);
        assert_eq!(router.mmio_write(slot + 0x44, &[0x81]),
            Err(MchError::Config(PciConfigError::Lpc(LpcError::ReservedBits))));
        select(&mut router, 31, 0x44); router.pio_read(0xcfc, &mut data[..1]).unwrap(); assert_eq!(data[0], 0x80);
        assert_eq!(router.pio_write(0xcfc, &[0; 2]),
            Err(MchError::Port(PciCf8Error::Host(PciConfigError::Lpc(LpcError::UnsupportedRegister)))));
        router.mmio_read(slot + 0x10, &mut data).unwrap(); assert_eq!(data, [0; 4], "LPC has no BARs");
        data.fill(0xa5);
        assert_eq!(router.mmio_read(slot + 0xf0, &mut data),
            Err(MchError::Config(PciConfigError::Lpc(LpcError::UnsupportedRegister))));
        assert_eq!(data, [0xa5; 4], "RCBA must not be fabricated");
    }

    #[test]
    fn lpc_constructor_is_opt_in_and_cannot_shadow_an_endpoint_or_private_memory() {
        let (device, counts) = endpoint();
        assert!(matches!(MchPci::with_q35_lpc(vec![(31, device.clone())], Bus::default(), &GuestMemory::default(),
            APERTURE, vec![], private_ranges(), lpc_layout()), Err(MchError::Slot)));
        assert_eq!(counts.lock().unwrap().reads, 0);
        let mut old = MchPci::new(vec![(31, device)], Bus::default(), &GuestMemory::default(), APERTURE).unwrap();
        select(&mut old, 31, 0); let mut data = [0; 4]; old.pio_read(0xcfc, &mut data).unwrap();
        assert_eq!(u32::from_le_bytes(data), 0x5678_1234);
        assert!(!old.lpc_pio_read(0x608, &mut data, 0).unwrap());
        assert!(MchPci::with_q35_lpc(vec![], Bus::default(), &GuestMemory::default(), APERTURE,
            vec![], vec![], lpc_layout()).is_err());
        let ram = GuestMemory::new(&[(GuestAddress(0x30000), page_size() as u64)]).unwrap();
        assert!(MchPci::with_q35_lpc(vec![], Bus::default(), &ram, APERTURE,
            vec![], private_ranges(), lpc_layout()).is_err());
        let router = lpc_router();
        assert!(router.root.ecam_port().save().is_err());
    }

    #[test]
    fn smram_failure_also_poison_gates_lpc_config_and_timer_routes() {
        let mut router = lpc_router(); enable(&mut router);
        let slot = BASE + (31 << 15);
        router.mmio_write(slot + 0x40, &0x600u32.to_le_bytes()).unwrap();
        router.mmio_write(slot + 0x44, &[0x80]).unwrap();
        assert!(router.mmio_write_with_smram(BASE + 0x50, &[255; 2], &mut |_| Err(PciConfigError::SmramBackend)).is_err());
        let poisoned = MchError::Config(PciConfigError::SmramPoisoned);
        let mut data = [0xa5; 4];
        assert_eq!(router.lpc_pio_read(0x608, &mut data, 0), Err(poisoned));
        assert_eq!(router.lpc_pio_write(0x608, &[0; 4], 0), Err(poisoned));
        assert_eq!(router.lpc_pio_read(0x700, &mut data, 0), Err(poisoned));
        assert_eq!(router.mmio_read(slot, &mut data), Err(poisoned));
        assert_eq!(router.lpc_pio_write_with_power(0x604, &[0, 0x20], 0, super::super::LpcIoContext::Normal), Err(poisoned));
        assert_eq!(data, [0xa5; 4]);
    }

    #[test]
    fn lpc_smi_error_or_panic_poison_every_cold_router_route() {
        use super::super::LpcIoContext;
        for panic in [false, true] {
            let (device, counts) = endpoint();
            let mut fixed = BusBuilder::new();
            fixed.insert(0x1000, 8, Arc::new(Mutex::new(Fixed(counts.clone())))).unwrap();
            let mut router = MchPci::with_q35_lpc(vec![(1, device)], fixed.build(), &GuestMemory::default(),
                APERTURE, vec![BAR_LOW], private_ranges(), lpc_layout()).unwrap();
            enable(&mut router);
            let slot = BASE + (31 << 15);
            router.mmio_write(slot + 0x40, &0x600u32.to_le_bytes()).unwrap();
            router.mmio_write(slot + 0x44, &[0x80]).unwrap();
            router.lpc_pio_write(0x630, &0x21u32.to_le_bytes(), 0).unwrap();
            let mut calls = 0;
            let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| router.lpc_pio_write_with_smi(
                0xb2, &[0x42], 0, LpcIoContext::Normal, &mut |_| {
                    calls += 1; if panic { panic!("backend panic") } Err(LpcError::SmiBackend)
                })));
            if panic { assert!(failure.is_err()); } else { assert_eq!(failure.unwrap(), Err(MchError::Lpc(LpcError::SmiBackend))); }
            assert_eq!(calls, 1);
            let before = counts.lock().unwrap().clone();
            let error = MchError::Config(PciConfigError::Lpc(LpcError::SmiPoisoned));
            let mut data = [0xa5; 4];
            for port in [0xcf8, 0xcfc] {
                assert_eq!(router.pio_read(port, &mut data), Err(error));
                assert_eq!(router.pio_write(port, &[0; 4]), Err(error));
                assert_eq!(router.pio_write_with_smram(port, &[0; 4], &mut |_| panic!("SMRAM effect")), Err(error));
            }
            for address in [BASE, slot, BASE + SLOT1, 0x1000, BAR_LOW.base, 0x3000] {
                assert_eq!(router.mmio_read(address, &mut data), Err(error));
                assert_eq!(router.mmio_write(address, &[0; 4]), Err(error));
                assert_eq!(router.mmio_write_with_smram(address, &[0; 4], &mut |_| panic!("SMRAM effect")), Err(error));
            }
            for port in [0x608, 0xb2, 0xb3, 0x700] {
                assert_eq!(router.lpc_pio_read(port, &mut data[..1], 0), Err(error));
                assert_eq!(router.lpc_pio_write(port, &[0], 0), Err(error));
                assert_eq!(router.lpc_pio_write_with_smi(port, &[0], 0, LpcIoContext::Normal,
                    &mut |_| panic!("SMI retry")), Err(error));
            }
            assert_eq!(router.lpc_pio_write_with_power(0x604, &[0, 0x20], 0, LpcIoContext::Normal), Err(error));
            assert_eq!(data, [0xa5; 4]);
            assert_eq!(*counts.lock().unwrap(), before);
        }
    }

    #[test]
    fn lpc_lock_shares_cf8_ecam_state_and_smi_callback_receives_committed_data() {
        use super::super::{LpcIoContext, LpcSmiRequest};
        let mut router = lpc_router(); enable(&mut router);
        let slot = BASE + (31 << 15);
        router.mmio_write(slot + 0x40, &0x600u32.to_le_bytes()).unwrap();
        router.mmio_write(slot + 0x44, &[0x80]).unwrap();
        router.lpc_pio_write(0x630, &0x21u32.to_le_bytes(), 0).unwrap();
        select(&mut router, 31, 0xa0); router.pio_write(0xcfc, &0x10u16.to_le_bytes()).unwrap();
        let mut read = [0; 4]; router.mmio_read(slot + 0xa0, &mut read[..2]).unwrap(); assert_eq!(read[..2], [0x10, 0]);
        router.mmio_write(slot + 0xa0, &[0; 2]).unwrap(); router.pio_read(0xcfc, &mut read[..2]).unwrap(); assert_eq!(read[..2], [0x10, 0]);
        router.lpc_pio_write(0x630, &0x20u32.to_le_bytes(), 0).unwrap();
        router.lpc_pio_read(0x630, &mut read, 0).unwrap(); assert_eq!(read, [0x21, 0, 0, 0]);
        router.lpc_pio_write(0xb3, &[0xa5], 0).unwrap();
        let mut requests = vec![];
        router.lpc_pio_write_with_smi(0xb2, &[0x42], 0, LpcIoContext::Normal,
            &mut |r| { requests.push(r); Ok(()) }).unwrap();
        assert_eq!(requests, [LpcSmiRequest { command: 0x42, data: 0xa5 }]);
        router.lpc_pio_read(0xb2, &mut read[..1], 0).unwrap(); assert_eq!(read[0], 0x42);
        assert_eq!(router.lpc_pio_write_with_smi(0xb2, &[0x43], 0, LpcIoContext::Smm,
            &mut |_| panic!("nested SMI")), Err(MchError::Lpc(LpcError::SmiContext)));
        router.lpc_pio_read(0xb2, &mut read[..1], 0).unwrap(); assert_eq!(read[0], 0x42);
    }

    #[test]
    fn consumed_s5_is_typed_once_and_all_routes_are_terminal() {
        use super::super::{LpcIoContext, LpcPioWriteOutcome};
        let (device, counts) = endpoint();
        let mut fixed = BusBuilder::new();
        fixed.insert(0x1000, 8, Arc::new(Mutex::new(Fixed(counts.clone())))).unwrap();
        let mut router = MchPci::with_q35_lpc(vec![(1, device)], fixed.build(), &GuestMemory::default(),
            APERTURE, vec![BAR_LOW], private_ranges(), lpc_layout()).unwrap(); enable(&mut router);
        let slot = BASE + (31 << 15);
        router.mmio_write(slot + 0x40, &0x600u32.to_le_bytes()).unwrap(); router.mmio_write(slot + 0x44, &[0x80]).unwrap();
        assert_eq!(router.lpc_pio_write(0x604, &[0, 0x20], 0), Err(MchError::Lpc(LpcError::PowerConsumerRequired)));
        assert_eq!(router.lpc_pio_write_with_power(0x604, &[0; 2], 0, LpcIoContext::Normal), Ok(LpcPioWriteOutcome::Handled));
        let mut read = [0xa5; 4]; assert!(router.lpc_pio_read(0x604, &mut read[..2], 0).unwrap()); assert_eq!(&read[..2], &[0; 2]);
        assert_eq!(router.lpc_pio_write_with_power(0x604, &[0, 0x20], 0, LpcIoContext::Normal), Ok(LpcPioWriteOutcome::AcpiS5));
        let before = counts.lock().unwrap().clone(); read.fill(0xa5);
        let terminal = MchError::Config(PciConfigError::Lpc(LpcError::PoweredOff));
        for port in [0xcf8, 0xcfc] {
            assert_eq!(router.pio_read(port, &mut read), Err(terminal)); assert_eq!(router.pio_write(port, &[0; 4]), Err(terminal));
            assert_eq!(router.pio_write_with_smram(port, &[0; 4], &mut |_| panic!("mapping after S5")), Err(terminal));
        }
        for addr in [BASE, slot, BASE + SLOT1, 0x1000, BAR_LOW.base, 0x3000] {
            assert_eq!(router.mmio_read(addr, &mut read), Err(terminal)); assert_eq!(router.mmio_write(addr, &[0; 4]), Err(terminal));
            assert_eq!(router.mmio_write_with_smram(addr, &[0; 4], &mut |_| panic!("mapping after S5")), Err(terminal));
        }
        for port in [0x604, 0x608, 0xb2, 0xb3, 0x700] {
            assert_eq!(router.lpc_pio_read(port, &mut read[..1], 0), Err(terminal)); assert_eq!(router.lpc_pio_write(port, &[0], 0), Err(terminal));
            assert_eq!(router.lpc_pio_write_with_smi(port, &[0], 0, LpcIoContext::Normal, &mut |_| panic!("SMI after S5")), Err(terminal));
            assert_eq!(router.lpc_pio_write_with_power(port, &[0, 0x20], 0, LpcIoContext::Normal), Err(terminal));
        }
        assert_eq!(*counts.lock().unwrap(), before); assert_eq!(read, [0xa5; 4]);
    }

    #[test]
    fn smram_cf8_and_ecam_share_applied_state_and_lock_masks() {
        use super::super::MchTsegState::*;
        let (mut router, _) = smram_router(); enable(&mut router);
        let mut states = Vec::new();
        let mut apply = |s| { states.push(s); Ok(s) };
        for (reg, bytes, expected) in [(0x50, vec![255, 255], vec![8, 0]),
            (0x9c, vec![255], vec![1]), (0x52, vec![2, 0], vec![2, 0]),
            (0xb0, vec![0, 0x20], vec![0, 0x20]),
            (0xa4, vec![0, 0, 0, 0x20], vec![0, 0, 0, 0x20]),
            (0xa8, vec![0, 0, 0, 0x20], vec![0, 0, 0, 0x20]),
            (0xac, vec![0, 0, 0x80, 0x1f], vec![0, 0, 0x80, 0x1f]),
            (0x9e, vec![0x3d], vec![0x3d]), (0x9d, vec![0xa], vec![0xa])] {
            select(&mut router, 0, reg);
            router.pio_write_with_smram(0xcfc + u16::from(reg & 3), &bytes, &mut apply).unwrap();
            let mut actual = vec![0xa5; bytes.len()];
            router.mmio_read(BASE + u64::from(reg), &mut actual).unwrap();
            assert_eq!(actual, expected);
        }
        for (reg, value, expected) in [(0x9e, 0x3c, 0x3c), (0x9e, 0x3d, 0x3d),
            (0x9d, 0x1a, 0x1a), (0x9c, 255, 2), (0x9c, 0, 2),
            (0x9d, 0x40, 0x1a), (0x9e, 0, 0x3d), (0x9d, 0x20, 0x3a)] {
            router.mmio_write_with_smram(BASE + u64::from(reg), &[value], &mut apply).unwrap();
            select(&mut router, 0, reg);
            let mut actual = [0xa5]; router.pio_read(0xcfc + u16::from(reg & 3), &mut actual).unwrap();
            assert_eq!(actual, [expected]);
        }
        assert_eq!(states.len(), 17, "exactly one application per accepted write");
        assert_eq!(states.iter().map(|s| s.tseg).collect::<Vec<_>>(),
            [vec![Open; 8], vec![Closed, Open, Closed], vec![Locked; 6]].concat());
        assert!(states[..12].iter().all(|s| !s.default_smbase_locked));
        assert!(states[12..].iter().all(|s| s.default_smbase_locked));
    }

    #[test]
    fn smram_callback_is_required_only_for_valid_new_bank_writes() {
        let (mut router, _) = smram_router(); enable(&mut router);
        assert_eq!(router.mmio_write(BASE + 0x50, &[255, 255]),
            Err(MchError::Config(PciConfigError::SmramBackendRequired)));
        let mut old = [0; 2]; router.mmio_read(BASE + 0x50, &mut old).unwrap(); assert_eq!(old, [255, 255]);
        let mut never = |_| panic!("unrelated/invalid write called mapping backend");
        router.mmio_write_with_smram(BASE + SLOT1 + 0x80, &[4], &mut never).unwrap();
        router.mmio_write_with_smram(BASE + 0x60, &(BASE | 1).to_le_bytes(), &mut never).unwrap();
        router.mmio_write_with_smram(0x1000, &[4], &mut never).unwrap();
        router.pio_write_with_smram(0xcf8, &0x80000050u32.to_le_bytes(), &mut never).unwrap();
        for (reg, bytes) in [(0x50, vec![8, 0]), (0x52, vec![0, 0]), (0x9d, vec![0x40]),
            (0x9e, vec![0xbc]), (0x9c, vec![4]), (0xac, vec![0, 0, 0, 0x20]),
            (0x50, vec![255; 4]), (0x9c, vec![255; 2]), (0x90, vec![0]), (0xb0, vec![0; 4])] {
            assert!(router.mmio_write_with_smram(BASE + reg, &bytes, &mut never).is_err());
            let mut data = [0; 2]; router.mmio_read(BASE + 0x50, &mut data).unwrap();
            assert_eq!(data, old);
        }
        // Recoverable invalid/missing-backend accesses must not poison.
        router.mmio_write_with_smram(BASE + 0x50, &[255, 255], &mut Ok).unwrap();
        router.mmio_read(BASE + 0x50, &mut old).unwrap(); assert_eq!(old, [8, 0]);
    }

    #[test]
    fn failed_mismatched_or_panicking_apply_poisons_every_router_route() {
        for fault in 0..3 {
            let (mut router, counts) = smram_router(); enable(&mut router);
            assign(&mut router, 1, BAR_LOW.base, false);
            *counts.lock().unwrap() = Counts::default();
            let mut calls = 0;
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                router.mmio_write_with_smram(BASE + 0x50, &[255, 255], &mut |mut s| {
                    calls += 1;
                    match fault {
                        0 => Err(PciConfigError::SmramBackend),
                        1 => { s.default_smbase_locked = true; Ok(s) }
                        _ => panic!("injected apply panic"),
                    }
                })
            }));
            if fault == 2 { assert!(outcome.is_err()); }
            else { assert_eq!(outcome.unwrap(), Err(MchError::Config(PciConfigError::SmramBackend))); }
            assert_eq!(calls, 1);
            for address in [BASE, BASE + 0x50, BASE + SLOT1, BAR_LOW.base, 0x1000, 0x2000, u64::MAX] {
                let mut data = [0xa5; 4];
                assert_eq!(router.mmio_read(address, &mut data), Err(MchError::Config(PciConfigError::SmramPoisoned)));
                assert_eq!(data, [0xa5; 4]);
                assert_eq!(router.mmio_write(address, &[0; 4]), Err(MchError::Config(PciConfigError::SmramPoisoned)));
                assert!(router.mmio_write_with_smram(address, &[0; 4], &mut |_| panic!("reapplied poison")).is_err());
            }
            for port in [0xcf8, 0xcfc, 0xcfd, 0] {
                let mut data = [0xa5; 4];
                assert_eq!(router.pio_read(port, &mut data), Err(MchError::Config(PciConfigError::SmramPoisoned)));
                assert_eq!(data, [0xa5; 4]);
                assert!(router.pio_write(port, &[0; 4]).is_err());
                assert!(router.pio_write_with_smram(port, &[0; 4], &mut |_| panic!("reapplied poison")).is_err());
            }
            let counts = counts.lock().unwrap();
            assert_eq!((counts.reads, counts.writes, counts.fixed, counts.bar_reads, counts.bar_writes), (0, 0, 0, 0, 0));
        }
    }

    #[test]
    fn smram_constructor_requires_both_private_ranges_excluded_from_every_route() {
        let empty = GuestMemory::default();
        for inventory in [vec![], private_ranges()[1..].to_vec(), vec![private_ranges()[0]],
            vec![BusRange { base: 0x30000, len: 0x10000 }, private_ranges()[1]]] {
            assert!(MchPci::with_q35_smram(vec![], Bus::default(), &empty, APERTURE, vec![], inventory).is_err());
        }
        for private in &private_ranges()[..2] {
            let ram = GuestMemory::new(&[(GuestAddress(private.base), page_size() as u64)]).unwrap();
            assert!(MchPci::with_q35_smram(vec![], Bus::default(), &ram, APERTURE, vec![], private_ranges()).is_err());
            let mut fixed = BusBuilder::new();
            fixed.insert(private.base, 4, Arc::new(Mutex::new(Fixed(Arc::default())))).unwrap();
            assert!(MchPci::with_q35_smram(vec![], fixed.build(), &empty, APERTURE, vec![], private_ranges()).is_err());
            assert!(MchPci::with_q35_smram(vec![], Bus::default(), &empty, APERTURE, vec![*private], private_ranges()).is_err());
            let ecam = BusRange { base: private.base & !(APERTURE_SIZE - 1), len: APERTURE_SIZE };
            assert!(MchPci::with_q35_smram(vec![], Bus::default(), &empty, ecam, vec![], private_ranges()).is_err());
        }
    }
    fn bar_router(devices: Vec<(u8, SharedPciDevice)>) -> MchPci {
        MchPci::with_bar_windows(devices, Bus::default(), &GuestMemory::default(), APERTURE,
            vec![BAR_LOW, BAR_HIGH], private_ranges()).unwrap()
    }
    fn assign(router: &mut MchPci, slot: u8, base: u64, is64: bool) {
        config_write(router, slot, 4, &0u16.to_le_bytes()).unwrap();
        config_write(router, slot, 0x10, &(base as u32).to_le_bytes()).unwrap();
        if is64 { config_write(router, slot, 0x14, &((base >> 32) as u32).to_le_bytes()).unwrap(); }
        config_write(router, slot, 4, &2u16.to_le_bytes()).unwrap();
    }

    #[test]
    fn checked_bar_route_reaches_actual_callback_and_tracks_ecam_rebase() {
        let (device, counts) = endpoint();
        let mut router = bar_router(vec![(1, device)]); enable(&mut router);
        assign(&mut router, 1, BAR_LOW.base, false);
        let mut data = [0; 4];
        assert_eq!(router.mmio_read(BAR_LOW.base + 4, &mut data), Ok(true));
        assert_eq!(u32::from_le_bytes(data), 0x1122_3348);
        assert_eq!(counts.lock().unwrap().last_bar, Some((0, 4)));
        router.mmio_write(BAR_LOW.base + 8, &[1, 2, 3, 4]).unwrap();
        assert_eq!(counts.lock().unwrap().last_bar, Some((0, 8)));
        router.mmio_write(BASE + SLOT1 + 0x10, &((BAR_LOW.base + 0x2000) as u32).to_le_bytes()).unwrap();
        data.fill(0xa5);
        assert_eq!(router.mmio_read(BAR_LOW.base, &mut data), Ok(false));
        assert_eq!(data, [0xa5; 4]);
        assert_eq!(router.mmio_read(BAR_LOW.base + 0x2000, &mut data), Ok(true));
        assert_eq!(counts.lock().unwrap().last_bar, Some((0, 0)));
    }

    #[test]
    fn bar64_probes_unassigned_and_mem_disabled_do_not_decode() {
        let (device, counts) = endpoint();
        device.lock().unwrap().config_mut().add_bar(0, BarSpec { size: 0x4000, is64: true, prefetch: false });
        let mut router = bar_router(vec![(1, device)]);
        let mut data = [0xa5; 4];
        config_write(&mut router, 1, 4, &2u16.to_le_bytes()).unwrap();
        assert_eq!(router.mmio_read(BAR_HIGH.base, &mut data), Ok(false)); // Unassigned.
        config_write(&mut router, 1, 4, &0u16.to_le_bytes()).unwrap();
        config_write(&mut router, 1, 0x10, &u32::MAX.to_le_bytes()).unwrap();
        config_write(&mut router, 1, 0x14, &u32::MAX.to_le_bytes()).unwrap();
        select(&mut router, 1, 0x10); router.pio_read(0xcfc, &mut data).unwrap();
        assert_eq!(u32::from_le_bytes(data), 0xffff_c004);
        data.fill(0xa5);
        assert_eq!(router.mmio_read(BAR_HIGH.base, &mut data), Ok(false));
        assert_eq!(data, [0xa5; 4]);
        assign(&mut router, 1, BAR_HIGH.base, true);
        assert_eq!(router.mmio_read(BAR_HIGH.base, &mut data), Ok(true));
        assert_eq!(counts.lock().unwrap().bar_reads, 1);
    }

    #[test]
    fn entire_enabled_map_is_checked_and_bad_config_is_repairable() {
        let (first, first_counts) = endpoint(); let (second, second_counts) = endpoint();
        let mut router = bar_router(vec![(2, second), (1, first)]); enable(&mut router);
        assign(&mut router, 1, BAR_LOW.base, false);
        assign(&mut router, 2, BAR_LOW.base + 0x2000, false);
        let before = second_counts.lock().unwrap().writes;
        // This callback is accepted and its side effect is NOT rolled back.
        config_write(&mut router, 2, 0x10, &(BASE as u32).to_le_bytes()).unwrap();
        assert_eq!(second_counts.lock().unwrap().writes, before + 1);
        let mut data = [0xa5; 4];
        assert_eq!(router.mmio_read(BAR_LOW.base, &mut data), Err(MchError::BarMap));
        assert_eq!(router.mmio_write(BAR_LOW.base, &[0; 4]), Err(MchError::BarMap));
        assert_eq!(data, [0xa5; 4]);
        assert_eq!(first_counts.lock().unwrap().bar_reads, 0);
        assert_eq!(first_counts.lock().unwrap().bar_writes, 0);
        select(&mut router, 2, 0x10); router.pio_read(0xcfc, &mut data).unwrap();
        assert_eq!(u32::from_le_bytes(data), BASE as u32);
        // ECAM still works and repairs the map; no old decode was retained.
        router.mmio_write(BASE + (2 << 15) + 0x10, &((BAR_LOW.base + 0x2000) as u32).to_le_bytes()).unwrap();
        assert_eq!(router.mmio_read(BAR_LOW.base, &mut data), Ok(true));
        assert_eq!(router.mmio_read(BAR_LOW.base + 0x2000, &mut data), Ok(true));
    }

    #[test]
    fn overlapping_bars_have_no_order_dependent_winner_but_adjacency_works() {
        for reverse in [false, true] {
            let (first, first_counts) = endpoint(); let (second, second_counts) = endpoint();
            let mut devices = vec![(1, first), (2, second)]; if reverse { devices.reverse(); }
            let mut router = bar_router(devices);
            assign(&mut router, 1, BAR_LOW.base, false); assign(&mut router, 2, BAR_LOW.base, false);
            let mut data = [0xa5; 4];
            assert_eq!(router.mmio_read(BAR_LOW.base, &mut data), Err(MchError::BarMap));
            assert_eq!(data, [0xa5; 4]);
            assert_eq!(first_counts.lock().unwrap().bar_reads + second_counts.lock().unwrap().bar_reads, 0);
            config_write(&mut router, 2, 0x10, &((BAR_LOW.base + 0x1000) as u32).to_le_bytes()).unwrap();
            assert_eq!(router.mmio_read(BAR_LOW.base, &mut data), Ok(true));
            assert_eq!(router.mmio_read(BAR_LOW.base + 0x1000, &mut data), Ok(true));
            // Internal checked route validates full BAR spans too; outer aligned
            // scalar accesses normally make this particular crossing impossible.
            assert_eq!(router.root.checked_bar_mmio(BAR_LOW.base + 0xffc, &mut [0xa5; 8], None,
                &router.bar_windows, &router.bar_exclusions), Err(MchError::Boundary));
        }
    }

    #[test]
    fn multiple_bars_keep_their_index_and_intra_endpoint_overlap_is_rejected() {
        let (device, counts) = endpoint();
        device.lock().unwrap().config_mut().add_bar(2, BarSpec { size: 4096, is64: false, prefetch: false });
        let mut router = bar_router(vec![(1, device)]);
        assign(&mut router, 1, BAR_LOW.base, false);
        config_write(&mut router, 1, 0x18, &((BAR_LOW.base + 0x1000) as u32).to_le_bytes()).unwrap();
        assert_eq!(router.mmio_write(BAR_LOW.base + 0x1008, &[0; 4]), Ok(true));
        assert_eq!(counts.lock().unwrap().last_bar, Some((2, 8)));
        config_write(&mut router, 1, 0x18, &(BAR_LOW.base as u32).to_le_bytes()).unwrap();
        let mut data = [0xa5; 4];
        assert_eq!(router.mmio_read(BAR_LOW.base, &mut data), Err(MchError::BarMap));
        assert_eq!(data, [0xa5; 4]);
        assert_eq!(counts.lock().unwrap().bar_reads, 0);
    }

    #[test]
    fn bar_layout_rejects_missing_private_inventory_and_every_collision_class() {
        let create = |windows, reserved| MchPci::with_bar_windows(vec![], Bus::default(),
            &GuestMemory::default(), APERTURE, windows, reserved);
        assert!(matches!(create(vec![BAR_LOW], vec![]), Err(MchError::BarLayout)));
        assert!(matches!(create(vec![BAR_LOW; 3], private_ranges()), Err(MchError::BarLayout)));
        assert!(matches!(create(vec![BAR_LOW], vec![BusRange { base: 0, len: 1 }; 65]), Err(MchError::BarLayout)));
        for window in [APERTURE, BusRange { base: BAR_LOW.base + 1, len: 4096 },
                       BusRange { base: BAR_LOW.base, len: 0 }, BusRange { base: ADDRESS_LIMIT, len: 4096 },
                       BusRange { base: u64::MAX - 4095, len: 4096 }] {
            assert!(matches!(create(vec![window], private_ranges()), Err(MchError::BarLayout)));
        }
        assert!(matches!(create(vec![BAR_LOW, BAR_LOW], private_ranges()), Err(MchError::BarLayout)));
        assert!(create(vec![BusRange { base: BAR_LOW.base, len: 4096 },
                           BusRange { base: BAR_LOW.base + 4096, len: 4096 }], private_ranges()).is_ok());
        for reserved in private_ranges() {
            assert!(matches!(create(vec![reserved], private_ranges()), Err(MchError::BarLayout)));
        }
        for bad in [BusRange { base: 0, len: 0 }, BusRange { base: u64::MAX, len: 2 },
                    BusRange { base: ADDRESS_LIMIT, len: 1 }] {
            assert!(matches!(create(vec![BAR_LOW], vec![bad]), Err(MchError::BarLayout)));
        }
        let ram = GuestMemory::new(&[(GuestAddress(BAR_LOW.base), page_size() as u64)]).unwrap();
        assert!(matches!(MchPci::with_bar_windows(vec![], Bus::default(), &ram, APERTURE,
            vec![BAR_LOW], private_ranges()), Err(MchError::BarLayout)));
        let mut fixed = BusBuilder::new();
        fixed.insert(BAR_LOW.base, 4, Arc::new(Mutex::new(Fixed(Arc::default())))).unwrap();
        assert!(matches!(MchPci::with_bar_windows(vec![], fixed.build(), &GuestMemory::default(), APERTURE,
            vec![BAR_LOW], private_ranges()), Err(MchError::BarLayout)));
    }

    #[test]
    fn invalid_enabled_bar_metadata_fails_before_callbacks() {
        let (device, counts) = endpoint(); let mut router = bar_router(vec![(1, device.clone())]);
        assign(&mut router, 1, BAR_LOW.base, false);
        let pristine = device.lock().unwrap().config().clone();
        let mut malformed = Vec::new();
        for (size, is64) in [(0u64, false), (6144, false), (0x2_0000_0000, false)] {
            let mut value = serde_json::to_value(&pristine).unwrap();
            value["bars"][0]["size"] = size.into(); value["bars"][0]["is64"] = is64.into();
            malformed.push(serde_json::from_value::<ConfigSpace>(value).unwrap());
        }
        let mut unaligned = pristine.clone(); unaligned.raw_mut()[0x10..0x14].copy_from_slice(&0x4000_0010u32.to_le_bytes());
        malformed.push(unaligned);
        let mut overflow = pristine.clone(); overflow.add_bar(0, BarSpec { size: 4096, is64: true, prefetch: false });
        overflow.write(0x10, &u32::MAX.to_le_bytes()); overflow.write(0x14, &u32::MAX.to_le_bytes()); malformed.push(overflow);
        let mut pair = pristine.clone(); pair.add_bar(0, BarSpec { size: 4096, is64: true, prefetch: false });
        pair.add_bar(1, BarSpec { size: 4096, is64: false, prefetch: false }); malformed.push(pair);
        for config in malformed {
            *device.lock().unwrap().config_mut() = config;
            let mut data = [0xa5; 4];
            assert_eq!(router.mmio_read(BAR_LOW.base, &mut data), Err(MchError::BarMap));
            assert_eq!(data, [0xa5; 4]);
        }
        assert_eq!(counts.lock().unwrap().bar_reads, 0);
        *device.lock().unwrap().config_mut() = pristine;
        assert_eq!(router.mmio_read(BAR_LOW.base, &mut [0; 4]), Ok(true));
    }

    #[test]
    fn endpoint_alias_busy_and_poison_are_fail_closed() {
        let (device, counts) = endpoint();
        assert!(matches!(MchPci::with_bar_windows(vec![(1, device.clone()), (2, device.clone())], Bus::default(),
            &GuestMemory::default(), APERTURE, vec![BAR_LOW], private_ranges()), Err(MchError::EndpointAlias)));
        let mut router = bar_router(vec![(1, device.clone())]); assign(&mut router, 1, BAR_LOW.base, false);
        let guard = device.lock().unwrap(); let mut data = [0xa5; 4];
        assert_eq!(router.mmio_read(BAR_LOW.base, &mut data), Err(MchError::BarBusy));
        assert_eq!(data, [0xa5; 4]); drop(guard);
        let poison = device.clone();
        assert!(std::thread::spawn(move || { let _guard = poison.lock().unwrap(); panic!("intentional poison fixture"); }).join().is_err());
        assert_eq!(router.mmio_read(BAR_LOW.base, &mut data), Err(MchError::BarPoisoned));
        assert_eq!(data, [0xa5; 4]); assert_eq!(counts.lock().unwrap().bar_reads, 0);
    }

    #[test]
    fn all_endpoint_guards_survive_until_the_selected_callback_finishes() {
        use std::sync::mpsc;
        use std::time::Duration;
        struct Blocking {
            config: ConfigSpace, entered: mpsc::Sender<()>, release: mpsc::Receiver<()>,
        }
        impl PciDevice for Blocking {
            fn config(&self) -> &ConfigSpace { &self.config }
            fn config_mut(&mut self) -> &mut ConfigSpace { &mut self.config }
            fn bar_read(&mut self, _: usize, _: u64, data: &mut [u8]) {
                self.entered.send(()).unwrap(); self.release.recv_timeout(Duration::from_secs(2)).unwrap(); data.fill(0x55);
            }
            fn bar_write(&mut self, _: usize, _: u64, _: &[u8]) { panic!("no write") }
            fn id(&self) -> String { "blocking".into() }
            fn save(&mut self) -> vroom_snapshot::Result<Vec<u8>> { panic!("cold only") }
            fn restore(&mut self, _: &[u8]) -> vroom_snapshot::Result<()> { panic!("cold only") }
        }
        let (other, _) = endpoint(); let config = other.lock().unwrap().config().clone();
        let (entered_tx, entered_rx) = mpsc::channel(); let (release_tx, release_rx) = mpsc::channel();
        let first: SharedPciDevice = Arc::new(Mutex::new(Blocking { config, entered: entered_tx, release: release_rx }));
        let mut router = bar_router(vec![(1, first), (2, other.clone())]);
        assign(&mut router, 1, BAR_LOW.base, false); assign(&mut router, 2, BAR_LOW.base + 0x2000, false);
        let reader = std::thread::spawn(move || {
            let mut data = [0; 4]; assert_eq!(router.mmio_read(BAR_LOW.base, &mut data), Ok(true));
            assert_eq!(data, [0x55; 4]); router
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(other.try_lock(), Err(std::sync::TryLockError::WouldBlock)));
        let (started_tx, started_rx) = mpsc::channel(); let (changed_tx, changed_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            other.lock().unwrap().config_mut().write(0x10, &(BASE as u32).to_le_bytes());
            changed_tx.send(()).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(changed_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        release_tx.send(()).unwrap();
        let mut router = reader.join().unwrap(); writer.join().unwrap();
        changed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(router.mmio_read(BAR_LOW.base, &mut [0; 4]), Err(MchError::BarMap));
    }

    #[test]
    fn real_nvme_cap_and_version_are_reachable_without_disks_or_queues() {
        struct NoMsi;
        impl crate::pci::MsiSender for NoMsi { fn send(&self, _: u64, _: u32) { panic!("no interrupt expected") } }
        let irq = crate::pci::IntxRouter::new((0..4).map(|_| Arc::new(crate::NoIrq) as Arc<dyn crate::IrqLine>).collect());
        let nvme = crate::nvme::Nvme::new(1, vec![], GuestMemory::default(), irq.pin(1, 1), Arc::new(NoMsi)).into_shared();
        let mut router = bar_router(vec![(1, nvme)]); assign(&mut router, 1, BAR_HIGH.base, true);
        let mut cap = [0; 8]; let mut version = [0; 4];
        assert_eq!(router.mmio_read(BAR_HIGH.base, &mut cap), Ok(true));
        assert_ne!(u64::from_le_bytes(cap) & (1 << 37), 0);
        assert_eq!(router.mmio_read(BAR_HIGH.base + 8, &mut version), Ok(true));
        assert_eq!(u32::from_le_bytes(version), 0x0001_0400);
        // No CC enable/doorbell write, namespace, disk, worker, guest or VM.
    }
}

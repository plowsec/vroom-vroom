//! Cold-only Q35 MCH configuration and checked ECAM routing.
//!
//! PCIEXBAR changes real absolute-GPA decoding inside one reserved aperture.
//! This is not a Q35 board: BAR-window MMIO, PAM/SMRAM/TSEG, reset and snapshots
//! are deliberately not implemented here. In particular, no register claims a
//! memory-protection effect that has not happened in the hypervisor.
//!
//! The exclusive router owns every route to its private MCH state. A future
//! SMRAM write must prepare state, apply a fallible mapping transition on the
//! exiting vCPU, then commit readback; it must not use infallible BusDevice.
//!
//! Register definitions and the pinned firmware's disable/high/enable sequence:
//! https://github.com/qemu/qemu/blob/v11.1.2/include/hw/pci-host/q35.h
//! https://github.com/tianocore/edk2/blob/4dfdca63a93497203f197ec98ba20e2327e4afe4/OvmfPkg/Library/PlatformInitLib/Platform.c

use std::sync::Arc;
use vroom_memory::GuestMemory;
use crate::{Bus, BusRange};
use super::{ConfigSpace, PciCf8, PciCf8Error, PciId, PciRoot, SharedPciDevice};

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MchError {
    #[error("invalid or overlapping reserved ECAM aperture")]
    Layout,
    #[error("duplicate or invalid PCI endpoint slot")]
    Slot,
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
}

impl MchConfig {
    fn new(aperture: BusRange) -> Self {
        let mut config = ConfigSpace::new(PciId { vendor: 0x8086, device: 0x29c0,
            class: 0x06_00_00, revision: 0, subsys_vendor: 0, subsys: 0 }, 0);
        config.raw_mut()[PCIEXBAR..PCIEXBAR + 8].copy_from_slice(&0xb000_0000u64.to_le_bytes());
        Self { config, aperture }
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

    pub(super) fn access(&mut self, off: usize, data: &mut [u8], write: Option<&[u8]>) -> Result<(), PciConfigError> {
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
        if off < PCIEXBAR || finish > PCIEXBAR + 8 { return Err(PciConfigError::UnsupportedRegister); }
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
/// stays reserved when disabled/shrunk. BAR MMIO is a separate future route.
/// The caller must also exclude the aperture from hidden HV RAM/ROM mappings;
/// this device layer can verify only supplied GuestMemory and the fixed bus.
#[derive(Debug)]
pub struct MchPci {
    root: Arc<PciRoot>,
    cf8: PciCf8,
    fixed: Bus,
    aperture: BusRange,
}

impl MchPci {
    pub fn new(devices: Vec<(u8, SharedPciDevice)>, fixed_mmio: Bus, ram: &GuestMemory,
               ecam_aperture: BusRange) -> Result<Self, MchError> {
        let finish = end(ecam_aperture).ok_or(MchError::Layout)?;
        if ecam_aperture.len != APERTURE_SIZE || ecam_aperture.base % APERTURE_SIZE != 0
            || finish > ADDRESS_LIMIT
            || fixed_mmio.ranges().any(|r| overlaps(r, ecam_aperture.base, finish))
            || ram.regions().any(|r| overlaps(BusRange { base: r.start().0, len: r.len() }, ecam_aperture.base, finish)) {
            return Err(MchError::Layout);
        }
        let mut slots = 0u32;
        for (slot, _) in &devices {
            if !(1..=31).contains(slot) || slots & (1u32 << slot) != 0 { return Err(MchError::Slot); }
            slots |= 1u32 << slot;
        }
        let root = PciRoot::with_mch(devices, MchConfig::new(ecam_aperture));
        let cf8 = PciCf8::new(root.clone());
        Ok(Self { root, cf8, fixed: fixed_mmio, aperture: ecam_aperture })
    }

    pub fn pio_read(&mut self, port: u16, data: &mut [u8]) -> Result<(), MchError> {
        Ok(self.cf8.read(port, data)?)
    }

    pub fn pio_write(&mut self, port: u16, data: &[u8]) -> Result<(), MchError> {
        Ok(self.cf8.write(port, data)?)
    }

    /// Absolute-GPA access. False is unmapped; errors and unmapped reads leave
    /// the buffer unchanged. Every full span is checked before any callback.
    pub fn mmio_read(&mut self, address: u64, data: &mut [u8]) -> Result<bool, MchError> {
        self.mmio(address, data, None)
    }

    pub fn mmio_write(&mut self, address: u64, data: &[u8]) -> Result<bool, MchError> {
        self.mmio(address, &mut [], Some(data))
    }

    fn mmio(&mut self, address: u64, data: &mut [u8], write: Option<&[u8]>) -> Result<bool, MchError> {
        let len = write.map_or(data.len(), |bytes| bytes.len());
        if !matches!(len, 1 | 2 | 4 | 8) { return Err(MchError::Width); }
        if address % len as u64 != 0 { return Err(MchError::Alignment); }
        let finish = address.checked_add(len as u64).ok_or(MchError::Boundary)?;
        if overlaps(self.aperture, address, finish) {
            if !contains(self.aperture, address, finish) { return Err(MchError::Boundary); }
            let Some(window) = self.root.mch_window() else { return Ok(false) };
            if !overlaps(window, address, finish) { return Ok(false); }
            if !contains(window, address, finish) { return Err(MchError::Boundary); }
            self.root.config_access(address - window.base, data, write)?;
            return Ok(true);
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

    #[derive(Default, Debug)]
    struct Counts { reads: usize, writes: usize, saves: usize, restores: usize, fixed: usize }
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
        fn bar_read(&mut self, _: usize, _: u64, _: &mut [u8]) { panic!("BAR MMIO is not wired") }
        fn bar_write(&mut self, _: usize, _: u64, _: &[u8]) { panic!("BAR MMIO is not wired") }
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
}

//! Restricted SMM memory ownership and mapping transaction, without ioctls.
//! KVM AS0 is normal mode, AS1 is SMM. The only private range in this first
//! implementation is the default 64 KiB SMBASE window. No open/close/remap.
#![cfg_attr(not(all(target_os = "linux", target_arch = "x86_64")), allow(dead_code))]

use std::sync::Arc;
use vroom_memory::{GuestMemory, MmapRegion};
use crate::{Error, Result};

pub(crate) const BASE: u64 = 0x30000;
pub(crate) const SIZE: usize = 0x10000;

#[derive(Default)]
pub(crate) struct Setup {
    pub requested: bool,
    pub mapping_attempted: bool,
    pub vcpu_attempted: bool,
}

impl Setup {
    pub fn begin(&mut self, finalized: bool) -> Result<()> {
        if self.requested || self.vcpu_attempted || self.mapping_attempted || finalized {
            return Err(invalid("configure SMM once on an unmapped VM before vCPU creation/finalize"));
        }
        // Capability, allocation and first registration failures all consume it.
        self.requested = true;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Mapping {
    pub slot: u32,
    pub gpa: u64,
    pub host: u64,
    pub size: u64,
    pub readonly: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase { Unmapped, Open, Sealed, Poisoned }

// Owners are deliberately separate from mapping metadata and never exposed.
#[allow(dead_code)] // Deliberately retained for Drop, never exposed to devices.
enum Owner { Ram(GuestMemory), Rom(Arc<MmapRegion>) }

pub(crate) struct Memory {
    private: MmapRegion,
    common: Vec<Owner>,
    mappings: Vec<Mapping>,
    phase: Phase,
    next: u32,
    limit: u32,
}

fn invalid(message: &str) -> Error { Error::Invalid(message.into()) }

fn overlaps(a: u64, size: u64, b: u64, other: u64) -> Result<bool> {
    let end = a.checked_add(size).ok_or_else(|| invalid("SMM mapping range overflow"))?;
    let other_end = b.checked_add(other).ok_or_else(|| invalid("SMM mapping range overflow"))?;
    Ok(a < other_end && b < end)
}

fn check_common(mapping: Mapping, private_host: u64) -> Result<()> {
    crate::x86_64::check_memory_range(mapping.gpa, mapping.size)?;
    if mapping.host == 0 || mapping.host & 0xfff != 0 {
        return Err(invalid("SMM common backing must be page aligned"));
    }
    if overlaps(mapping.gpa, mapping.size, BASE, SIZE as u64)?
        || overlaps(mapping.host, mapping.size, private_host, SIZE as u64)? {
        return Err(invalid("common memory aliases private SMRAM"));
    }
    Ok(())
}

impl Memory {
    /// Construct before the first ioctl; caller stores this owner first.
    pub fn new(initial: &[u8], max_slots: usize) -> Result<Self> {
        if initial.len() != SIZE || max_slots < 2 {
            return Err(invalid("SMM needs exactly 64 KiB and at least two memory slots"));
        }
        let private = MmapRegion::new(SIZE).map_err(|e| invalid(&format!("allocate SMRAM: {e}")))?;
        private.load(initial).map_err(|e| invalid(&format!("initialize SMRAM: {e}")))?;
        Ok(Self { private, common: Vec::new(), mappings: Vec::new(),
            phase: Phase::Unmapped, next: 1, limit: max_slots.min(1 << 16) as u32 })
    }

    pub fn install_private(&mut self, mut install: impl FnMut(Mapping) -> Result<()>) -> Result<()> {
        if self.phase != Phase::Unmapped { return Err(invalid("SMM configuration already attempted")); }
        // A failed ioctl never drops a possibly mapped owner or enables a CPU.
        self.phase = Phase::Poisoned;
        let mapping = Mapping { slot: 1 << 16, gpa: BASE, host: self.private.as_ptr() as u64,
            size: SIZE as u64, readonly: false };
        install(mapping)?;
        self.mappings.push(mapping);
        self.phase = Phase::Open;
        Ok(())
    }

    fn preflight(&self, requested: &[Mapping]) -> Result<()> {
        if self.phase != Phase::Open { return Err(invalid("SMM topology is frozen or poisoned")); }
        if requested.is_empty() { return Err(invalid("empty SMM common mapping")); }
        if self.next as usize + requested.len() > self.limit as usize {
            return Err(invalid("SMM memory-slot limit exceeded"));
        }
        for (index, &mapping) in requested.iter().enumerate() {
            check_common(mapping, self.private.as_ptr() as u64)?;
            for prior in self.mappings.iter().chain(requested[..index].iter()) {
                if overlaps(mapping.gpa, mapping.size, prior.gpa, prior.size)? {
                    return Err(invalid("overlapping SMM common guest mappings"));
                }
            }
        }
        Ok(())
    }

    fn install_common(&mut self, requested: Vec<Mapping>, owner: Owner,
        mut install: impl FnMut(Mapping) -> Result<()>) -> Result<()> {
        self.preflight(&requested)?;
        // Retain ALL regions of the owner before the first AS0/AS1 ioctl.
        self.common.push(owner);
        self.phase = Phase::Poisoned;
        for mut mapping in requested {
            mapping.slot = self.next;
            install(mapping)?;
            self.mappings.push(mapping);
            mapping.slot |= 1 << 16;
            install(mapping)?;
            self.mappings.push(mapping);
            self.next += 1;
        }
        self.phase = Phase::Open;
        Ok(())
    }

    pub fn map_memory(&mut self, memory: &GuestMemory, install: impl FnMut(Mapping) -> Result<()>) -> Result<()> {
        let requested = memory.regions().map(|r| Mapping { slot: 0, gpa: r.start().raw(),
            host: r.host_ptr() as u64, size: r.len(), readonly: false }).collect();
        self.install_common(requested, Owner::Ram(memory.clone()), install)
    }

    pub fn map_readonly(&mut self, gpa: u64, image: Arc<MmapRegion>,
        install: impl FnMut(Mapping) -> Result<()>) -> Result<()> {
        let mapping = Mapping { slot: 0, gpa, host: image.as_ptr() as u64,
            size: image.size() as u64, readonly: true };
        self.install_common(vec![mapping], Owner::Rom(image), install)
    }

    /// Called under the VM mapping lock BEFORE even attempting CREATE_VCPU.
    pub fn seal(&mut self) -> Result<()> {
        if self.phase != Phase::Open { return Err(invalid("SMM topology is frozen or poisoned")); }
        self.phase = Phase::Sealed;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vroom_memory::GuestAddress;

    fn open() -> Memory {
        let mut m = Memory::new(&vec![0xa5; SIZE], 8).unwrap();
        m.install_private(|_| Ok(())).unwrap(); m
    }

    #[test]
    fn configuration_requires_no_prior_map_cpu_or_configuration_attempt() {
        for mut setup in [Setup { mapping_attempted: true, ..Default::default() },
            Setup { vcpu_attempted: true, ..Default::default() },
            Setup { requested: true, ..Default::default() }] {
            assert!(setup.begin(false).is_err());
        }
        assert!(Setup::default().begin(true).is_err());
        let mut setup = Setup::default();
        setup.begin(false).unwrap();
        assert!(setup.requested);
        // A subsequent failed capability/allocation check leaves this latched.
        assert!(setup.begin(false).is_err());
    }

    #[test]
    fn private_slot_only_in_as1_and_common_ram_rom_are_mirrored() {
        let mut m = Memory::new(&vec![0xa5; SIZE], 8).unwrap();
        let mut calls = Vec::new();
        m.install_private(|slot| { calls.push(slot); Ok(()) }).unwrap();
        let mem = GuestMemory::new(&[(GuestAddress(0x10000), 0x10000)]).unwrap();
        let rom = Arc::new(MmapRegion::new(0x10000).unwrap());
        m.map_memory(&mem, |slot| { calls.push(slot); Ok(()) }).unwrap();
        m.map_readonly(0xffff0000, rom, |slot| { calls.push(slot); Ok(()) }).unwrap();
        assert_eq!(calls.iter().map(|s| s.slot).collect::<Vec<_>>(), [0x10000, 1, 0x10001, 2, 0x10002]);
        assert_eq!((calls[0].gpa, calls[0].size), (BASE, SIZE as u64));
        for pair in [&calls[1..3], &calls[3..5]] {
            assert_eq!((pair[0].gpa, pair[0].host, pair[0].size, pair[0].readonly),
                (pair[1].gpa, pair[1].host, pair[1].size, pair[1].readonly));
        }
        assert!(calls[3].readonly);
        // Normal device DMA has neither a GPA range nor a host pointer to SMRAM.
        assert!(!mem.check_range(GuestAddress(BASE), SIZE as u64));
        assert!(mem.host_ranges(GuestAddress(BASE), SIZE as u64).is_none());
        assert!(mem.write(GuestAddress(BASE), &[1]).is_err());
        assert!(calls[1].host + calls[1].size <= calls[0].host || calls[0].host + calls[0].size <= calls[1].host);
    }

    #[test]
    fn overlap_overflow_and_alias_checks_precede_all_ioctls() {
        let private = 0x100000;
        let good = Mapping { slot: 0, gpa: 0x10000, host: 0x200000, size: 0x10000, readonly: false };
        for mapping in [Mapping { gpa: BASE, ..good }, Mapping { gpa: BASE - 0x1000, ..good },
            Mapping { host: private, ..good }, Mapping { host: private - 0x1000, ..good },
            Mapping { gpa: u64::MAX - 0xfff, ..good }, Mapping { host: u64::MAX - 0xfff, ..good },
            Mapping { size: 0, ..good }, Mapping { host: 1, ..good }] {
            assert!(check_common(mapping, private).is_err(), "{mapping:?}");
        }
        assert!(check_common(good, private).is_ok());
        let mut m = open();
        let overlap = GuestMemory::new(&[(GuestAddress(BASE - 0x4000), 0x8000)]).unwrap();
        let mut calls = 0;
        assert!(m.map_memory(&overlap, |_| { calls += 1; Ok(()) }).is_err());
        assert_eq!(calls, 0);
        assert!(m.common.is_empty());
        assert_eq!(m.phase, Phase::Open);
    }

    #[test]
    fn every_partial_failure_retains_owners_and_permanently_blocks_cpu_creation() {
        for fail_at in 0..4 {
            let mut m = open();
            let mem = GuestMemory::new(&[(GuestAddress(0x10000), 0x10000), (GuestAddress(0x50000), 0x10000)]).unwrap();
            mem.write(GuestAddress(0x10000), &[0x39]).unwrap();
            let mut calls = 0;
            assert!(m.map_memory(&mem, |_| {
                let at = calls; calls += 1;
                if at == fail_at { Err(invalid("injected mapping failure")) } else { Ok(()) }
            }).is_err());
            drop(mem);
            assert_eq!(calls, fail_at + 1);
            assert_eq!(m.phase, Phase::Poisoned);
            let Owner::Ram(retained) = &m.common[0] else { panic!("RAM owner missing") };
            assert_eq!(retained.read_obj::<u8>(GuestAddress(0x10000)).unwrap(), 0x39);
            let retained = retained.clone();
            assert!(m.seal().is_err());
            assert!(m.map_memory(&retained, |_| panic!("retry performed an ioctl")).is_err());
        }
    }

    #[test]
    fn private_and_rom_failure_owners_survive_and_seal_precedes_vcpu_attempt() {
        let mut m = Memory::new(&vec![0xa5; SIZE], 8).unwrap();
        assert!(m.install_private(|_| Err(invalid("injected private failure"))).is_err());
        assert_eq!(m.private.size(), SIZE);
        assert!(m.seal().is_err());
        assert!(m.install_private(|_| panic!("retry")).is_err());
        let mut m = open();
        let rom = Arc::new(MmapRegion::new(0x10000).unwrap());
        let weak = Arc::downgrade(&rom);
        assert!(m.map_readonly(0xffff0000, rom, |_| Err(invalid("injected ROM failure"))).is_err());
        assert!(weak.upgrade().is_some());
        let Owner::Rom(owner) = &m.common[0] else { panic!("ROM owner missing") };
        assert_eq!(owner.size(), 0x10000);
        assert!(m.seal().is_err());
        drop(m);
        assert!(weak.upgrade().is_none());
        let mut m = open();
        m.seal().unwrap(); // even if the following CREATE_VCPU fails
        assert!(m.seal().is_err());
        let mem = GuestMemory::new(&[(GuestAddress(0x10000), 0x10000)]).unwrap();
        assert!(m.map_memory(&mem, |_| panic!("map after vCPU attempt")).is_err());
    }

    #[test]
    fn slot_limits_and_duplicate_common_ranges_fail_before_mutation() {
        assert!(Memory::new(&[], 8).is_err());
        assert!(Memory::new(&vec![0; SIZE], 1).is_err());
        let mut m = Memory::new(&vec![0; SIZE], 2).unwrap();
        m.install_private(|_| Ok(())).unwrap();
        let mem = GuestMemory::new(&[(GuestAddress(0x10000), 0x10000)]).unwrap();
        m.map_memory(&mem, |_| Ok(())).unwrap();
        assert!(m.map_memory(&mem, |_| panic!("overlap/limit ioctl")).is_err());
        assert_eq!(m.common.len(), 1);
        m.seal().unwrap();
        let mut m = open();
        m.map_memory(&mem, |_| Ok(())).unwrap();
        assert!(m.map_memory(&mem, |_| panic!("overlap ioctl")).is_err());
        assert_eq!(m.common.len(), 1);
    }
}

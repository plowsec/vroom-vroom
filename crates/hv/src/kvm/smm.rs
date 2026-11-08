//! Restricted SMM memory ownership and mapping transaction, without ioctls.
//! KVM AS0 is normal mode, AS1 is SMM. Static 64 KiB remains AS1-only. The
//! separate fixed Q35 profile owns 128 KiB default SMBASE and 8 MiB TSEG, with
//! initially open CPU-only AS0 aliases and irreversible close/lock transitions.
//! No private backing is ever supplied to GuestMemory or a device DMA path.
#![cfg_attr(not(all(target_os = "linux", target_arch = "x86_64")), allow(dead_code))]

use std::sync::Arc;
use vroom_memory::{GuestMemory, MmapRegion};
use crate::{Error, Result, X86SmramState, X86TsegState};

pub(crate) const BASE: u64 = 0x30000;
pub(crate) const SIZE: usize = 0x10000;
pub(crate) const Q35_DEFAULT_SIZE: usize = 0x20000;
pub(crate) const TSEG_BASE: u64 = 0x1f800000;
pub(crate) const TSEG_SIZE: usize = 0x800000;
pub(crate) const STATIC_RANGES: &[(u64, u64)] = &[(BASE, SIZE as u64)];
const Q35_RANGES: &[(u64, u64)] = &[(BASE, Q35_DEFAULT_SIZE as u64), (TSEG_BASE, TSEG_SIZE as u64)];
const INITIAL_Q35: X86SmramState = X86SmramState { tseg: X86TsegState::Open, default_smbase_locked: false };

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
    tseg: Option<MmapRegion>,
    q35: Option<X86SmramState>,
    common: Vec<Owner>,
    // Frozen topology/registration history. Q35 AS0 visibility after CPU
    // creation is represented by q35, not by deleting ownership metadata.
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

fn check_common(mapping: Mapping, private_host: u64, private_range: (u64, u64)) -> Result<()> {
    crate::x86_64::check_memory_range(mapping.gpa, mapping.size)?;
    if mapping.host == 0 || mapping.host & 0xfff != 0 {
        return Err(invalid("SMM common backing must be page aligned"));
    }
    if overlaps(mapping.gpa, mapping.size, private_range.0, private_range.1)?
        || overlaps(mapping.host, mapping.size, private_host, private_range.1)? {
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
        Ok(Self { private, tseg: None, q35: None, common: Vec::new(), mappings: Vec::new(),
            phase: Phase::Unmapped, next: 1, limit: max_slots.min(1 << 16) as u32 })
    }

    /// All allocations exist before the first registration. Slots 0/1 are
    /// reserved in BOTH namespaces, even when their AS0 aliases are removed.
    pub fn new_q35(max_slots: usize) -> Result<Self> {
        if max_slots < 3 { return Err(invalid("Q35 SMRAM needs two private slots and a common slot")); }
        let private = MmapRegion::new(Q35_DEFAULT_SIZE)?;
        let tseg = MmapRegion::new(TSEG_SIZE)?;
        Ok(Self { private, tseg: Some(tseg), q35: Some(INITIAL_Q35), common: Vec::new(), mappings: Vec::new(),
            phase: Phase::Unmapped, next: 2, limit: max_slots.min(1 << 16) as u32 })
    }

    pub fn private_ranges(&self) -> &'static [(u64, u64)] {
        if self.q35.is_some() { Q35_RANGES } else { STATIC_RANGES }
    }

    pub fn check_cpu(&self) -> Result<()> {
        if self.phase != Phase::Sealed { return Err(invalid("SMM memory is not sealed or is poisoned; recreate the VM")); }
        Ok(())
    }

    pub fn q35_state(&self) -> Result<X86SmramState> {
        self.check_cpu()?;
        self.q35.ok_or(Error::Unsupported("SMRAM transitions require fixed Q35 configuration"))
    }

    fn default_slot(&self, as1: bool) -> Mapping {
        Mapping { slot: if as1 { 1 << 16 } else { 0 }, gpa: BASE, host: self.private.as_ptr() as u64,
            size: self.private.size() as u64, readonly: false }
    }

    fn tseg_slot(&self, as1: bool) -> Result<Mapping> {
        let tseg = self.tseg.as_ref().ok_or(Error::Unsupported("no Q35 TSEG"))?;
        Ok(Mapping { slot: if as1 { (1 << 16) | 1 } else { 1 }, gpa: TSEG_BASE,
            host: tseg.as_ptr() as u64, size: TSEG_SIZE as u64, readonly: false })
    }

    pub fn install_private(&mut self, mut install: impl FnMut(Mapping) -> Result<()>) -> Result<()> {
        if self.phase != Phase::Unmapped { return Err(invalid("SMM configuration already attempted")); }
        // A failed ioctl never drops a possibly mapped owner or enables a CPU.
        self.phase = Phase::Poisoned;
        let mut mappings = vec![self.default_slot(true)];
        if self.q35.is_some() {
            mappings.extend([self.tseg_slot(true)?, self.default_slot(false), self.tseg_slot(false)?]);
        }
        for mapping in mappings {
            install(mapping)?;
            self.mappings.push(mapping);
        }
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
            check_common(mapping, self.private.as_ptr() as u64, (BASE, self.private.size() as u64))?;
            if let Some(tseg) = &self.tseg {
                check_common(mapping, tseg.as_ptr() as u64, (TSEG_BASE, TSEG_SIZE as u64))?;
            }
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

    /// Called only by the sole stopped CPU after its write-exit grant is
    /// consumed. Multiple ioctls are not atomic: retain owners and poison
    /// before the first one; no rollback/readback success after any failure.
    pub fn apply_q35(&mut self, target: X86SmramState,
        mut install: impl FnMut(Mapping) -> Result<()>) -> Result<X86SmramState> {
        let old = self.q35_state()?;
        if (old.tseg == X86TsegState::Locked && target.tseg != X86TsegState::Locked)
            || (old.default_smbase_locked && !target.default_smbase_locked) {
            return Err(invalid("SMRAM locks cannot be cleared before cold recreation"));
        }
        let mut changes = Vec::new();
        if (old.tseg == X86TsegState::Open) != (target.tseg == X86TsegState::Open) {
            let mut slot = self.tseg_slot(false)?;
            if target.tseg != X86TsegState::Open { slot.size = 0; }
            changes.push(slot);
        }
        if !old.default_smbase_locked && target.default_smbase_locked {
            let mut slot = self.default_slot(false); slot.size = 0; changes.push(slot);
        }
        self.phase = Phase::Poisoned;
        for mapping in changes { install(mapping)?; }
        self.q35 = Some(target);
        self.phase = Phase::Sealed;
        Ok(target)
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
            assert!(check_common(mapping, private, (BASE, SIZE as u64)).is_err(), "{mapping:?}");
        }
        assert!(check_common(good, private, (BASE, SIZE as u64)).is_ok());
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

    fn q35() -> Memory {
        let mut memory = Memory::new_q35(8).unwrap();
        memory.install_private(|_| Ok(())).unwrap();
        memory.seal().unwrap(); memory
    }

    #[test]
    fn q35_reserves_both_namespaces_and_never_exports_private_memory_for_dma() {
        let mut memory = Memory::new_q35(3).unwrap();
        let mut calls = Vec::new();
        memory.install_private(|slot| { calls.push(slot); Ok(()) }).unwrap();
        assert_eq!(calls.iter().map(|s| (s.slot, s.gpa, s.size)).collect::<Vec<_>>(),
            [(0x10000, BASE, 0x20000), (0x10001, TSEG_BASE, 0x800000),
             (0, BASE, 0x20000), (1, TSEG_BASE, 0x800000)]);
        assert_eq!(calls[0].host, calls[2].host);
        assert_eq!(calls[1].host, calls[3].host);
        assert!(!overlaps(calls[0].host, calls[0].size, calls[1].host, calls[1].size).unwrap());
        let ram = GuestMemory::new(&[(GuestAddress(0x10000), 0x10000)]).unwrap();
        memory.map_memory(&ram, |slot| { calls.push(slot); Ok(()) }).unwrap();
        assert_eq!(calls[4].slot, 2); assert_eq!(calls[5].slot, 0x10002);
        assert!(memory.map_memory(&ram, |_| panic!("over capacity")).is_err());
        memory.seal().unwrap();
        for state in [INITIAL_Q35, X86SmramState { tseg: X86TsegState::Closed, ..INITIAL_Q35 },
            INITIAL_Q35, X86SmramState { tseg: X86TsegState::Locked, default_smbase_locked: true }] {
            memory.apply_q35(state, |_| Ok(())).unwrap();
            for &(base, size) in memory.private_ranges() {
                assert!(!ram.check_range(GuestAddress(base), size));
                assert!(ram.host_ranges(GuestAddress(base), size).is_none());
                assert!(ram.read_obj::<u8>(GuestAddress(base)).is_err());
                assert!(ram.write(GuestAddress(base), &[1]).is_err());
            }
        }
        assert!(Memory::new_q35(2).is_err());
    }

    #[test]
    fn q35_reopens_same_backing_and_locks_each_range_irreversibly() {
        let mut memory = q35();
        let mut calls = Vec::new();
        let closed = X86SmramState { tseg: X86TsegState::Closed, ..INITIAL_Q35 };
        let locked = X86SmramState { tseg: X86TsegState::Locked, ..INITIAL_Q35 };
        memory.apply_q35(closed, |slot| { calls.push(slot); Ok(()) }).unwrap();
        memory.apply_q35(INITIAL_Q35, |slot| { calls.push(slot); Ok(()) }).unwrap();
        memory.apply_q35(locked, |slot| { calls.push(slot); Ok(()) }).unwrap();
        assert_eq!(calls.iter().map(|s| (s.slot, s.size)).collect::<Vec<_>>(), [(1, 0), (1, 0x800000), (1, 0)]);
        assert!(calls.iter().all(|s| s.host == calls[0].host && s.gpa == TSEG_BASE));
        assert!(memory.apply_q35(closed, |_| panic!("unlock attempted ioctl")).is_err());
        assert!(memory.apply_q35(INITIAL_Q35, |_| panic!("reopen attempted ioctl")).is_err());
        assert_eq!(memory.apply_q35(locked, |_| panic!("idempotent lock ioctl")).unwrap(), locked);
        let both = X86SmramState { default_smbase_locked: true, ..locked };
        memory.apply_q35(both, |slot| { calls.push(slot); Ok(()) }).unwrap();
        assert_eq!((calls[3].slot, calls[3].gpa, calls[3].size), (0, BASE, 0));
        assert!(memory.apply_q35(locked, |_| panic!("default unlock ioctl")).is_err());
        assert_eq!(memory.q35_state().unwrap(), both);
        assert!(memory.private.shared_fd().is_none());
        assert!(memory.tseg.as_ref().unwrap().shared_fd().is_none());
        // No reset or reinstallation path can erase either latch.
        assert!(memory.install_private(|_| panic!("reinstall ioctl")).is_err());
        assert!(memory.seal().is_err());
    }

    #[test]
    fn q35_default_lock_does_not_implicitly_lock_tseg() {
        let mut memory = q35();
        let state = X86SmramState { default_smbase_locked: true, ..INITIAL_Q35 };
        let mut calls = Vec::new();
        memory.apply_q35(state, |slot| { calls.push(slot); Ok(()) }).unwrap();
        memory.apply_q35(X86SmramState { tseg: X86TsegState::Closed, ..state },
            |slot| { calls.push(slot); Ok(()) }).unwrap();
        memory.apply_q35(state, |slot| { calls.push(slot); Ok(()) }).unwrap();
        assert_eq!(calls.iter().map(|s| (s.slot, s.size)).collect::<Vec<_>>(), [(0, 0), (1, 0), (1, 0x800000)]);
        assert_eq!(memory.q35_state().unwrap(), state);
    }

    #[test]
    fn every_q35_initial_and_transition_failure_poisons_and_retains_both_owners() {
        for fail_at in 0..4 {
            let mut memory = Memory::new_q35(8).unwrap();
            let mut n = 0;
            assert!(memory.install_private(|_| {
                let call = n; n += 1;
                if call == fail_at { Err(invalid("injected private failure")) } else { Ok(()) }
            }).is_err());
            assert_eq!(n, fail_at + 1);
            assert_eq!(memory.private.size(), Q35_DEFAULT_SIZE);
            assert_eq!(memory.tseg.as_ref().unwrap().size(), TSEG_SIZE);
            assert!(memory.check_cpu().is_err()); assert!(memory.q35_state().is_err());
            assert!(memory.seal().is_err());
        }
        for fail_at in 0..2 {
            let mut memory = q35();
            let before = [memory.default_slot(false), memory.tseg_slot(false).unwrap()];
            let mut n = 0;
            let both = X86SmramState { tseg: X86TsegState::Locked, default_smbase_locked: true };
            assert!(memory.apply_q35(both, |_| {
                let call = n; n += 1;
                if call == fail_at { Err(invalid("injected removal failure")) } else { Ok(()) }
            }).is_err());
            assert_eq!(n, fail_at + 1);
            assert!(memory.check_cpu().is_err(), "shared RUN/completion/SMI readiness gate");
            assert!(memory.q35_state().is_err(), "old readback must not masquerade as applied state");
            assert!(memory.apply_q35(INITIAL_Q35, |_| panic!("poisoned retry")).is_err());
            assert_eq!([memory.default_slot(false), memory.tseg_slot(false).unwrap()], before);
        }
        let mut memory = q35();
        memory.apply_q35(X86SmramState { tseg: X86TsegState::Closed, ..INITIAL_Q35 }, |_| Ok(())).unwrap();
        assert!(memory.apply_q35(INITIAL_Q35, |_| Err(invalid("injected reopen failure"))).is_err());
        assert!(memory.check_cpu().is_err()); assert!(memory.q35_state().is_err());
    }

    #[test]
    fn q35_rejects_both_gpa_and_backing_aliases_before_any_mapping() {
        let mut memory = Memory::new_q35(8).unwrap();
        memory.install_private(|_| Ok(())).unwrap();
        for private in [memory.default_slot(false), memory.tseg_slot(false).unwrap()] {
            let good = Mapping { slot: 0, gpa: 0x10000, host: 0x200000, size: 0x10000, readonly: false };
            for mapping in [Mapping { gpa: private.gpa, ..good },
                Mapping { gpa: private.gpa - 0x1000, ..good },
                Mapping { host: private.host, ..good }, Mapping { host: private.host - 0x1000, ..good }] {
                assert!(memory.preflight(&[mapping]).is_err());
            }
            let ram = GuestMemory::new(&[(GuestAddress(private.gpa), 0x10000)]).unwrap();
            assert!(memory.map_memory(&ram, |_| panic!("alias ioctl")).is_err());
            let rom = Arc::new(MmapRegion::new(0x10000).unwrap());
            assert!(memory.map_readonly(private.gpa, rom, |_| panic!("ROM alias ioctl")).is_err());
        }
        assert!(memory.common.is_empty());
        // Generic/common mapping before configure is rejected by Setup, even
        // if its first ioctl failed; no private allocation can alias it later.
        let mut setup = Setup { mapping_attempted: true, ..Default::default() };
        assert!(setup.begin(false).is_err());
    }

    #[test]
    fn static_profile_rejects_dynamic_transitions_and_retained_q35_owner_survives_vm_handle() {
        let mut memory = open(); memory.seal().unwrap();
        assert!(memory.apply_q35(INITIAL_Q35, |_| panic!("static profile transition")).is_err());
        assert!(memory.check_cpu().is_ok());
        let vm_owner = Arc::new(std::sync::Mutex::new(q35()));
        let cpu_owner = vm_owner.clone(); let weak = Arc::downgrade(&vm_owner);
        drop(vm_owner);
        {
            let mut memory = cpu_owner.lock().unwrap();
            memory.apply_q35(X86SmramState { tseg: X86TsegState::Locked, default_smbase_locked: true }, |_| Ok(())).unwrap();
            assert_eq!(memory.private.size(), Q35_DEFAULT_SIZE);
            assert_eq!(memory.tseg.as_ref().unwrap().size(), TSEG_SIZE);
        }
        assert!(weak.upgrade().is_some()); drop(cpu_owner); assert!(weak.upgrade().is_none());
    }
}

//! x86 KVM backend: one CPU, Linux64 or fresh reset-vector entry, scalar I/O.
//! Optional restricted AS1 SMRAM is for the SMI/RSM/flash harness, not a board.
//! CPU/irqchip snapshots and guest hypervisors are deliberately unsupported.

use std::sync::{Arc, Mutex};

use kvm_bindings::{kvm_mp_state, kvm_pit_config, kvm_regs, kvm_segment, CpuId,
    KVM_MAX_CPUID_ENTRIES, KVM_MP_STATE_RUNNABLE, KVM_PIT_SPEAKER_DUMMY,
    KVM_SYSTEM_EVENT_RESET, KVM_SYSTEM_EVENT_SHUTDOWN, KVM_RUN_X86_SMM};
use kvm_ioctls::{Cap, Kvm, VcpuExit as KExit, VcpuFd, VmFd};

use super::{clear_thread_run, kerr, set_thread_run, Chip, KvmKicker};
pub(super) use crate::x86_64::check_memory_range;
use crate::x86_64::{cpuid_registers, initialize_reset_vector, mmio_len, scalar_pio, validate_boot,
    check_completion, check_smi_ready, check_smm_boot, classify_io, PendingRead,
    ResetEntryGuard, ResetSegment, ResetVectorCpu, ResetVectorState, IDENTITY_MAP, TSS_BASE};
use crate::{BootRegs, Error, IrqChipConfig, IrqChipInfo, Result, Vcpu, VcpuExit, VcpuKicker, VcpuState, VmParams, X86IoExit};

const _: () = assert!(KVM_RUN_X86_SMM == 1); // pure classifier's UAPI bit

pub(super) fn irqchip_info() -> IrqChipInfo {
    IrqChipInfo { dist_size: 0, redist_size_per_cpu: 0, msi_size: 0, spi_base: 0, spi_count: 24, msi_frame_in_kernel: true }
}

pub(super) fn create_vm(kvm: &Kvm, params: &VmParams) -> Result<(VmFd, Chip)> {
    if params.irqchip != IrqChipConfig::Ioapic {
        return Err(Error::Unsupported("x86 KVM requires the Ioapic configuration"));
    }
    if params.vcpus != 1 {
        return Err(Error::Unsupported("initial x86 KVM backend supports one vCPU"));
    }
    if params.nested {
        return Err(Error::Unsupported("exposing virtualization inside the x86 guest"));
    }
    for (cap, name) in [
        (Cap::UserMemory, "KVM_CAP_USER_MEMORY"),
        (Cap::Irqchip, "KVM_CAP_IRQCHIP"),
        (Cap::SetTssAddr, "KVM_CAP_SET_TSS_ADDR"),
        (Cap::SetIdentityMapAddr, "KVM_CAP_SET_IDENTITY_MAP_ADDR"),
        (Cap::Pit2, "KVM_CAP_PIT2"),
        (Cap::ExtCpuid, "KVM_CAP_EXT_CPUID"),
        (Cap::MpState, "KVM_CAP_MP_STATE"),
        (Cap::ImmediateExit, "KVM_CAP_IMMEDIATE_EXIT"),
    ] {
        if !kvm.check_extension(cap) {
            return Err(Error::Unsupported(name));
        }
    }
    let fd = kvm.create_vm().map_err(kerr("KVM_CREATE_VM"))?;
    fd.set_tss_address(TSS_BASE as usize).map_err(kerr("KVM_SET_TSS_ADDR"))?;
    fd.set_identity_map_address(IDENTITY_MAP).map_err(kerr("KVM_SET_IDENTITY_MAP_ADDR"))?;
    fd.create_irq_chip().map_err(kerr("KVM_CREATE_IRQCHIP"))?;
    // Linux calibrates timers through PIT channel 2 / speaker port 0x61.
    // Ask KVM to supply that port rather than inventing counter values.
    fd.create_pit2(kvm_pit_config { flags: KVM_PIT_SPEAKER_DUMMY, ..Default::default() })
        .map_err(kerr("KVM_CREATE_PIT2"))?;
    Ok((fd, Chip::Ioapic))
}

/// Keep KVM's supported feature set, with one logical CPU and no guest VMX/SVM.
/// This is host-dependent, not a portable CPU/snapshot compatibility model.
fn configure_cpuid(cpuid: &mut CpuId) -> Result<()> {
    let pae = cpuid.as_slice().iter().any(|e| e.function == 1 && e.edx & (1 << 6) != 0);
    let long_mode = cpuid.as_slice().iter().any(|e| e.function == 0x8000_0001 && e.edx & (1 << 29) != 0);
    if !pae || !long_mode {
        return Err(Error::Unsupported("KVM CPU lacks PAE or long mode"));
    }
    for e in cpuid.as_mut_slice() {
        [e.eax, e.ebx, e.ecx, e.edx] = cpuid_registers(e.function, e.index, [e.eax, e.ebx, e.ecx, e.edx]);
    }
    Ok(())
}

fn segment(selector: u16, code: bool) -> kvm_segment {
    kvm_segment {
        base: 0, limit: 0xffff_ffff, selector, type_: if code { 11 } else { 3 },
        present: 1, dpl: 0, db: u8::from(!code), s: 1, l: u8::from(code), g: 1,
        ..Default::default()
    }
}

fn reset_segment(s: kvm_segment) -> ResetSegment {
    ResetSegment { base: s.base, limit: s.limit, selector: s.selector, kind: s.type_,
        attributes: [s.present, s.dpl, s.db, s.s, s.l, s.g, s.avl, s.unusable] }
}

impl ResetVectorCpu for VcpuFd {
    fn read_reset_state(&mut self) -> Result<ResetVectorState> {
        let r = self.get_regs().map_err(kerr("KVM_GET_REGS for firmware entry"))?;
        let s = self.get_sregs().map_err(kerr("KVM_GET_SREGS for firmware entry"))?;
        let mp = self.get_mp_state().map_err(kerr("KVM_GET_MP_STATE for firmware entry"))?;
        Ok(ResetVectorState {
            gprs: [r.rax, r.rbx, r.rcx, r.rdx, r.rsi, r.rdi, r.rsp, r.rbp,
                r.r8, r.r9, r.r10, r.r11, r.r12, r.r13, r.r14, r.r15, r.rip, r.rflags],
            controls: [s.cr0, s.cr2, s.cr3, s.cr4, s.cr8, s.efer],
            segments: [s.cs, s.ds, s.es, s.fs, s.gs, s.ss, s.tr, s.ldt].map(reset_segment),
            tables: [(s.gdt.base, s.gdt.limit), (s.idt.base, s.idt.limit)],
            interrupt_bitmap: s.interrupt_bitmap,
            runnable: mp.mp_state == KVM_MP_STATE_RUNNABLE,
        })
    }

    fn set_reset_gprs(&mut self, signature: u32) -> Result<()> {
        self.set_regs(&kvm_regs { rdx: u64::from(signature), rip: 0xfff0, rflags: 2, ..Default::default() })
            .map_err(kerr("KVM_SET_REGS for firmware entry"))
    }

    fn set_runnable(&mut self) -> Result<()> {
        self.set_mp_state(kvm_mp_state { mp_state: KVM_MP_STATE_RUNNABLE })
            .map_err(kerr("KVM_SET_MP_STATE for firmware entry"))
    }
}

pub struct KvmVcpu {
    // Close the CPU fd before dropping its last memory backing owners.
    fd: VcpuFd,
    smm_owners: Option<Arc<Mutex<super::Slots>>>,
    kicker: Arc<KvmKicker>,
    run_size: usize,
    pending: Option<PendingRead>,
    reset_entry: ResetEntryGuard,
    cpuid_signature: u32,
    initialized: bool,
    last_run_smm: bool,
    smm_faulted: bool,
}

impl std::fmt::Debug for KvmVcpu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvmVcpu").field("index", &0).field("pending", &self.pending).finish()
    }
}

impl KvmVcpu {
    pub(super) fn new(vm: &VmFd, index: u32, n: u32, smm_owners: Option<Arc<Mutex<super::Slots>>>) -> Result<Self> {
        if index != 0 || n != 1 {
            return Err(Error::Unsupported("initial x86 KVM backend supports only vCPU 0"));
        }
        let kvm = Kvm::new().map_err(kerr("open KVM capabilities"))?;
        let mut cpuid = kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES).map_err(kerr("KVM_GET_SUPPORTED_CPUID"))?;
        configure_cpuid(&mut cpuid)?;
        let cpuid_signature = cpuid.as_slice().iter().find(|e| e.function == 1)
            .map(|e| e.eax).ok_or(Error::Unsupported("KVM CPU lacks its version signature"))?;
        let mut fd = vm.create_vcpu(0).map_err(kerr("KVM_CREATE_VCPU"))?;
        fd.set_cpuid2(&cpuid).map_err(kerr("KVM_SET_CPUID2"))?;
        let run_size = kvm.get_vcpu_mmap_size().map_err(kerr("KVM_GET_VCPU_MMAP_SIZE"))?;
        set_thread_run(fd.get_kvm_run() as *mut _);
        Ok(Self { fd, smm_owners, kicker: KvmKicker::current(), run_size, pending: None,
            reset_entry: ResetEntryGuard::Fresh, cpuid_signature, initialized: false,
            last_run_smm: false, smm_faulted: false })
    }

    /// Called only immediately after an IoIn/IoOut exit.
    fn pio_location(&mut self, total: usize) -> Result<(usize, usize)> {
        // SAFETY: the just-returned KVM exit is IO, so this union field is active.
        let io = unsafe { self.fd.get_kvm_run().__bindgen_anon_1.io };
        let len = scalar_pio(io.size, io.count, total)?;
        let offset = usize::try_from(io.data_offset).map_err(|_| Error::GuestFault("PIO offset overflow".into()))?;
        if offset.checked_add(len).map_or(true, |end| end > self.run_size) {
            return Err(Error::GuestFault("PIO data outside the KVM mapping".into()));
        }
        Ok((len, offset))
    }

    fn io_exit(&mut self, flags: u16, io: X86IoExit) -> Result<VcpuExit> {
        let result = classify_io(self.smm_owners.is_some(), flags, io);
        if result.is_err() { self.smm_faulted = true; }
        result
    }

    fn check_smm_fault(&self) -> Result<()> {
        if self.smm_faulted { return Err(Error::Invalid("failed SMM operation; recreate the vCPU".into())); }
        Ok(())
    }
}

impl Drop for KvmVcpu {
    fn drop(&mut self) {
        // A retained kicker can signal this thread after the vCPU is dropped.
        // Remove the signal handler's pointer before the fd unmaps kvm_run.
        clear_thread_run(self.fd.get_kvm_run() as *mut _);
    }
}

impl Vcpu for KvmVcpu {
    fn index(&self) -> u32 { 0 }

    fn run(&mut self) -> Result<VcpuExit> {
        // Even an unsuccessful or kicked attempt ends fresh-reset eligibility.
        self.reset_entry.other_attempt()?;
        self.check_smm_fault()?;
        if self.smm_owners.is_some() && !self.initialized {
            return Err(Error::Invalid("initialize restricted SMM CPU before running".into()));
        }
        if self.pending.is_some() {
            return Err(Error::Invalid("complete the pending guest read before running".into()));
        }
        // A thread may own vCPUs from several VMs. Bind the one entering RUN,
        // not merely whichever vCPU that thread constructed most recently.
        set_thread_run(self.fd.get_kvm_run() as *mut _);
        loop {
            // Clear BEFORE testing the atomic flag: a kick arriving between
            // that test and KVM_RUN must retain immediate_exit=1.
            self.fd.set_kvm_immediate_exit(0);
            if self.kicker.take() {
                return Ok(VcpuExit::Kicked);
            }
            let run = self.fd.get_kvm_run() as *mut kvm_bindings::kvm_run;
            let result = self.fd.run();
            // SAFETY: the live fd owns this mapping. flags is disjoint from
            // the borrowed exit data. Read it from THIS successful RUN, before
            // any other ioctl or pending-read publication; never GET_EVENTS.
            let flags = if result.is_ok() { unsafe { (*run).flags } } else { 0 };
            if result.is_ok() { self.last_run_smm = flags & KVM_RUN_X86_SMM as u16 != 0; }
            return match result {
                Ok(KExit::IoOut(port, data)) => {
                    let len = data.len();
                    if len > 4 { return Err(Error::Unsupported("multi-element/string x86 port I/O")); }
                    let mut value = [0; 4];
                    value[..len].copy_from_slice(data);
                    self.pio_location(len)?;
                    self.io_exit(flags, X86IoExit::PioOut { port, len, data: value })
                }
                Ok(KExit::IoIn(port, data)) => {
                    let total = data.len();
                    let (len, offset) = self.pio_location(total)?;
                    let exit = self.io_exit(flags, X86IoExit::PioIn { port, len })?;
                    self.pending = Some(PendingRead::Pio { len, offset });
                    Ok(exit)
                }
                Ok(KExit::MmioWrite(addr, data)) => {
                    let len = data.len();
                    mmio_len(len)?;
                    let mut value = [0; 8];
                    value[..len].copy_from_slice(data);
                    self.io_exit(flags, X86IoExit::MmioWrite { addr, len, data: value })
                }
                Ok(KExit::MmioRead(addr, data)) => {
                    let len = data.len();
                    mmio_len(len)?;
                    let exit = self.io_exit(flags, X86IoExit::MmioRead { addr, len })?;
                    self.pending = Some(PendingRead::Mmio(len));
                    Ok(exit)
                }
                Ok(KExit::Hlt) => Ok(VcpuExit::Halted),
                Ok(KExit::Intr) => continue,
                // On x86 this exit normally means a triple fault, not poweroff.
                Ok(KExit::Shutdown) => Err(Error::GuestFault("x86 shutdown/triple fault".into())),
                Ok(KExit::SystemEvent(KVM_SYSTEM_EVENT_SHUTDOWN, _)) => Ok(VcpuExit::SystemOff),
                Ok(KExit::SystemEvent(KVM_SYSTEM_EVENT_RESET, _)) => Ok(VcpuExit::SystemReset),
                Ok(other) => Err(Error::GuestFault(format!("unsupported x86 KVM exit {other:?}"))),
                Err(e) if e.errno() == libc::EINTR || e.errno() == libc::EAGAIN => continue,
                Err(e) => Err(kerr("KVM_RUN")(e)),
            };
        }
    }

    fn complete_mmio_read(&mut self, data: &[u8]) -> Result<()> {
        self.check_smm_fault()?;
        match check_completion(self.pending, true, data.len())? {
            PendingRead::Mmio(len) => {
                // SAFETY: a pending MMIO read owns this exit until completion.
                unsafe { self.fd.get_kvm_run().__bindgen_anon_1.mmio.data[..len].copy_from_slice(data) };
                self.pending = None;
                Ok(())
            }
            _ => Err(Error::Invalid("no matching MMIO read or wrong completion length".into())),
        }
    }

    fn complete_pio_in(&mut self, data: &[u8]) -> Result<()> {
        self.check_smm_fault()?;
        match check_completion(self.pending, false, data.len())? {
            PendingRead::Pio { len, offset } => {
                let base = self.fd.get_kvm_run() as *mut _ as *mut u8;
                // SAFETY: pio_location checked offset+len against the mmap
                // length. Its live fd retains that writable mapping; no RUN
                // or boot-register change is allowed while the read is pending.
                unsafe { std::slice::from_raw_parts_mut(base.add(offset), len).copy_from_slice(data) };
                self.pending = None;
                Ok(())
            }
            _ => Err(Error::Invalid("no matching PIO read or wrong completion length".into())),
        }
    }

    fn complete_hypercall(&mut self, _ret: [u64; 4]) -> Result<()> {
        Err(Error::Unsupported("x86 userspace hypercalls"))
    }

    fn kicker(&self) -> Arc<dyn VcpuKicker> { self.kicker.clone() }

    fn inject_smi(&mut self) -> Result<()> {
        self.check_smm_fault()?;
        check_smi_ready(self.smm_owners.is_some(), self.initialized, self.pending.is_some(), self.last_run_smm)?;
        // A failed injection never permits an ambiguous retry or RUN.
        self.smm_faulted = true;
        self.fd.smi().map_err(kerr("KVM_SMI"))?;
        self.smm_faulted = false;
        Ok(())
    }

    fn set_boot_regs(&mut self, regs: &BootRegs) -> Result<()> {
        self.check_smm_fault()?;
        if self.last_run_smm { return Err(Error::Invalid("cannot replace boot registers while in SMM".into())); }
        if let Err(error) = check_smm_boot(self.smm_owners.is_some(), regs) {
            self.reset_entry.other_attempt()?;
            return Err(error);
        }
        if matches!(regs, BootRegs::X86ResetVector) {
            // Any previous RUN/boot attempt has already consumed eligibility.
            // The helper keeps failures latched, including readback failures.
            let result = initialize_reset_vector(&mut self.reset_entry, &mut self.fd, self.cpuid_signature);
            if result.is_ok() { self.initialized = true; }
            return result;
        }
        self.reset_entry.other_attempt()?;
        if self.pending.is_some() {
            return Err(Error::Invalid("complete the pending guest read before changing boot state".into()));
        }
        let BootRegs::X86_64 { rip, rsi, cr3, gdt } = *regs else {
            return Err(Error::Invalid("ARM boot registers on x86".into()));
        };
        validate_boot(rip, rsi, cr3, gdt)?;
        let mut s = self.fd.get_sregs().map_err(kerr("KVM_GET_SREGS"))?;
        s.cs = segment(0x10, true);
        s.ds = segment(0x18, false);
        s.es = s.ds; s.fs = s.ds; s.gs = s.ds; s.ss = s.ds;
        s.gdt.base = gdt; s.gdt.limit = 31;
        s.cr0 = 0x8001_0033; // PG, WP, NE, ET, MP, PE
        s.cr3 = cr3;
        s.cr4 = 1 << 5; // PAE; no LA57 or guest VMX
        s.efer = (1 << 8) | (1 << 10); // LME, LMA
        self.fd.set_sregs(&s).map_err(kerr("KVM_SET_SREGS"))?;
        self.fd.set_regs(&kvm_regs { rip, rsi, rflags: 2, ..Default::default() }).map_err(kerr("KVM_SET_REGS"))?;
        self.fd.set_mp_state(kvm_mp_state { mp_state: KVM_MP_STATE_RUNNABLE }).map_err(kerr("KVM_SET_MP_STATE"))?;
        self.initialized = true;
        Ok(())
    }

    fn arch_id(&self) -> u64 { 0 }

    fn save_state(&mut self) -> Result<VcpuState> {
        Err(Error::Unsupported("x86 CPU snapshots are not implemented"))
    }

    fn restore_state(&mut self, _state: &VcpuState) -> Result<()> {
        Err(Error::Unsupported("x86 CPU snapshots are not implemented"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn flat_linux64_segment_cache_matches_the_gdt() {
        let code = segment(0x10, true);
        let data = segment(0x18, false);
        assert_eq!((code.limit, data.limit), (u32::MAX, u32::MAX));
        assert_eq!((code.selector, code.l, code.db), (0x10, 1, 0));
        assert_eq!((data.selector, data.l, data.db), (0x18, 0, 1));
    }

    #[test]
    fn cpuid_is_single_cpu_without_guest_virtualization() {
        use kvm_bindings::kvm_cpuid_entry2 as Entry;
        let mut cpuid = CpuId::from_entries(&[
            Entry { function: 1, ebx: u32::MAX, ecx: 1 << 5, edx: (1 << 6) | (1 << 28), ..Default::default() },
            Entry { function: 0x8000_0001, ecx: 1 << 2, edx: 1 << 29, ..Default::default() },
            Entry { function: 0xb, index: 0, eax: 5, ebx: 32, edx: 12, ..Default::default() },
            Entry { function: 0x8000_000a, eax: 1, edx: u32::MAX, ..Default::default() },
        ]).unwrap();
        configure_cpuid(&mut cpuid).unwrap();
        let entries = cpuid.as_slice();
        assert_eq!(entries[0].ebx >> 16, 1);
        assert_eq!(entries[0].ecx & (1 << 5), 0);
        assert_eq!(entries[0].edx & (1 << 28), 0);
        assert_eq!(entries[1].ecx & (1 << 2), 0);
        assert_eq!((entries[2].eax, entries[2].ebx, entries[2].edx), (0, 1, 0));
        assert_eq!((entries[3].eax, entries[3].edx), (0, 0));
        let mut absent = CpuId::from_entries(&[]).unwrap();
        assert!(configure_cpuid(&mut absent).is_err());
    }
}

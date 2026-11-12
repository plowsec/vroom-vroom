//! x86 KVM with in-kernel PIC, IOAPIC, LAPIC and PIT.
use super::{kerr, set_thread_run, Chip, KvmKicker};
use crate::{
    BootRegs, Error, IrqChipConfig, IrqChipInfo, Result, Vcpu, VcpuExit, VcpuKicker, VcpuState,
    VmParams,
};
use kvm_bindings::*;
use kvm_ioctls::{Kvm, VcpuExit as KExit, VcpuFd, VmFd};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub(super) fn irqchip_info() -> IrqChipInfo {
    IrqChipInfo {
        dist_size: 0,
        redist_size_per_cpu: 0,
        msi_size: 0,
        spi_base: 0,
        spi_count: 24,
        msi_frame_in_kernel: true,
    }
}

pub(super) fn create_vm(kvm: &Kvm, p: &VmParams) -> Result<(VmFd, Chip)> {
    if p.irqchip != IrqChipConfig::Ioapic || p.vcpus == 0 || p.vcpus > 254 {
        return Err(Error::Invalid(
            "x86 requires IOAPIC and 1..=254 vCPUs".into(),
        ));
    }
    if p.nested {
        return Err(Error::Unsupported("nested virtualization on x86"));
    }
    let vm = kvm.create_vm().map_err(kerr("KVM_CREATE_VM"))?;
    vm.set_tss_address(0xfefb_d000)
        .map_err(kerr("KVM_SET_TSS_ADDR"))?;
    vm.set_identity_map_address(0xfefb_c000)
        .map_err(kerr("KVM_SET_IDENTITY_MAP_ADDR"))?;
    vm.create_irq_chip().map_err(kerr("KVM_CREATE_IRQCHIP"))?;
    let routes = pc_routes();
    // SAFETY: repr(C) header matches kvm_irq_routing, followed by exactly
    // nr live routing entries. KVM reads the flexible array during ioctl.
    let routing = unsafe { &*((&routes as *const PcRoutes).cast::<kvm_irq_routing>()) };
    vm.set_gsi_routing(routing)
        .map_err(kerr("KVM_SET_GSI_ROUTING"))?;
    vm.create_pit2(kvm_pit_config {
        flags: KVM_PIT_SPEAKER_DUMMY,
        ..Default::default()
    })
    .map_err(kerr("KVM_CREATE_PIT2"))?;
    Ok((vm, Chip::Ioapic))
}

#[repr(C)]
struct PcRoutes {
    nr: u32,
    flags: u32,
    entries: [kvm_irq_routing_entry; 40],
}

fn pc_routes() -> PcRoutes {
    let mut table = PcRoutes {
        nr: 0,
        flags: 0,
        entries: [kvm_irq_routing_entry::default(); 40],
    };
    let mut add = |gsi, irqchip, pin| {
        let mut e = kvm_irq_routing_entry {
            gsi,
            type_: KVM_IRQ_ROUTING_IRQCHIP,
            ..Default::default()
        };
        e.u.irqchip = kvm_irq_routing_irqchip { irqchip, pin };
        table.entries[table.nr as usize] = e;
        table.nr += 1;
    };
    for gsi in 0..24 {
        add(
            gsi,
            KVM_IRQCHIP_IOAPIC,
            match gsi {
                0 => 2,
                2 => 0,
                n => n,
            },
        );
        if gsi < 16 {
            add(
                gsi,
                if gsi < 8 {
                    KVM_IRQCHIP_PIC_MASTER
                } else {
                    KVM_IRQCHIP_PIC_SLAVE
                },
                gsi % 8,
            );
        }
    }
    table
}

fn state_error(e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("x86 state: {e}"))
}

#[derive(Serialize, Deserialize)]
struct CpuState {
    regs: kvm_regs,
    sregs: kvm_sregs,
    xsave: kvm_xsave,
    xcrs: kvm_xcrs,
    lapic: kvm_lapic_state,
    mp: kvm_mp_state,
    events: kvm_vcpu_events,
    debug: kvm_debugregs,
    msrs: Vec<kvm_msr_entry>,
}

#[derive(Serialize, Deserialize)]
struct ChipState {
    chips: Vec<kvm_irqchip>,
    pit: kvm_pit_state2,
    clock: kvm_clock_data,
}

pub(super) fn save_chip(vm: &VmFd) -> Result<Vec<u8>> {
    let mut chips = Vec::new();
    for chip_id in 0..3 {
        let mut chip = kvm_irqchip {
            chip_id,
            ..Default::default()
        };
        vm.get_irqchip(&mut chip).map_err(kerr("KVM_GET_IRQCHIP"))?;
        chips.push(chip);
    }
    serde_json::to_vec(&ChipState {
        chips,
        pit: vm.get_pit2().map_err(kerr("KVM_GET_PIT2"))?,
        clock: vm.get_clock().map_err(kerr("KVM_GET_CLOCK"))?,
    })
    .map_err(state_error)
}

pub(super) fn restore_chip(vm: &VmFd, blob: &[u8]) -> Result<()> {
    let s: ChipState = serde_json::from_slice(blob).map_err(state_error)?;
    if s.chips.len() != 3
        || s.chips
            .iter()
            .enumerate()
            .any(|(i, c)| c.chip_id != i as u32)
    {
        return Err(state_error("invalid irqchip IDs"));
    }
    for c in &s.chips {
        vm.set_irqchip(c).map_err(kerr("KVM_SET_IRQCHIP"))?;
    }
    vm.set_pit2(&s.pit).map_err(kerr("KVM_SET_PIT2"))?;
    let mut clock = s.clock;
    clock.flags = 0;
    vm.set_clock(&clock).map_err(kerr("KVM_SET_CLOCK"))
}

#[derive(Debug)]
enum Read {
    Mmio(usize),
    Pio { offset: usize, len: usize },
}

#[derive(Debug)]
pub struct KvmVcpu {
    fd: VcpuFd,
    index: u32,
    kicker: Arc<KvmKicker>,
    pending: Option<Read>,
    msr_indices: Vec<u32>,
}

impl Drop for KvmVcpu {
    fn drop(&mut self) {
        set_thread_run(std::ptr::null_mut());
    }
}

impl KvmVcpu {
    pub(super) fn new(vm: &VmFd, index: u32, n: u32) -> Result<Self> {
        let kvm = Kvm::new().map_err(kerr("open KVM capabilities"))?;
        let mut cpuid = kvm
            .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
            .map_err(kerr("KVM_GET_SUPPORTED_CPUID"))?;
        for e in cpuid.as_mut_slice() {
            match e.function {
                1 => {
                    e.ebx = (e.ebx & 0x0000_ffff) | (n << 16) | (index << 24);
                    // Conservative baseline: SSE, without nested VMX or XSAVE/AVX.
                    // This keeps saved extended state within the fixed KVM XSAVE ABI.
                    e.ecx &= !((1 << 5) | (1 << 12) | (7 << 26) | (1 << 29));
                    e.ecx |= 1 << 31;
                    if n > 1 {
                        e.edx |= 1 << 28;
                    } else {
                        e.edx &= !(1 << 28);
                    }
                }
                7 | 0xd => {
                    e.eax = 0;
                    e.ebx = 0;
                    e.ecx = 0;
                    e.edx = 0;
                }
                0xb | 0x1f => {
                    e.edx = index;
                    if e.index == 0 {
                        e.eax = 0;
                        e.ebx = 1;
                        e.ecx = 1 << 8;
                    } else if e.index == 1 {
                        e.eax = n.next_power_of_two().trailing_zeros();
                        e.ebx = n;
                        e.ecx = (2 << 8) | 1;
                    } else {
                        e.eax = 0;
                        e.ebx = 0;
                    }
                }
                0x8000_0001 => e.ecx &= !(1 << 2),
                0x8000_0008 => {
                    e.ecx = (e.ecx & !0xffff)
                        | (n - 1)
                        | (n.next_power_of_two().trailing_zeros() << 12);
                }
                0x8000_001e => {
                    e.eax = index;
                    e.ebx = index; // one thread per core
                    e.ecx = 0; // one node
                    e.edx = 0;
                }
                _ => {}
            }
        }
        let mut fd = vm
            .create_vcpu(index as u64)
            .map_err(kerr("KVM_CREATE_VCPU"))?;
        fd.set_cpuid2(&cpuid).map_err(kerr("KVM_SET_CPUID2"))?;
        let entries: Vec<_> = [
            0x174,
            0x175,
            0x176,
            0xc000_0081,
            0xc000_0082,
            0xc000_0083,
            0xc000_0084,
            0xc000_0102,
        ]
        .into_iter()
        .map(|index| kvm_msr_entry {
            index,
            ..Default::default()
        })
        .collect();
        let msrs = Msrs::from_entries(&entries).map_err(state_error)?;
        if fd.set_msrs(&msrs).map_err(kerr("KVM_SET_MSRS"))? != entries.len() {
            return Err(state_error("short MSR initialization"));
        }
        fd.set_mp_state(kvm_mp_state {
            mp_state: if index == 0 {
                KVM_MP_STATE_RUNNABLE
            } else {
                KVM_MP_STATE_UNINITIALIZED
            },
        })
        .map_err(kerr("KVM_SET_MP_STATE"))?;
        let msr_indices = kvm
            .get_msr_index_list()
            .map_err(kerr("KVM_GET_MSR_INDEX_LIST"))?
            .as_slice()
            .iter()
            .copied()
            // KVM's system list includes VMX capability MSRs even when
            // this vCPU's CPUID disables VMX. Those cannot be read from
            // this guest and have no state to restore without nested VMX.
            .filter(|index| !(0x480..=0x49f).contains(index))
            .collect();
        set_thread_run(fd.get_kvm_run() as *mut _);
        Ok(Self {
            fd,
            index,
            kicker: KvmKicker::current(),
            pending: None,
            msr_indices,
        })
    }
}

impl Vcpu for KvmVcpu {
    fn debug_write_register(&mut self, register: crate::debug::Register, value: u64) -> Result<()> {
        use crate::debug::{Register, X86Register as R};
        let Register::X86(register) = register else {
            return Err(Error::Invalid("an AMD64 register is required".into()));
        };
        let mut r = self.fd.get_regs().map_err(kerr("KVM_GET_REGS"))?;
        let target = match register {
            R::Rax => &mut r.rax,
            R::Rbx => &mut r.rbx,
            R::Rcx => &mut r.rcx,
            R::Rdx => &mut r.rdx,
            R::Rsi => &mut r.rsi,
            R::Rdi => &mut r.rdi,
            R::Rbp => &mut r.rbp,
            R::Rsp => &mut r.rsp,
            R::R8 => &mut r.r8,
            R::R9 => &mut r.r9,
            R::R10 => &mut r.r10,
            R::R11 => &mut r.r11,
            R::R12 => &mut r.r12,
            R::R13 => &mut r.r13,
            R::R14 => &mut r.r14,
            R::R15 => &mut r.r15,
            R::Rip => &mut r.rip,
            R::Rflags => &mut r.rflags,
            _ => {
                let mut s = self.fd.get_sregs().map_err(kerr("KVM_GET_SREGS"))?;
                match register {
                    R::Cr0 => s.cr0 = value,
                    R::Cr2 => s.cr2 = value,
                    R::Cr3 => s.cr3 = value,
                    R::Cr4 => s.cr4 = value,
                    R::Efer => s.efer = value,
                    _ => unreachable!(),
                }
                return self.fd.set_sregs(&s).map_err(kerr("KVM_SET_SREGS"));
            }
        };
        *target = if register == R::Rflags {
            value | 2
        } else {
            value
        };
        self.fd.set_regs(&r).map_err(kerr("KVM_SET_REGS"))
    }
    fn configure_debug(&mut self, config: &crate::debug::DebugConfig) -> Result<()> {
        let debug = guest_debug(config)?;
        self.fd
            .set_guest_debug(&debug)
            .map_err(kerr("KVM_SET_GUEST_DEBUG"))
    }
    fn index(&self) -> u32 {
        self.index
    }
    fn arch_id(&self) -> u64 {
        self.index as u64
    }
    // AP startup is owned by KVM (INIT/SIPI), as PSCI is on ARM.
    fn in_kernel_psci(&self) -> bool {
        true
    }
    fn kicker(&self) -> Arc<dyn VcpuKicker> {
        self.kicker.clone()
    }
    fn run(&mut self) -> Result<VcpuExit> {
        loop {
            if self.kicker.take() {
                self.fd.set_kvm_immediate_exit(0);
                return Ok(VcpuExit::Kicked);
            }
            let exit = match self.fd.run() {
                Ok(KExit::MmioRead(addr, data)) => {
                    let len = data.len();
                    self.pending = Some(Read::Mmio(len));
                    VcpuExit::MmioRead { addr, len }
                }
                Ok(KExit::MmioWrite(addr, bytes)) => {
                    let mut data = [0; 8];
                    let len = bytes.len();
                    data[..len].copy_from_slice(bytes);
                    VcpuExit::MmioWrite { addr, len, data }
                }
                Ok(KExit::IoIn(port, bytes)) => {
                    let len = bytes.len();
                    let ptr = bytes.as_mut_ptr() as usize;
                    let run = self.fd.get_kvm_run();
                    // SAFETY: the exit was KVM_EXIT_IO.
                    let io = unsafe { run.__bindgen_anon_1.io };
                    let offset = ptr - (run as *mut _ as usize);
                    self.pending = Some(Read::Pio { offset, len });
                    VcpuExit::PioInBatch {
                        port,
                        size: io.size as usize,
                        count: io.count as usize,
                    }
                }
                Ok(KExit::IoOut(port, bytes)) => {
                    let data = bytes.to_vec();
                    // SAFETY: the exit was KVM_EXIT_IO.
                    let size = unsafe { self.fd.get_kvm_run().__bindgen_anon_1.io.size as usize };
                    VcpuExit::PioOutBatch { port, size, data }
                }
                Ok(KExit::Shutdown) => VcpuExit::SystemReset,
                Ok(KExit::Debug(info)) => {
                    use crate::debug::{DebugReason, DebugStop};
                    let watch = (0..4)
                        .find(|&i| info.dr6 & (1 << i) != 0 && (info.dr7 >> (16 + i * 4)) & 3 != 0);
                    let reason = if info.exception == 3 {
                        DebugReason::SoftwareBreakpoint
                    } else if watch.is_some() {
                        DebugReason::Watchpoint
                    } else if info.dr6 & (1 << 14) != 0 {
                        DebugReason::SingleStep
                    } else {
                        DebugReason::Breakpoint
                    };
                    VcpuExit::Debug(DebugStop {
                        cpu: self.index,
                        pc: info.pc,
                        syndrome: info.dr6,
                        reason,
                        fault_address: None,
                    })
                }
                Ok(KExit::SystemEvent(ty, _)) => {
                    if ty == KVM_SYSTEM_EVENT_RESET {
                        VcpuExit::SystemReset
                    } else {
                        VcpuExit::SystemOff
                    }
                }
                Ok(KExit::Hlt) | Ok(KExit::Intr) => continue,
                Ok(KExit::InternalError) => {
                    let regs = self.fd.get_regs().map_err(kerr("KVM_GET_REGS"))?;
                    let sregs = self.fd.get_sregs().map_err(kerr("KVM_GET_SREGS"))?;
                    // SAFETY: KVM_EXIT_INTERNAL_ERROR selects the internal arm.
                    let internal = unsafe { self.fd.get_kvm_run().__bindgen_anon_1.internal };
                    return Err(Error::GuestFault(format!("KVM internal error {}: rip={:#x}, cs={:#x}, cr0={:#x}, cr3={:#x}, data={:x?}", internal.suberror, regs.rip, sregs.cs.base, sregs.cr0, sregs.cr3, &internal.data[..(internal.ndata as usize).min(internal.data.len())])));
                }
                Ok(other) => {
                    return Err(Error::GuestFault(format!(
                        "unexpected x86 KVM exit {other:?}"
                    )))
                }
                Err(e) if e.errno() == libc::EINTR || e.errno() == libc::EAGAIN => {
                    self.fd.set_kvm_immediate_exit(0);
                    continue;
                }
                Err(e) => return Err(kerr("KVM_RUN")(e)),
            };
            return Ok(exit);
        }
    }
    fn complete_mmio_read(&mut self, data: &[u8]) -> Result<()> {
        let Some(Read::Mmio(len)) = self.pending.take() else {
            return Err(state_error("no MMIO read pending"));
        };
        if data.len() != len {
            return Err(state_error("wrong MMIO length"));
        }
        // SAFETY: last exit was MMIO, data fits the eight-byte field.
        unsafe {
            self.fd.get_kvm_run().__bindgen_anon_1.mmio.data[..len].copy_from_slice(data);
        }
        Ok(())
    }
    fn complete_pio_in(&mut self, data: &[u8]) -> Result<()> {
        let Some(Read::Pio { offset, len }) = self.pending.take() else {
            return Err(state_error("no port read pending"));
        };
        if data.len() != len {
            return Err(state_error("wrong port read length"));
        }
        let run = self.fd.get_kvm_run() as *mut _ as *mut u8;
        // SAFETY: offset/len came from the IoIn slice of this live run mapping.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), run.add(offset), len);
        }
        Ok(())
    }
    fn complete_hypercall(&mut self, _: [u64; 4]) -> Result<()> {
        Err(Error::Unsupported("x86 hypercalls"))
    }
    fn set_boot_regs(&mut self, regs: &BootRegs) -> Result<()> {
        if matches!(regs, BootRegs::X86Reset) {
            return Ok(());
        }
        let BootRegs::X86_64 { rip, rsi, cr3, gdt } = *regs else {
            return Err(state_error("x86 boot registers required"));
        };
        let mut s = self.fd.get_sregs().map_err(kerr("KVM_GET_SREGS"))?;
        s.cr0 = 0x8000_0031;
        s.cr3 = cr3;
        s.cr4 = 0x20;
        s.efer = 0x500;
        s.gdt = kvm_dtable {
            base: gdt,
            limit: 23,
            ..Default::default()
        };
        let code = kvm_segment {
            limit: u32::MAX,
            selector: 8,
            type_: 11,
            present: 1,
            s: 1,
            l: 1,
            g: 1,
            ..Default::default()
        };
        let data = kvm_segment {
            selector: 16,
            type_: 3,
            l: 0,
            db: 1,
            ..code
        };
        s.cs = code;
        s.ds = data;
        s.es = data;
        s.fs = data;
        s.gs = data;
        s.ss = data;
        self.fd.set_sregs(&s).map_err(kerr("KVM_SET_SREGS"))?;
        self.fd
            .set_regs(&kvm_regs {
                rip,
                rsi,
                rflags: 2,
                ..Default::default()
            })
            .map_err(kerr("KVM_SET_REGS"))
    }
    fn save_state(&mut self) -> Result<VcpuState> {
        let entries: Vec<_> = self
            .msr_indices
            .iter()
            .map(|&index| kvm_msr_entry {
                index,
                ..Default::default()
            })
            .collect();
        let mut msrs = Msrs::from_entries(&entries).map_err(state_error)?;
        let count = self.fd.get_msrs(&mut msrs).map_err(kerr("KVM_GET_MSRS"))?;
        if count != entries.len() {
            return Err(state_error(format!(
                "MSR save stopped at {:#x} ({count}/{})",
                entries[count].index,
                entries.len()
            )));
        }
        let mp = self.fd.get_mp_state().map_err(kerr("KVM_GET_MP_STATE"))?;
        let s = CpuState {
            regs: self.fd.get_regs().map_err(kerr("KVM_GET_REGS"))?,
            sregs: self.fd.get_sregs().map_err(kerr("KVM_GET_SREGS"))?,
            xsave: self.fd.get_xsave().map_err(kerr("KVM_GET_XSAVE"))?,
            xcrs: self.fd.get_xcrs().map_err(kerr("KVM_GET_XCRS"))?,
            lapic: self.fd.get_lapic().map_err(kerr("KVM_GET_LAPIC"))?,
            mp,
            events: self
                .fd
                .get_vcpu_events()
                .map_err(kerr("KVM_GET_VCPU_EVENTS"))?,
            debug: self
                .fd
                .get_debug_regs()
                .map_err(kerr("KVM_GET_DEBUGREGS"))?,
            msrs: msrs.as_slice().to_vec(),
        };
        use crate::debug::{x86_state as S, X86Register as R};
        let r = &s.regs;
        let sr = &s.sregs;
        let mut debug_regs = vec![
            (R::Rax as u32, r.rax),
            (R::Rbx as u32, r.rbx),
            (R::Rcx as u32, r.rcx),
            (R::Rdx as u32, r.rdx),
            (R::Rsi as u32, r.rsi),
            (R::Rdi as u32, r.rdi),
            (R::Rbp as u32, r.rbp),
            (R::Rsp as u32, r.rsp),
            (R::R8 as u32, r.r8),
            (R::R9 as u32, r.r9),
            (R::R10 as u32, r.r10),
            (R::R11 as u32, r.r11),
            (R::R12 as u32, r.r12),
            (R::R13 as u32, r.r13),
            (R::R14 as u32, r.r14),
            (R::R15 as u32, r.r15),
            (R::Rip as u32, r.rip),
            (R::Rflags as u32, r.rflags),
            (R::Cr0 as u32, sr.cr0),
            (R::Cr2 as u32, sr.cr2),
            (R::Cr3 as u32, sr.cr3),
            (R::Cr4 as u32, sr.cr4),
            (R::Efer as u32, sr.efer),
            (S::CS_BASE, sr.cs.base),
            (S::GS_BASE, sr.gs.base),
            (S::IDT_BASE, sr.idt.base),
            (S::IDT_LIMIT, sr.idt.limit as u64),
            (S::CS_SELECTOR, sr.cs.selector as u64),
            (S::GS_SELECTOR, sr.gs.selector as u64),
        ];
        for &(index, id) in &[(0xc000_0082, S::LSTAR), (0xc000_0102, S::KERNEL_GS_BASE)] {
            if let Some(msr) = s.msrs.iter().find(|m| m.index == index) {
                debug_regs.push((id, msr.data));
            }
        }
        Ok(VcpuState {
            backend: "kvm-x86_64-v1".into(),
            regs: debug_regs,
            index: self.index,
            powered_on: mp.mp_state != KVM_MP_STATE_UNINITIALIZED,
            raw: vec![(0, serde_json::to_vec(&s).map_err(state_error)?)],
            ..Default::default()
        })
    }
    fn restore_state(&mut self, state: &VcpuState) -> Result<()> {
        if state.backend != "kvm-x86_64-v1"
            || state.index != self.index
            || state.raw.len() != 1
            || state.raw[0].0 != 0
        {
            return Err(state_error("wrong backend or CPU"));
        }
        let s: CpuState = serde_json::from_slice(&state.raw[0].1).map_err(state_error)?;
        self.fd.set_sregs(&s.sregs).map_err(kerr("KVM_SET_SREGS"))?;
        let msrs = Msrs::from_entries(&s.msrs).map_err(state_error)?;
        if self.fd.set_msrs(&msrs).map_err(kerr("KVM_SET_MSRS"))? != s.msrs.len() {
            return Err(state_error("short MSR restore"));
        }
        self.fd.set_xcrs(&s.xcrs).map_err(kerr("KVM_SET_XCRS"))?;
        self.fd.set_xsave(&s.xsave).map_err(kerr("KVM_SET_XSAVE"))?;
        self.fd.set_lapic(&s.lapic).map_err(kerr("KVM_SET_LAPIC"))?;
        self.fd.set_regs(&s.regs).map_err(kerr("KVM_SET_REGS"))?;
        self.fd
            .set_debug_regs(&s.debug)
            .map_err(kerr("KVM_SET_DEBUGREGS"))?;
        self.fd
            .set_vcpu_events(&s.events)
            .map_err(kerr("KVM_SET_VCPU_EVENTS"))?;
        self.fd
            .set_mp_state(s.mp)
            .map_err(kerr("KVM_SET_MP_STATE"))?;
        self.pending = None;
        Ok(())
    }
}

fn guest_debug(config: &crate::debug::DebugConfig) -> Result<kvm_guest_debug> {
    let mut d = kvm_guest_debug::default();
    if !config.enabled {
        return Ok(d);
    }
    if config.trap_brk_immediate.is_some() {
        return Err(Error::Unsupported("ARM64 BRK immediate on x86"));
    }
    if config.breakpoints.len() + config.watchpoints.len() > 4 {
        return Err(Error::Invalid(
            "x86 has four shared execution/data breakpoint slots".into(),
        ));
    }
    d.control = KVM_GUESTDBG_ENABLE | KVM_GUESTDBG_USE_HW_BP;
    if config.single_step {
        d.control |= KVM_GUESTDBG_SINGLESTEP;
    }
    if config.trap_software_breakpoints {
        d.control |= KVM_GUESTDBG_USE_SW_BP;
    }
    let mut dr7 = 0x400u64;
    for (i, &address) in config.breakpoints.iter().enumerate() {
        d.arch.debugreg[i] = address;
        dr7 |= 1 << (i * 2);
    }
    for (n, w) in config.watchpoints.iter().enumerate() {
        let i = config.breakpoints.len() + n;
        if !matches!(w.size, 1 | 2 | 4 | 8) || w.address % u64::from(w.size) != 0 || !w.write {
            return Err(Error::Invalid(
                "x86 watchpoints need aligned 1/2/4/8 bytes and write or read+write (no read-only)"
                    .into(),
            ));
        }
        let length = match w.size {
            1 => 0,
            2 => 1,
            4 => 3,
            8 => 2,
            _ => unreachable!(),
        };
        let access = if w.read { 3 } else { 1 };
        d.arch.debugreg[i] = w.address;
        dr7 |= 1 << (i * 2);
        dr7 |= (access | (length << 2)) << (16 + i * 4);
    }
    d.arch.debugreg[7] = dr7;
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn debug_slots_and_watch_encodings() {
        use crate::debug::{DebugConfig, Watchpoint};
        let mut c = DebugConfig {
            enabled: true,
            breakpoints: vec![0x1001],
            watchpoints: vec![Watchpoint {
                address: 0x2000,
                size: 8,
                read: true,
                write: true,
            }],
            ..Default::default()
        };
        let d = guest_debug(&c).unwrap();
        assert_eq!(&d.arch.debugreg[..2], &[0x1001, 0x2000]);
        assert_eq!((d.arch.debugreg[7] >> 20) & 15, 11);
        c.breakpoints = vec![1, 2, 3, 4];
        assert!(guest_debug(&c).is_err());
        c.breakpoints.clear();
        c.watchpoints[0].write = false;
        assert!(guest_debug(&c).is_err());
        c.watchpoints[0].write = true;
        c.watchpoints[0].address = 0x2001;
        assert!(guest_debug(&c).is_err());
        c.enabled = false;
        assert_eq!(guest_debug(&c).unwrap().control, 0);
    }
    #[test]
    fn pc_irq0_override_matches_routing() {
        let routes = pc_routes();
        assert_eq!(routes.nr, 40);
        for e in &routes.entries {
            // SAFETY: pc_routes only creates IRQCHIP entries.
            let chip = unsafe { e.u.irqchip };
            if chip.irqchip == KVM_IRQCHIP_IOAPIC {
                assert_eq!(
                    chip.pin,
                    match e.gsi {
                        0 => 2,
                        2 => 0,
                        n => n,
                    }
                );
            }
        }
        assert_eq!(
            std::mem::offset_of!(PcRoutes, entries),
            std::mem::size_of::<kvm_irq_routing>()
        );
    }
}

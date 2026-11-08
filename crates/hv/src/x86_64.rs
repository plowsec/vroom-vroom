//! Pure contracts for the first x86 KVM backend, testable on any host.
#![cfg_attr(not(all(target_os = "linux", target_arch = "x86_64")), allow(dead_code))]

use crate::{Error, Result};

// Keep KVM's four private pages below the top 4 MiB used by OVMF flash.
// The default identity-map address (0xfffb_c000) overlaps that firmware.
// This only reserves address space; it does not enable a firmware board.
pub(crate) const IDENTITY_MAP: u64 = 0xffbf_c000;
pub(crate) const TSS_BASE: u64 = IDENTITY_MAP + 0x1000;
// KVM's private identity-map page and three TSS pages, then the irqchip MMIO.
const RESERVED: [(u64, u64); 3] = [(IDENTITY_MAP, 0x4000), (0xfec0_0000, 0x1000), (0xfee0_0000, 0x1000)];

pub(crate) fn check_memory_range(start: u64, size: u64) -> Result<()> {
    let end = start.checked_add(size).ok_or_else(|| Error::Invalid("guest memory range overflow".into()))?;
    if size == 0 || start & 0xfff != 0 || size & 0xfff != 0 {
        return Err(Error::Invalid("x86 memory slots must contain aligned 4 KiB pages".into()));
    }
    if RESERVED.iter().any(|&(base, len)| start < base + len && base < end) {
        return Err(Error::Invalid("guest memory overlaps reserved KVM TSS, identity-map or irqchip pages".into()));
    }
    Ok(())
}

fn canonical(address: u64) -> bool {
    matches!(address >> 47, 0 | 0x1ffff)
}

pub(crate) fn validate_boot(rip: u64, rsi: u64, cr3: u64, gdt: u64) -> Result<()> {
    if !canonical(rip) || !canonical(rsi) || !canonical(gdt)
        || !gdt.checked_add(31).is_some_and(canonical) || gdt & 7 != 0
        || cr3 & 0xfff != 0 || cr3 >= 1 << 52 {
        return Err(Error::Invalid("invalid x86 Linux64 boot address, CR3 or GDT alignment".into()));
    }
    Ok(())
}

/// The observable fresh-KVM RESET contract, independent of Linux bindings.
/// See kvm_vcpu_reset(), vmx_vcpu_reset() and SVM init_vmcb() in Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResetSegment {
    pub base: u64,
    pub limit: u32,
    pub selector: u16,
    pub kind: u8,
    /// present, dpl, db, s, l, g, avl, unusable (unpacked KVM fields).
    pub attributes: [u8; 8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResetVectorState {
    /// RAX,RBX,RCX,RDX,RSI,RDI,RSP,RBP,R8..R15,RIP,RFLAGS.
    pub gprs: [u64; 18],
    /// CR0,CR2,CR3,CR4,CR8,EFER.
    pub controls: [u64; 6],
    /// CS,DS,ES,FS,GS,SS,TR,LDT.
    pub segments: [ResetSegment; 8],
    /// GDT and IDT: base, limit.
    pub tables: [(u64, u16); 2],
    pub interrupt_bitmap: [u64; 4],
    pub runnable: bool,
}

impl ResetVectorState {
    pub fn validate(&self, signature: Option<u32>) -> Result<()> {
        let bad = || Error::Invalid("vCPU is not in the required fresh x86 reset state".into());
        // KVM RESET happens before userspace installs CPUID, so its initial
        // RDX signature is not authoritative. Only that GPR is normalized.
        let mut gprs = [0; 18];
        gprs[3] = signature.map(u64::from).unwrap_or(self.gprs[3]);
        gprs[16] = 0xfff0;
        gprs[17] = 2;
        if self.gprs != gprs || self.gprs[3] > u64::from(u32::MAX)
            || self.controls != [0x6000_0010, 0, 0, 0, 0, 0]
            || self.tables != [(0, 0xffff); 2] || self.interrupt_bitmap != [0; 4]
            || !self.runnable {
            return Err(bad());
        }
        for (index, segment) in self.segments.iter().enumerate() {
            let (base, selector, system) = match index {
                0 => (0xffff_0000, 0xf000, false),
                6 | 7 => (0, 0, true),
                _ => (0, 0, false),
            };
            // VMX sets accessed data/code bits; SVM leaves them clear. KVM's
            // TR reset representation is busy32 on VMX and busy16 on SVM.
            let kind_ok = match index {
                0 => matches!(segment.kind, 10 | 11),
                6 => matches!(segment.kind, 3 | 11),
                7 => segment.kind == 2,
                _ => matches!(segment.kind, 2 | 3),
            };
            if !kind_ok || segment.base != base || segment.selector != selector
                || segment.limit != 0xffff
                || segment.attributes != [1, 0, 0, u8::from(!system), 0, 0, 0, 0] {
                return Err(bad());
            }
        }
        Ok(())
    }
}

/// A failed firmware initialization must not be retried or followed by RUN.
/// Existing Linux register-setting calls remain repeatable when no such
/// failure occurred; any boot/run attempt consumes firmware eligibility.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) enum ResetEntryGuard {
    #[default]
    Fresh,
    Used,
    Failed,
}

impl ResetEntryGuard {
    pub fn other_attempt(&mut self) -> Result<()> {
        if *self == Self::Failed {
            return Err(Error::Invalid("x86 firmware initialization failed; recreate the vCPU".into()));
        }
        *self = Self::Used;
        Ok(())
    }
}

pub(crate) trait ResetVectorCpu {
    fn read_reset_state(&mut self) -> Result<ResetVectorState>;
    fn set_reset_gprs(&mut self, signature: u32) -> Result<()>;
    fn set_runnable(&mut self) -> Result<()>;
}

pub(crate) fn initialize_reset_vector(
    guard: &mut ResetEntryGuard, cpu: &mut impl ResetVectorCpu, signature: u32,
) -> Result<()> {
    if *guard != ResetEntryGuard::Fresh {
        return Err(Error::Invalid("firmware reset-vector entry requires a fresh vCPU".into()));
    }
    // Consume eligibility before the first fallible read/write. No rollback
    // is claimed: a partially initialized vCPU must be destroyed.
    *guard = ResetEntryGuard::Failed;
    cpu.read_reset_state()?.validate(None)?;
    cpu.set_reset_gprs(signature)?;
    cpu.set_runnable()?;
    cpu.read_reset_state()?.validate(Some(signature))?;
    *guard = ResetEntryGuard::Used;
    Ok(())
}

pub(crate) fn scalar_pio(size: u8, count: u32, total: usize) -> Result<usize> {
    if count != 1 {
        return Err(Error::Unsupported("multi-element/string x86 port I/O"));
    }
    if !matches!(size, 1 | 2 | 4) || total != size as usize {
        return Err(Error::GuestFault("invalid scalar port I/O size".into()));
    }
    Ok(total)
}

pub(crate) fn mmio_len(len: usize) -> Result<()> {
    if !(1..=8).contains(&len) {
        return Err(Error::GuestFault("invalid MMIO size".into()));
    }
    Ok(())
}

/// Restrict KVM-derived registers to one CPU without guest VMX/SVM.
pub(crate) fn cpuid_registers(function: u32, index: u32, mut registers: [u32; 4]) -> [u32; 4] {
    let [eax, ebx, ecx, edx] = &mut registers;
    match function {
        1 => {
            *ecx &= !(1 << 5); // VMX
            *edx &= !(1 << 28); // HTT
            *ebx = (*ebx & 0xffff) | (1 << 16); // one logical CPU, APIC id 0
        }
        4 | 0x8000_001d => *eax &= 0x3fff,
        0xb | 0x1f => {
            *eax = 0;
            *ebx = u32::from(index < 2);
            *ecx = index | if index < 2 { (index + 1) << 8 } else { 0 };
            *edx = 0;
        }
        0x8000_0001 => *ecx &= !(1 << 2), // SVM
        0x8000_0008 => *ecx &= !0xf0ff,
        0x8000_000a => { *eax = 0; *ebx = 0; *ecx = 0; *edx = 0; }
        0x8000_001e => { *eax = 0; *ebx &= !0xffff; *ecx &= !0x7ff; }
        _ => {}
    }
    registers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reset_state() -> ResetVectorState {
        let data = ResetSegment { base: 0, limit: 0xffff, selector: 0, kind: 3,
            attributes: [1, 0, 0, 1, 0, 0, 0, 0] };
        let mut segments = [data; 8];
        segments[0] = ResetSegment { base: 0xffff_0000, selector: 0xf000, kind: 11, ..data };
        segments[6] = ResetSegment { kind: 11, attributes: [1, 0, 0, 0, 0, 0, 0, 0], ..data };
        segments[7] = ResetSegment { kind: 2, ..segments[6] };
        let mut gprs = [0; 18];
        gprs[3] = 0x600; gprs[16] = 0xfff0; gprs[17] = 2;
        ResetVectorState { gprs, controls: [0x6000_0010, 0, 0, 0, 0, 0], segments,
            tables: [(0, 0xffff); 2], interrupt_bitmap: [0; 4], runnable: true }
    }

    struct ResetCpu {
        state: ResetVectorState,
        calls: Vec<&'static str>,
        fail_at: usize,
        discard_gprs: bool,
    }
    impl ResetCpu {
        fn new() -> Self {
            Self { state: reset_state(), calls: Vec::new(), fail_at: 0, discard_gprs: false }
        }
        fn call(&mut self, name: &'static str) -> Result<()> {
            self.calls.push(name);
            if self.calls.len() == self.fail_at { return Err(Error::Invalid("injected reset I/O failure".into())); }
            Ok(())
        }
    }
    impl ResetVectorCpu for ResetCpu {
        fn read_reset_state(&mut self) -> Result<ResetVectorState> {
            self.call("read")?;
            Ok(self.state.clone())
        }
        fn set_reset_gprs(&mut self, signature: u32) -> Result<()> {
            self.call("gprs")?;
            if !self.discard_gprs { self.state.gprs[3] = u64::from(signature); }
            Ok(())
        }
        fn set_runnable(&mut self) -> Result<()> {
            self.call("runnable")?;
            self.state.runnable = true;
            Ok(())
        }
    }

    #[test]
    fn firmware_reset_state_is_real_mode_at_the_top_of_32_bit_space() {
        let mut state = reset_state();
        state.validate(None).unwrap();
        assert_eq!(state.segments[0].base + state.gprs[16], 0xffff_fff0);
        state.gprs[3] = 0x806f8;
        state.validate(Some(0x806f8)).unwrap();
        assert!(state.validate(Some(0x906f0)).is_err());
        // KVM's documented SVM reset encodings also satisfy the contract.
        state.segments[0].kind = 10;
        for segment in &mut state.segments[1..6] { segment.kind = 2; }
        state.segments[6].kind = 3;
        state.validate(Some(0x806f8)).unwrap();
    }

    #[test]
    fn altered_reset_registers_segments_tables_and_pending_events_are_rejected() {
        let original = reset_state();
        for index in 0..18 {
            if index == 3 { continue; }
            let mut state = original.clone(); state.gprs[index] ^= 1;
            assert!(state.validate(None).is_err(), "GPR {index}");
        }
        let mut state = original.clone(); state.gprs[3] = 1 << 32;
        assert!(state.validate(None).is_err());
        for index in 0..6 {
            let mut state = original.clone(); state.controls[index] ^= 1;
            assert!(state.validate(None).is_err(), "control {index}");
        }
        for index in 0..8 {
            for field in 0..12 {
                let mut state = original.clone();
                let segment = &mut state.segments[index];
                match field {
                    0 => segment.base ^= 1,
                    1 => segment.selector ^= 1,
                    2 => segment.limit ^= 1,
                    3 => segment.kind = 0,
                    _ => segment.attributes[field - 4] ^= 1,
                }
                assert!(state.validate(None).is_err(), "segment {index} field {field}");
            }
        }
        for index in 0..2 {
            let mut state = original.clone(); state.tables[index].0 = 1;
            assert!(state.validate(None).is_err());
            let mut state = original.clone(); state.tables[index].1 = 0;
            assert!(state.validate(None).is_err());
        }
        for index in 0..4 {
            let mut state = original.clone(); state.interrupt_bitmap[index] = 1;
            assert!(state.validate(None).is_err());
        }
        let mut state = original; state.runnable = false;
        assert!(state.validate(None).is_err());
    }

    #[test]
    fn firmware_entry_is_once_only_with_verified_readback() {
        let mut guard = ResetEntryGuard::default();
        let mut cpu = ResetCpu::new();
        let prior = cpu.state.clone();
        initialize_reset_vector(&mut guard, &mut cpu, 0x806f8).unwrap();
        assert_eq!(cpu.calls, ["read", "gprs", "runnable", "read"]);
        assert_eq!(cpu.state.gprs[3], 0x806f8);
        assert_eq!(cpu.state.controls, prior.controls);
        assert_eq!(cpu.state.segments, prior.segments);
        assert_eq!(guard, ResetEntryGuard::Used);
        assert!(initialize_reset_vector(&mut guard, &mut cpu, 0x806f8).is_err());
        assert_eq!(cpu.calls.len(), 4);
        guard.other_attempt().unwrap(); // normal execution remains available
    }

    #[test]
    fn failed_firmware_entry_cannot_retry_run_or_switch_to_linux() {
        for fail_at in 1..=4 {
            let mut guard = ResetEntryGuard::default();
            let mut cpu = ResetCpu::new(); cpu.fail_at = fail_at;
            assert!(initialize_reset_vector(&mut guard, &mut cpu, 0x806f8).is_err());
            assert_eq!(cpu.calls.len(), fail_at);
            assert_eq!(guard, ResetEntryGuard::Failed);
            assert!(guard.other_attempt().is_err());
            assert!(initialize_reset_vector(&mut guard, &mut cpu, 0x806f8).is_err());
            assert_eq!(cpu.calls.len(), fail_at);
        }
        for discard in [false, true] {
            let mut guard = ResetEntryGuard::default();
            let mut cpu = ResetCpu::new();
            if discard { cpu.discard_gprs = true; } else { cpu.state.controls[0] |= 1; }
            assert!(initialize_reset_vector(&mut guard, &mut cpu, 0x806f8).is_err());
            assert_eq!(cpu.calls.len(), if discard { 4 } else { 1 });
            assert!(guard.other_attempt().is_err());
            assert!(initialize_reset_vector(&mut guard, &mut cpu, 0x806f8).is_err());
        }
    }

    #[test]
    fn any_other_boot_or_run_attempt_consumes_only_firmware_eligibility() {
        let mut guard = ResetEntryGuard::default();
        guard.other_attempt().unwrap(); // called before a fallible boot/RUN
        guard.other_attempt().unwrap(); // existing Linux operations stay repeatable
        let mut cpu = ResetCpu::new();
        assert!(initialize_reset_vector(&mut guard, &mut cpu, 0x806f8).is_err());
        assert!(cpu.calls.is_empty());
        guard.other_attempt().unwrap();
    }

    #[test]
    fn reserved_memory_cannot_be_mapped() {
        assert!(check_memory_range(0, 2 << 20).is_ok());
        for (base, len) in RESERVED {
            assert!(check_memory_range(base, 0x1000).is_err());
            assert!(check_memory_range(base - 0x1000, len + 0x2000).is_err());
            assert!(check_memory_range(base - 0x1000, 0x1000).is_ok());
            assert!(check_memory_range(base + len, 0x1000).is_ok());
        }
        assert!(check_memory_range(u64::MAX - 0xfff, 0x1000).is_err());
        assert!(check_memory_range(1, 0x1000).is_err());
    }

    #[test]
    fn selected_ovmf_code_and_vars_fit_above_kvm_private_pages() {
        // QEMU 11.1.2's edk2-x86_64-secure-code.fd and edk2-i386-vars.fd.
        // Pure layout fixture: no firmware files are read or executed.
        let top = 1u64 << 32;
        let code_size = 0x37c000;
        let vars_size = 0x84000;
        let code_base = top - code_size;
        let vars_base = code_base - vars_size;
        assert_eq!(code_size + vars_size, 4 << 20);
        assert_eq!(code_base, 0xffc8_4000);
        assert_eq!(vars_base, 0xffc0_0000);
        assert_eq!(IDENTITY_MAP + 0x4000, vars_base);
        assert_eq!(TSS_BASE + 3 * 0x1000, vars_base);
        assert!(check_memory_range(code_base, code_size).is_ok());
        assert!(check_memory_range(vars_base, vars_size).is_ok());
        // The former private-page addresses are now available for CODE.
        assert!(check_memory_range(0xfffb_c000, 0x4000).is_ok());
    }

    #[test]
    fn ram_cannot_overlap_the_relocated_identity_or_tss_pages() {
        assert!(check_memory_range(0, 512 << 20).is_ok());
        assert!(check_memory_range(IDENTITY_MAP - 0x1000, 0x1000).is_ok());
        assert!(check_memory_range(IDENTITY_MAP - 0x1000, 0x2000).is_err());
        for page in 0..4 {
            assert!(check_memory_range(IDENTITY_MAP + page * 0x1000, 0x1000).is_err());
        }
        assert!(check_memory_range(IDENTITY_MAP, 0x4000).is_err());
        assert!(check_memory_range(TSS_BASE + 0x2000, 0x2000).is_err());
        assert!(check_memory_range(IDENTITY_MAP + 0x4000, 0x1000).is_ok());
        assert_eq!(IDENTITY_MAP & 0xfff, 0);
        assert!(TSS_BASE + 0x3000 < 1 << 32);
    }

    #[test]
    fn boot_addresses_match_the_48_bit_linux64_contract() {
        assert!(validate_boot(0x10000, 0x8000, 0x1000, 0x5000).is_ok());
        assert!(validate_boot(0xffff_ffff_8100_0000, 0, 0, 0xffff_ffff_8200_0000).is_ok());
        assert!(validate_boot(0x0000_8000_0000_0000, 0, 0, 0).is_err());
        assert!(validate_boot(0, 0, 0x1001, 0).is_err());
        assert!(validate_boot(0, 0, 1 << 52, 0).is_err());
        assert!(validate_boot(0, 0, 0, u64::MAX - 7).is_err());
    }

    #[test]
    fn pio_never_truncates_a_string_operation() {
        assert_eq!(scalar_pio(4, 1, 4).unwrap(), 4);
        assert!(scalar_pio(1, 4, 4).is_err());
        assert!(scalar_pio(2, 2, 4).is_err());
        assert!(scalar_pio(1, 0, 0).is_err());
        assert!(scalar_pio(8, 1, 8).is_err());
        assert!(scalar_pio(2, 1, 1).is_err());
        assert!(mmio_len(0).is_err());
        assert!(mmio_len(9).is_err());
    }

    #[test]
    fn cpu_policy_hides_guest_virtualization_and_extra_processors() {
        let input = [0x123, u32::MAX, (1 << 5) | 1, (1 << 28) | (1 << 6)];
        assert_eq!(cpuid_registers(1, 0, input), [0x123, 0x1ffff, 1, 1 << 6]);
        assert_eq!(cpuid_registers(0x8000_0001, 0, [0, 0, 7, 1 << 29]), [0, 0, 3, 1 << 29]);
        assert_eq!(cpuid_registers(0x8000_000a, 0, [u32::MAX; 4]), [0; 4]);
        assert_eq!(cpuid_registers(0xb, 0, [u32::MAX; 4]), [0, 1, 0x100, 0]);
        assert_eq!(cpuid_registers(0x1f, 1, [u32::MAX; 4]), [0, 1, 0x201, 0]);
        assert_eq!(cpuid_registers(0x1f, 2, [u32::MAX; 4]), [0, 0, 2, 0]);
        assert_eq!(cpuid_registers(7, 0, input), input, "other supported leaves stay unchanged");
    }
}

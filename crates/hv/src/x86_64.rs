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

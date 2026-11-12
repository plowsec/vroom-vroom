//! Fixed Q35 topology for the selected 4 MiB, SMM-requiring OVMF image pair.
//!
//! Firmware sees nominal physical RAM, including CPU-private SMRAM backing.
//! Devices see only [`Q35FirmwareLayout::ordinary_ram`]. OVMF subtracts TSEG
//! and reserves default-SMBASE itself; passing the device view as `etc/e820`
//! would subtract TSEG twice. These files are NOT an OS usable-memory map.
//!
//! No mapping, firmware authentication, board enablement or guest execution
//! happens here. See `crates/linux-harness/FIRMWARE_LAYOUT.md` for the pinned
//! firmware consumers and the real-device integration test.

use crate::{Error, Result};

const LOW_RAM_END: u64 = 0x2000_0000;
const DEFAULT_SMBASE: (u64, u64) = (0x3_0000, 0x2_0000);
const TSEG: (u64, u64) = (LOW_RAM_END - (8 << 20), 8 << 20);
const LEGACY_HOLE: (u64, u64) = (0xa_0000, 0x6_0000);
const FLASH_BASE: u64 = 0xffc0_0000;
const FLASH_SIZE: u64 = 4 << 20;
const VARS_SIZE: usize = 540_672;
const CODE_SIZE: usize = 3_653_632;
const KVM_PRIVATE: (u64, u64) = (FLASH_BASE - 0x4000, 0x4000);
const ECAM: (u64, u64) = (0xe000_0000, 0x1000_0000);
const BAR_WINDOWS: [(u64, u64); 2] = [
    (0x8000_0000, 0x6000_0000),
    (0x8_0000_0000, 0x8_0000_0000),
];

/// Immutable one-CPU, nominal 512 MiB reference topology. All range getters
/// return `(base, size)` in bytes, never an inclusive end. The constructor
/// checks image geometry only; the future board must own and authenticate the
/// selected CODE/VARS bytes before mapping or starting helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Q35FirmwareLayout { _private: () }

impl Q35FirmwareLayout {
    pub fn for_images(code_len: usize, vars_len: usize) -> Result<Self> {
        if code_len.checked_add(vars_len) != Some(FLASH_SIZE as usize)
            || code_len != CODE_SIZE || vars_len != VARS_SIZE {
            return Err(Error::Layout("Q35 firmware requires the selected 3653632-byte CODE and 540672-byte VARS pair".into()));
        }
        Ok(Self { _private: () })
    }

    pub fn nominal_ram_bytes(&self) -> u64 { LOW_RAM_END }
    pub fn cpu_count(&self) -> u16 { 1 }

    /// The only ranges to place in GuestMemory or expose to device DMA.
    /// Private SMRAM stays absent even while its AS0 CPU view is open.
    pub fn ordinary_ram(&self) -> [(u64, u64); 3] {
        [(0, DEFAULT_SMBASE.0),
         (DEFAULT_SMBASE.0 + DEFAULT_SMBASE.1, LEGACY_HOLE.0 - DEFAULT_SMBASE.0 - DEFAULT_SMBASE.1),
         (0x10_0000, TSEG.0 - 0x10_0000)]
    }

    /// Fixed backend-owned ranges, never GuestMemory, host shares or DMA.
    pub fn default_smbase(&self) -> (u64, u64) { DEFAULT_SMBASE }
    pub fn tseg(&self) -> (u64, u64) { TSEG }
    pub fn code(&self) -> (u64, u64) { (FLASH_BASE + VARS_SIZE as u64, CODE_SIZE as u64) }
    pub fn vars(&self) -> (u64, u64) { (FLASH_BASE, VARS_SIZE as u64) }
    pub fn kvm_private(&self) -> (u64, u64) { KVM_PRIVATE }
    pub fn ecam(&self) -> (u64, u64) { ECAM }
    pub fn bar_windows(&self) -> [(u64, u64); 2] { BAR_WINDOWS }

    /// Permanent BAR exclusions. A reservation does not advertise a working
    /// device (in particular HPET), nor implement legacy PAM/shadow RAM.
    pub fn reserved_ranges(&self) -> [(u64, u64); 10] {
        [DEFAULT_SMBASE, TSEG, LEGACY_HOLE, ECAM,
         (FLASH_BASE, FLASH_SIZE), KVM_PRIVATE,
         (0xfec0_0000, 0x1000), // IOAPIC
         (0xfed0_0000, 0x400), // HPET reservation only
         (0xfed1_c000, 0x4000), // ICH9 root-complex register block
         (0xfee0_0000, 0x10_0000)] // LAPIC reservation
    }

    /// CMOS register/value pairs for the later RTC consumer: memory above
    /// 16 MiB in 64 KiB units, then above 4 GiB (none). This is nominal memory,
    /// not the smaller sum of ordinary_ram(). No RTC implementation is implied.
    pub fn cmos_memory(&self) -> [(u8, u8); 5] {
        let above_16m = ((LOW_RAM_END - (16 << 20)) >> 16) as u16;
        let [lo, hi] = above_16m.to_le_bytes();
        [(0x34, lo), (0x35, hi), (0x5b, 0), (0x5c, 0), (0x5d, 0)]
    }

    /// Firmware-only `etc/e820`: one packed LE address/length/type entry.
    /// QEMU publishes nominal physical low RAM; pinned OVMF reserves both
    /// private areas when it creates memory HOBs. Do not pass this to Linux's
    /// direct boot loader or report it as an OS usable-memory map.
    pub fn firmware_e820(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(20);
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&LOW_RAM_END.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes()); // E820_RAM
        bytes
    }

    /// Exact `etc/hardware-info` host-bridge record consumed by pinned OVMF.
    /// It binds PCI allocation to the MCH router's two actual BAR windows.
    /// ECAM is independently decoded at the selected firmware's e0000000 PCD;
    /// its descriptor pair is absent because both selected consumers ignore
    /// that parser output (a nonempty pair would produce a tolerated warning).
    /// See the protocol tests and contract.
    pub fn hardware_info(&self) -> Vec<u8> {
        let mut bytes = vec![0; 16 + 120];
        bytes[..8].copy_from_slice(&1u64.to_le_bytes()); // HardwareInfoTypeHostBridge
        bytes[8..16].copy_from_slice(&120u64.to_le_bytes());
        let record = &mut bytes[16..];
        // Version=0, Attributes=0, reserved/padding=0. CombineMemPMem only:
        // DMA stays below4GiB; extended config space is actually supported.
        record[16..20].copy_from_slice(&4u32.to_le_bytes());
        record[21] = 255; // bus range 0..255
        for (offset, (base, size)) in [(40, BAR_WINDOWS[0]), (72, BAR_WINDOWS[1])] {
            record[offset..offset + 8].copy_from_slice(&base.to_le_bytes());
            record[offset + 8..offset + 16].copy_from_slice(&size.to_le_bytes());
        }
        // IO, PMem, PMemAbove4G and PcieConfig remain absent, size zero.
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlaps((a, n): (u64, u64), (b, m): (u64, u64)) -> bool { a < b + m && b < a + n }

    #[test]
    fn image_pair_is_exact_and_overflow_safe() {
        assert!(Q35FirmwareLayout::for_images(CODE_SIZE, VARS_SIZE).is_ok());
        for (code, vars) in [(0, 0), (VARS_SIZE, CODE_SIZE), (CODE_SIZE - 1, VARS_SIZE + 1),
            (CODE_SIZE - 4096, VARS_SIZE + 4096), (CODE_SIZE, VARS_SIZE - 1),
            (usize::MAX, 1), (usize::MAX, usize::MAX)] {
            assert!(Q35FirmwareLayout::for_images(code, vars).is_err());
        }
    }

    #[test]
    fn inventories_are_disjoint_and_cover_the_fixed_physical_topology() {
        let l = Q35FirmwareLayout::for_images(CODE_SIZE, VARS_SIZE).unwrap();
        let ordinary = l.ordinary_ram();
        assert_eq!(ordinary.iter().map(|(_, size)| size).sum::<u64>(), 527_958_016);
        let mut low = ordinary.to_vec();
        low.extend([l.default_smbase(), l.tseg(), LEGACY_HOLE]);
        low.sort_unstable();
        let mut cursor = 0;
        for (base, size) in low { assert_eq!(base, cursor); cursor += size; }
        assert_eq!(cursor, l.nominal_ram_bytes());
        let mut exclusive = ordinary.to_vec();
        exclusive.extend(l.reserved_ranges());
        exclusive.extend(l.bar_windows());
        for (i, range) in exclusive.iter().enumerate() {
            assert!(range.1 > 0 && range.0.checked_add(range.1).is_some());
            for other in &exclusive[..i] { assert!(!overlaps(*range, *other), "{range:x?} / {other:x?}"); }
        }
        assert_eq!(l.kvm_private().0 + l.kvm_private().1, l.vars().0);
        assert_eq!(l.vars().0 + l.vars().1, l.code().0);
        assert_eq!(l.code().0 + l.code().1, 1 << 32);
        for range in [l.default_smbase(), l.tseg(), l.code(), l.vars(), l.kvm_private()] {
            assert_eq!(range.0 % 4096, 0); assert_eq!(range.1 % 4096, 0);
        }
        assert_eq!(l.cmos_memory(), [(0x34, 0), (0x35, 0x1f), (0x5b, 0), (0x5c, 0), (0x5d, 0)]);
    }
}

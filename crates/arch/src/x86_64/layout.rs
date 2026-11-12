use crate::{Error, Result};

pub const LOW_RAM_END: u64 = 0xc000_0000;
pub const HIGH_RAM_BASE: u64 = 0x1_0000_0000;
pub const PCI_MMIO_BASE: u64 = LOW_RAM_END;
pub const PCI_MMIO_SIZE: u64 = 0x2000_0000;
pub const PCI_ECAM_BASE: u64 = 0xe000_0000;
pub const PCI_ECAM_SIZE: u64 = 0x1000_0000;
pub const PCI_MMIO64_SIZE: u64 = 32 << 30;
pub const RAM_MAX: u64 = 447 << 30;

/// Match OVMF's 32 GiB aperture aligned above the top of guest RAM.
pub fn pci_mmio64(ram_size: u64) -> (u64, u64) {
    let ram_end = if ram_size > LOW_RAM_END {
        HIGH_RAM_BASE + ram_size - LOW_RAM_END
    } else {
        ram_size
    };
    (
        ram_end.max(HIGH_RAM_BASE).next_multiple_of(PCI_MMIO64_SIZE),
        PCI_MMIO64_SIZE,
    )
}
pub const IOAPIC_BASE: u64 = 0xfec0_0000;
pub const LAPIC_BASE: u64 = 0xfee0_0000;
pub const TPM_BASE: u64 = 0xfed4_0000;
pub const UART_PORT: u64 = 0x3f8;
pub const UART_IRQ: u32 = 4;
pub const PCI_IRQ_BASE: u32 = 16;
pub const PM_BASE: u16 = 0xb000;
pub const FW_CFG_PORT: u64 = 0x510;
pub const MAX_CPUS: u32 = 254;

/// Backed memory excludes the legacy VGA/ROM hole. The allocation size
/// includes that hole, just like the PC firmware's RAM size accounting.
pub fn ram_regions(size: u64) -> Result<Vec<(u64, u64)>> {
    if !((64 << 20)..=RAM_MAX).contains(&size) {
        return Err(Error::Layout(
            "x86 RAM must be between 64 MiB and 447 GiB".into(),
        ));
    }
    let mut ram = vec![
        (0, 0xa0000),
        (0xf0000, 0x10000),
        (0x100000, size.min(LOW_RAM_END) - 0x100000),
    ];
    if size > LOW_RAM_END {
        ram.push((HIGH_RAM_BASE, size - LOW_RAM_END));
    }
    Ok(ram)
}

/// Packed fw_cfg etc/e820 entries: address, length, type (all LE).
pub fn e820(size: u64) -> Result<Vec<u8>> {
    let mut entries = Vec::new();
    let mut add = |base: u64, len: u64, kind: u32| {
        entries.extend(base.to_le_bytes());
        entries.extend(len.to_le_bytes());
        entries.extend(kind.to_le_bytes());
    };
    for (base, len) in ram_regions(size)? {
        if base != 0xf0000 {
            add(base, len, 1);
        }
    }
    add(0xa0000, 0x60000, 2);
    Ok(entries)
}

/// PC firmware expects the base-zero RAM record to describe all low
/// memory, including the legacy hole it reserves itself. Older OVMF
/// builds use that record's length directly when sizing permanent PEI RAM.
pub fn firmware_e820(size: u64) -> Result<Vec<u8>> {
    ram_regions(size)?;
    let mut entries = Vec::new();
    for (base, len) in [
        (0u64, size.min(LOW_RAM_END)),
        (HIGH_RAM_BASE, size.saturating_sub(LOW_RAM_END)),
    ] {
        if len == 0 {
            continue;
        }
        entries.extend(base.to_le_bytes());
        entries.extend(len.to_le_bytes());
        entries.extend(1u32.to_le_bytes());
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pci_aperture_tracks_top_of_ram() {
        assert_eq!(pci_mmio64(512 << 20), (32 << 30, 32 << 30));
        assert_eq!(pci_mmio64(64 << 30), (96 << 30, 32 << 30));
        for size in [512 << 20, 31 << 30, 32 << 30, 64 << 30, RAM_MAX] {
            let (base, len) = pci_mmio64(size);
            let ram_end = ram_regions(size)
                .unwrap()
                .into_iter()
                .map(|(a, n)| a + n)
                .max()
                .unwrap();
            assert!(base >= ram_end);
            assert_eq!(base % len, 0);
            assert!(base + len <= 1 << 39);
        }
    }
    #[test]
    fn firmware_low_memory_record_includes_legacy_hole() {
        for size in [512 << 20, 4 << 30] {
            let fw = firmware_e820(size).unwrap();
            assert_eq!(u64::from_le_bytes(fw[..8].try_into().unwrap()), 0);
            assert_eq!(
                u64::from_le_bytes(fw[8..16].try_into().unwrap()),
                size.min(LOW_RAM_END)
            );
            assert_eq!(u32::from_le_bytes(fw[16..20].try_into().unwrap()), 1);
        }
    }
    #[test]
    fn ram_avoids_device_windows() {
        assert_eq!(
            ram_regions(4 << 30).unwrap(),
            vec![
                (0, 0xa0000),
                (0xf0000, 0x10000),
                (0x100000, LOW_RAM_END - 0x100000),
                (HIGH_RAM_BASE, 1 << 30)
            ]
        );
        assert!(ram_regions(0).is_err());
        for (base, len) in ram_regions(8 << 30).unwrap() {
            assert!(base + len <= LOW_RAM_END || base >= HIGH_RAM_BASE);
        }
    }
}

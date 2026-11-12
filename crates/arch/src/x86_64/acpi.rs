use super::pc_layout::*;
use crate::acpi::aml::{self, res};
use crate::acpi::{assemble_pc, AcpiBlobs, AcpiIds, Table};

pub const RSDP_ADDR: u64 = 0xf0000;

/// Direct Linux boot has no firmware to execute the fw_cfg linker.
/// Relocate our generated tables into the reserved F-segment instead.
pub fn install(mem: &vroom_memory::GuestMemory, blobs: &AcpiBlobs) -> crate::Result<()> {
    use crate::acpi::{checksum, RSDP_FILE, TABLES_FILE};
    use vroom_memory::GuestAddress;
    let mut files = [
        (RSDP_FILE, blobs.rsdp.clone(), RSDP_ADDR),
        (TABLES_FILE, blobs.tables.clone(), RSDP_ADDR + 4096),
    ];
    let bad = || crate::Error::Layout("invalid x86 ACPI linker command".into());
    if blobs.tables.len() > 0xf000 {
        return Err(bad());
    }
    for c in blobs.loader.chunks_exact(128) {
        let u32_at = |n| u32::from_le_bytes([c[n], c[n + 1], c[n + 2], c[n + 3]]) as usize;
        let name = |n| {
            String::from_utf8_lossy(&c[n..n + 56])
                .trim_end_matches('\0')
                .to_string()
        };
        match u32_at(0) {
            1 => {} // fixed allocations above
            2 => {
                let src = files.iter().find(|f| f.0 == name(60)).ok_or_else(bad)?.2;
                let dst = files.iter_mut().find(|f| f.0 == name(4)).ok_or_else(bad)?;
                let off = u32_at(116);
                if c[120] != 8 {
                    return Err(bad());
                }
                let bytes = dst.1.get_mut(off..off + 8).ok_or_else(bad)?;
                let old = u64::from_le_bytes(bytes.try_into().map_err(|_| bad())?);
                bytes.copy_from_slice(&old.checked_add(src).ok_or_else(bad)?.to_le_bytes());
            }
            3 => {
                let dst = files.iter_mut().find(|f| f.0 == name(4)).ok_or_else(bad)?;
                let (at, start, len) = (u32_at(60), u32_at(64), u32_at(68));
                *dst.1.get_mut(at).ok_or_else(bad)? = 0;
                let sum = checksum(dst.1.get(start..start + len).ok_or_else(bad)?);
                dst.1[at] = sum;
            }
            _ => return Err(bad()),
        }
    }
    for (_, bytes, base) in files {
        mem.write(GuestAddress(base), &bytes)?;
    }
    Ok(())
}

pub fn build(cpus: u32, tpm: bool, ids: &AcpiIds, ram_size: u64) -> AcpiBlobs {
    let (pci64_base, pci64_size) = pci_mmio64(ram_size);
    let mut fadt = Table::new(b"FACP", 6);
    fadt.zeros(276 - 36);
    let mut f = fadt.finish();
    f[46..48].copy_from_slice(&9u16.to_le_bytes()); // SCI
    f[56..60].copy_from_slice(&(PM_BASE as u32).to_le_bytes());
    f[64..68].copy_from_slice(&(PM_BASE as u32 + 4).to_le_bytes());
    f[76..80].copy_from_slice(&(PM_BASE as u32 + 8).to_le_bytes());
    f[88] = 4;
    f[89] = 2;
    f[91] = 4;
    f[109..111].copy_from_slice(&1u16.to_le_bytes()); // legacy devices, no 8042
    f[112..116].copy_from_slice(&((1u32 << 10) | (1 << 8) | (1 << 5)).to_le_bytes()); // reset, 32-bit timer, fixed sleep button
    f[116..128].copy_from_slice(&[1, 8, 0, 1, 0xf9, 0x0c, 0, 0, 0, 0, 0, 0]);
    f[128] = 6;
    f[131] = 3;
    // ACPI 5+ consumers (including Windows' early HAL) use the extended
    // GAS fields instead of falling back to the legacy port addresses.
    // Keep both descriptions consistent for the same fixed registers.
    for (offset, width, access, port) in [
        (148, 32, 2, PM_BASE),     // X_PM1a_EVT_BLK: status + enable
        (172, 16, 2, PM_BASE + 4), // X_PM1a_CNT_BLK
        (208, 32, 3, PM_BASE + 8), // X_PM_TMR_BLK
    ] {
        f[offset..offset + 4].copy_from_slice(&[1, width, 0, access]);
        f[offset + 4..offset + 12].copy_from_slice(&(port as u64).to_le_bytes());
    }
    f[268..276].copy_from_slice(&ids.hypervisor_id);
    let mut madt = Table::new(b"APIC", 5);
    madt.u32(LAPIC_BASE as u32).u32(1);
    for i in 0..cpus {
        madt.u8(0).u8(8).u8(i as u8).u8(i as u8).u32(1);
    }
    madt.u8(1)
        .u8(12)
        .u8(cpus as u8)
        .u8(0)
        .u32(IOAPIC_BASE as u32)
        .u32(0);
    // The backend explicitly wires IRQ0 to IOAPIC pin 2.
    madt.u8(2).u8(10).u8(0).u8(0).u32(2).u16(0);
    madt.u8(2).u8(10).u8(0).u8(9).u32(9).u16(0x0f);
    madt.u8(4).u8(6).u8(0xff).u16(0).u8(1); // LINT1 NMI
    let mut mcfg = Table::new(b"MCFG", 1);
    mcfg.u64(0).u64(PCI_ECAM_BASE).u16(0).u8(0).u8(255).u32(0);
    let mut sb = Vec::new();
    for i in 0..cpus {
        sb.push(aml::device(
            &format!("C{i:03X}"),
            vec![
                aml::name("_HID", aml::string("ACPI0007")),
                aml::name("_UID", aml::int(i as u64)),
            ],
        ));
    }
    let mut prt = Vec::new();
    for slot in 0..32u64 {
        for pin in 0..4u64 {
            prt.push(aml::package(vec![
                aml::int((slot << 16) | 0xffff),
                aml::int(pin),
                aml::int(0),
                aml::int(PCI_IRQ_BASE as u64 + ((slot + pin) % 4)),
            ]));
        }
    }
    sb.push(aml::device(
        "PCI0",
        vec![
            aml::name("_HID", aml::eisa_id("PNP0A08")),
            aml::name("_CID", aml::eisa_id("PNP0A03")),
            aml::name("_UID", aml::int(0)),
            aml::name("_SEG", aml::int(0)),
            aml::name("_BBN", aml::int(0)),
            aml::name("_PRT", aml::package(prt)),
            aml::name(
                "_CRS",
                aml::resource_template(vec![
                    res::word_bus(0, 255),
                    res::dword_memory(PCI_MMIO_BASE as u32, PCI_MMIO_SIZE as u32),
                    res::qword_memory(pci64_base, pci64_size),
                    res::dword_io(0x1000, 0xa000, 0),
                    res::dword_io(0xb040, 0x4fc0, 0),
                ]),
            ),
        ],
    ));
    sb.push(aml::device(
        "RES0",
        vec![
            aml::name("_HID", aml::eisa_id("PNP0C02")),
            aml::name(
                "_CRS",
                aml::resource_template(vec![
                    res::memory32_fixed(PCI_ECAM_BASE as u32, PCI_ECAM_SIZE as u32, true),
                    res::memory32_fixed(IOAPIC_BASE as u32, 4096, true),
                    res::memory32_fixed(LAPIC_BASE as u32, 4096, true),
                    vec![0x47, 1, 0, 0xb0, 0, 0xb0, 1, 64],
                ]),
            ),
        ],
    ));
    sb.push(aml::device(
        "COM1",
        vec![
            aml::name("_HID", aml::eisa_id("PNP0501")),
            aml::name("_UID", aml::int(1)),
            aml::name(
                "_CRS",
                aml::resource_template(vec![
                    vec![0x47, 1, 0xf8, 3, 0xf8, 3, 1, 8],
                    res::interrupt(&[UART_IRQ], false, true, false),
                ]),
            ),
        ],
    ));
    if tpm {
        sb.push(aml::device(
            "TPM0",
            vec![
                aml::name("_HID", aml::string("MSFT0101")),
                aml::name(
                    "_CRS",
                    aml::resource_template(vec![res::memory32_fixed(
                        TPM_BASE as u32,
                        0x5000,
                        true,
                    )]),
                ),
            ],
        ));
    }
    let mut dsdt = Table::new(b"DSDT", 2);
    dsdt.raw(&aml::scope("\\_SB", sb));
    dsdt.raw(&aml::name(
        "\\_S5",
        aml::package(vec![aml::int(0), aml::int(0), aml::int(0), aml::int(0)]),
    ));
    let mut tables = vec![madt.finish(), mcfg.finish()];
    if tpm {
        let mut t = Table::new(b"TPM2", 4);
        t.u16(0).u16(0).u64(0).u32(6).zeros(12).u32(0).u64(0);
        tables.push(t.finish());
    }
    assemble_pc(ids, f, dsdt.finish(), tables)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn modern_fadt_power_registers_match_legacy_ports() {
        let b = build(4, false, &AcpiIds::default(), 4 << 30);
        let pos = b.tables.windows(4).position(|s| s == b"FACP").unwrap();
        let f = &b.tables[pos..pos + 276];
        assert!(f[8] >= 5);
        for (legacy, extended, width, access, port) in [
            (56, 148, 32, 2, PM_BASE),
            (64, 172, 16, 2, PM_BASE + 4),
            (76, 208, 32, 3, PM_BASE + 8),
        ] {
            assert_eq!(
                u32::from_le_bytes(f[legacy..legacy + 4].try_into().unwrap()),
                port as u32
            );
            assert_eq!(&f[extended..extended + 4], &[1, width, 0, access]);
            assert_eq!(
                u64::from_le_bytes(f[extended + 4..extended + 12].try_into().unwrap()),
                port as u64
            );
        }
    }

    #[test]
    fn tables_link_and_checksum() {
        let b = build(4, true, &AcpiIds::default(), 512 << 20);
        let (rsdp, tables) = crate::acpi::tests::link(&b, 0x1000000, 0xf0000);
        assert_eq!(crate::acpi::checksum(&rsdp), 0);
        for sig in [b"FACP", b"APIC", b"MCFG", b"DSDT", b"TPM2"] {
            let pos = tables.windows(4).position(|s| s == sig).unwrap();
            let len = u32::from_le_bytes(tables[pos + 4..pos + 8].try_into().unwrap()) as usize;
            assert_eq!(crate::acpi::checksum(&tables[pos..pos + len]), 0);
        }
    }

    #[test]
    fn direct_boot_tables_match_firmware_linker() {
        use vroom_memory::{GuestAddress, GuestMemory};
        let b = build(254, true, &AcpiIds::default(), 512 << 20);
        let mem = GuestMemory::new(&[(GuestAddress(RSDP_ADDR), 65536)]).unwrap();
        install(&mem, &b).unwrap();
        let (rsdp, tables) = crate::acpi::tests::link(&b, RSDP_ADDR + 4096, RSDP_ADDR);
        let mut read_rsdp = vec![0; rsdp.len()];
        mem.read(GuestAddress(RSDP_ADDR), &mut read_rsdp).unwrap();
        let mut read_tables = vec![0; tables.len()];
        mem.read(GuestAddress(RSDP_ADDR + 4096), &mut read_tables)
            .unwrap();
        assert_eq!(read_rsdp, rsdp);
        assert_eq!(read_tables, tables);
    }
}

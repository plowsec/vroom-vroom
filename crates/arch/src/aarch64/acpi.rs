//! ACPI tables for the aarch64 board (what Windows on ARM boots from).

use super::layout::*;
use crate::acpi::aml::{self, res};
use crate::acpi::{assemble, AcpiBlobs, Table};

const GSIV_SPI_BASE: u32 = 32;
const VGIC_MAINT_PPI: u32 = 25;
const PMU_PPI: u32 = 23;

#[derive(Debug, Clone)]
pub struct AcpiConfig {
    pub cpu_mpidrs: Vec<u64>,
    pub gic_dist_size: u64,
    pub gic_redist_size: u64,
    pub gic_msi: Option<(u64, u64)>,
    pub pci: bool,
    /// virtio-mmio slots in use: (base, size, spi). Linux reads these from ACPI.
    pub virtio_mmio: Vec<(u64, u64, u32)>,
    /// PSCI via SMC (guest at EL2) instead of HVC.
    pub psci_smc: bool,
    /// TPM TIS registers, if a vTPM is attached.
    pub tpm: Option<u64>,
}

/// TPM2 table, rev 4: FIFO/TIS interface (start method 6), event log
/// comes from the firmware (EFI_TCG2), so no log area here.
fn tpm2() -> Vec<u8> {
    let mut t = Table::new(b"TPM2", 4);
    t.u16(0).u16(0); // client platform, reserved
    t.u64(0); // control area: none for TIS
    t.u32(6); // start method: memory mapped (TIS/FIFO)
    t.zeros(12); // start method parameters
    t.u32(0).u64(0); // LAML, LASA
    t.finish()
}

/// FADT, HW-reduced, PSCI over HVC. X_DSDT lives at offset 140.
fn fadt(smc: bool) -> Vec<u8> {
    let mut t = Table::new(b"FACP", 6);
    t.zeros(276 - 36);
    let mut b = t.finish();
    b[8] = 6;
    b[131] = 3; // FADT minor version (ACPI 6.3)
    // Flags: HW_REDUCED_ACPI
    b[112..116].copy_from_slice(&(1u32 << 20).to_le_bytes());
    // ARM boot arch: PSCI compliant, use HVC.
    b[129..131].copy_from_slice(&(if smc { 1u16 } else { 3u16 }).to_le_bytes());
    b[268..276].copy_from_slice(b"BULWARK\0");
    b
}

fn madt(c: &AcpiConfig) -> Vec<u8> {
    let mut t = Table::new(b"APIC", 5);
    t.u32(0).u32(0);
    for (i, &mpidr) in c.cpu_mpidrs.iter().enumerate() {
        t.u8(0x0b).u8(80).u16(0);
        t.u32(i as u32) // CPU interface number
            .u32(i as u32) // ACPI processor UID
            .u32(1) // enabled
            .u32(0) // parking protocol
            .u32(PMU_PPI)
            .u64(0) // parked address
            .u64(0) // GICC base (system registers)
            .u64(0)
            .u64(0)
            .u32(VGIC_MAINT_PPI)
            .u64(0) // GICR base: from the GICR structure
            .u64(mpidr & 0xff_00ff_ffff)
            .u8(0)
            .u8(0)
            .u16(0);
    }
    // GICD
    t.u8(0x0c).u8(24).u16(0).u32(0).u64(GIC_DIST_BASE).u32(0).u8(3).zeros(3);
    // GICR discovery range
    let redist_len = c.gic_redist_size * c.cpu_mpidrs.len() as u64;
    t.u8(0x0e).u8(16).u16(0).u64(GIC_REDIST_BASE).u32(redist_len as u32);
    if let Some((base, _)) = c.gic_msi {
        t.u8(0x0d).u8(24).u16(0).u32(0).u64(base).u32(1).u16(MSI_INTID_COUNT as u16).u16(MSI_INTID_BASE as u16);
    }
    t.finish()
}

fn gtdt() -> Vec<u8> {
    let mut t = Table::new(b"GTDT", 3);
    let level_high = 0u32;
    let always_on = 1u32 << 2;
    t.u64(u64::MAX).u32(0);
    t.u32(16 + 13).u32(level_high | always_on); // secure EL1
    t.u32(16 + 14).u32(level_high | always_on); // non-secure EL1
    t.u32(16 + 11).u32(level_high | always_on); // virtual EL1
    t.u32(16 + 10).u32(level_high | always_on); // EL2
    t.u64(u64::MAX).u32(0).u32(0);
    t.u32(0).u32(0); // virtual EL2 (unused)
    t.finish()
}

fn spcr() -> Vec<u8> {
    let mut t = Table::new(b"SPCR", 2);
    t.u8(3).zeros(3); // ARM PL011
    t.gas_mmio32(UART_BASE);
    t.u8(8).u8(0).u32(GSIV_SPI_BASE + UART_SPI); // GIC interrupt
    t.u8(7).u8(0).u8(1).u8(0).u8(0).u8(0); // 115200 8N1, no flow ctl, VT100
    t.u16(0xffff).u16(0xffff).u8(0).u8(0).u8(0).u32(0).u8(0).u32(0);
    t.finish()
}

fn dbg2() -> Vec<u8> {
    let ns = b"\\_SB.COM0\0";
    let mut t = Table::new(b"DBG2", 0);
    t.u32(44).u32(1);
    let info_len = 22 + 12 + 4 + ns.len() as u16;
    t.u8(0).u16(info_len).u8(1).u16(ns.len() as u16).u16(38).u16(0).u16(0);
    t.u16(0x8000).u16(0x0003).u16(0).u16(22).u16(34);
    t.gas_mmio32(UART_BASE);
    t.u32(0x1000);
    t.raw(ns);
    t.finish()
}

fn mcfg() -> Vec<u8> {
    let mut t = Table::new(b"MCFG", 1);
    let buses = (PCI_ECAM_SIZE >> 20) as u8;
    t.u64(0).u64(PCI_ECAM_BASE).u16(0).u8(0).u8(buses - 1).u32(0);
    t.finish()
}

fn dsdt(c: &AcpiConfig) -> Vec<u8> {
    let mut sb = Vec::new();
    for i in 0..c.cpu_mpidrs.len() {
        sb.push(aml::device(
            &format!("C{i:03X}"),
            vec![aml::name("_HID", aml::string("ACPI0007")), aml::name("_UID", aml::int(i as u64))],
        ));
    }
    sb.push(aml::device(
        "COM0",
        vec![
            aml::name("_HID", aml::string("ARMH0011")),
            aml::name("_UID", aml::int(0)),
            aml::name(
                "_CRS",
                aml::resource_template(vec![
                    res::memory32_fixed(UART_BASE as u32, MMIO_DEV_SIZE as u32, true),
                    res::interrupt(&[GSIV_SPI_BASE + UART_SPI], true, true, false),
                ]),
            ),
        ],
    ));
    for (i, &(base, size, spi)) in c.virtio_mmio.iter().enumerate() {
        sb.push(aml::device(
            &format!("VR{i:02X}"),
            vec![
                aml::name("_HID", aml::string("LNRO0005")),
                aml::name("_UID", aml::int(i as u64)),
                aml::name("_CCA", aml::int(1)),
                aml::name(
                    "_CRS",
                    aml::resource_template(vec![
                        res::memory32_fixed(base as u32, size as u32, true),
                        res::interrupt(&[GSIV_SPI_BASE + spi], true, true, false),
                    ]),
                ),
            ],
        ));
    }
    if c.pci {
        sb.push(pci_root());
    }
    if let Some(base) = c.tpm {
        sb.push(aml::device(
            "TPM0",
            vec![
                aml::name("_HID", aml::string("MSFT0101")),
                aml::name("_UID", aml::int(0)),
                aml::name("_CRS", aml::resource_template(vec![res::memory32_fixed(base as u32, TPM_SIZE as u32, true)])),
            ],
        ));
    }
    let mut t = Table::new(b"DSDT", 2);
    t.raw(&aml::scope("\\_SB", sb));
    t.finish()
}

/// PCIe root complex with INTx routed through 4 link devices.
fn pci_root() -> Vec<u8> {
    let mut children = vec![
        aml::name("_HID", aml::eisa_id("PNP0A08")),
        aml::name("_CID", aml::eisa_id("PNP0A03")),
        aml::name("_SEG", aml::int(0)),
        aml::name("_BBN", aml::int(0)),
        aml::name("_UID", aml::int(0)),
        aml::name("_CCA", aml::int(1)),
    ];
    for link in 0..4u32 {
        let gsiv = GSIV_SPI_BASE + PCI_INTX_SPI + link;
        let crs = aml::resource_template(vec![res::interrupt(&[gsiv], true, true, false)]);
        children.push(aml::device(
            &format!("GSI{link}"),
            vec![
                aml::name("_HID", aml::eisa_id("PNP0C0F")),
                aml::name("_UID", aml::int(link as u64)),
                aml::name("_PRS", crs.clone()),
                aml::name("_CRS", crs),
                aml::method("_SRS", 1, false, vec![]),
                aml::method("_DIS", 0, false, vec![]),
            ],
        ));
    }
    // _PRT: slot/pin -> link device GSIx (same swizzle as the FDT).
    let mut prt = Vec::new();
    for slot in 0..32u64 {
        for pin in 0..4u64 {
            let link = (slot + pin) % 4;
            prt.push(aml::package(vec![
                aml::int((slot << 16) | 0xffff),
                aml::int(pin),
                aml::path(&format!("GSI{link}")),
                aml::int(0),
            ]));
        }
    }
    children.push(aml::name("_PRT", aml::package(prt)));
    let buses = (PCI_ECAM_SIZE >> 20) as u16;
    children.push(aml::name(
        "_CRS",
        aml::resource_template(vec![
            res::word_bus(0, buses - 1),
            res::dword_memory(PCI_MMIO_BASE as u32, PCI_MMIO_SIZE as u32),
            res::dword_io(0, PCI_PIO_SIZE as u32, PCI_PIO_BASE as u32),
            res::qword_memory(PCI_MMIO64_BASE, PCI_MMIO64_SIZE),
        ]),
    ));
    // Grant the OS everything it asks for.
    children.push(aml::method("_OSC", 4, false, vec![aml::ret(aml::arg(3))]));
    children.push(aml::device(
        "RES0",
        vec![
            aml::name("_HID", aml::eisa_id("PNP0C02")),
            aml::name(
                "_CRS",
                aml::resource_template(vec![res::qword_memory(PCI_ECAM_BASE, PCI_ECAM_SIZE)]),
            ),
        ],
    ));
    aml::device("PCI0", children)
}

pub fn build(c: &AcpiConfig) -> AcpiBlobs {
    let mut others = vec![madt(c), gtdt(), spcr(), dbg2()];
    if c.pci {
        others.push(mcfg());
    }
    if c.tpm.is_some() {
        others.push(tpm2());
    }
    assemble(fadt(c.psci_smc), 140, dsdt(c), others)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_build() {
        let b = build(&AcpiConfig {
            cpu_mpidrs: vec![0x8000_0000, 0x8000_0001],
            gic_dist_size: 0x10000,
            gic_redist_size: 0x20000,
            gic_msi: Some((GIC_MSI_BASE, 0x10000)),
            pci: true,
            virtio_mmio: vec![(VIRTIO_MMIO_BASE, 0x200, 16)],
            psci_smc: false,
            tpm: Some(TPM_BASE),
        });
        assert_eq!(b.loader.len() % 128, 0);
        for sig in [&b"FACP"[..], b"APIC", b"GTDT", b"SPCR", b"DBG2", b"MCFG", b"DSDT", b"XSDT", b"TPM2"] {
            assert!(b.tables.windows(4).any(|w| w == sig), "{}", String::from_utf8_lossy(sig));
        }
        // Table lengths are self-consistent.
        let madt = madt(&AcpiConfig {
            cpu_mpidrs: vec![0x8000_0000],
            gic_dist_size: 0,
            gic_redist_size: 0x20000,
            gic_msi: None,
            pci: false,
            virtio_mmio: vec![],
            psci_smc: false,
            tpm: None,
        });
        assert_eq!(madt.len(), 36 + 8 + 80 + 24 + 16);
        assert_eq!(dbg2().len(), 44 + 22 + 12 + 4 + 10);
        assert_eq!(spcr().len(), 80);
        assert_eq!(gtdt().len(), 104);
    }
}

#[cfg(test)]
mod dump {
    /// `BULWARK_ACPI_DUMP=dir cargo test -p bulwark-arch dump` writes each
    /// table to dir/<SIG>.dat for `iasl -d`.
    #[test]
    fn dump() {
        let Some(dir) = std::env::var_os("BULWARK_ACPI_DUMP") else { return };
        let b = super::build(&super::AcpiConfig {
            cpu_mpidrs: vec![0x8000_0000, 0x8000_0001],
            gic_dist_size: 0x10000,
            gic_redist_size: 0x20000,
            gic_msi: Some((super::GIC_MSI_BASE, 0x10000)),
            pci: true,
            virtio_mmio: vec![(super::VIRTIO_MMIO_BASE, 0x200, 16)],
            psci_smc: false,
        });
        let t = &b.tables;
        let mut off = 0;
        while off + 8 <= t.len() {
            let len = u32::from_le_bytes([t[off + 4], t[off + 5], t[off + 6], t[off + 7]]) as usize;
            if len < 36 {
                off += 8;
                continue;
            }
            let sig = String::from_utf8_lossy(&t[off..off + 4]).to_string();
            let mut tbl = t[off..off + len].to_vec();
            tbl[9] = 0;
            tbl[9] = crate::acpi::checksum(&tbl);
            std::fs::write(std::path::Path::new(&dir).join(format!("{sig}.dat")), tbl).unwrap();
            off = (off + len).div_ceil(8) * 8;
        }
    }
}

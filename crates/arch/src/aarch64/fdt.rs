//! Flattened device tree for the aarch64 board.

use vm_fdt::{FdtWriter, FdtWriterResult};

use super::layout::*;
use crate::{host_random, Error, Result};

const PHANDLE_GIC: u32 = 1;
const PHANDLE_CLK: u32 = 2;
const PHANDLE_MSI: u32 = 3;

const GIC_SPI: u32 = 0;
const GIC_PPI: u32 = 1;
const IRQ_LEVEL_HIGH: u32 = 4;

/// Hard cap on FDT size (Linux requirement: < 2 MiB).
pub const FDT_MAX_SIZE: usize = 0x20_0000;

/// Optional devices described in the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FdtDevice {
    VirtioMmio { base: u64, size: u64, spi: u32 },
    PciEcam { ecam_base: u64, ecam_size: u64, buses: u32 },
    /// PL061 with a power-button key wired to GPIO 3 (graceful shutdown).
    GpioPowerKey { base: u64, spi: u32 },
}

#[derive(Debug, Clone)]
pub struct FdtConfig {
    /// MPIDR of every vCPU, in index order.
    pub cpu_mpidrs: Vec<u64>,
    pub ram: Vec<(u64, u64)>,
    pub cmdline: String,
    pub initrd: Option<(u64, u64)>,
    pub gic_dist_size: u64,
    pub gic_redist_size: u64,
    pub gic_msi: Option<(u64, u64)>,
    pub devices: Vec<FdtDevice>,
}

pub fn create_fdt(cfg: &FdtConfig) -> Result<Vec<u8>> {
    build(cfg).map_err(|e| Error::Fdt(e.to_string()))
}

fn build(cfg: &FdtConfig) -> FdtWriterResult<Vec<u8>> {
    let mut f = FdtWriter::new()?;
    let root = f.begin_node("")?;
    f.property_string("compatible", "linux,dummy-virt")?;
    f.property_string("model", "bulwark-virt")?;
    f.property_u32("#address-cells", 2)?;
    f.property_u32("#size-cells", 2)?;
    f.property_u32("interrupt-parent", PHANDLE_GIC)?;

    chosen(&mut f, cfg)?;
    for &(base, size) in &cfg.ram {
        let n = f.begin_node(&format!("memory@{base:x}"))?;
        f.property_string("device_type", "memory")?;
        f.property_array_u64("reg", &[base, size])?;
        f.end_node(n)?;
    }
    cpus(&mut f, cfg)?;
    psci(&mut f)?;
    gic(&mut f, cfg)?;
    timer(&mut f)?;
    clock(&mut f)?;
    uart(&mut f)?;
    rtc(&mut f)?;
    for d in &cfg.devices {
        match *d {
            FdtDevice::VirtioMmio { base, size, spi } => virtio_mmio(&mut f, base, size, spi)?,
            FdtDevice::PciEcam { ecam_base, ecam_size, buses } => {
                pci(&mut f, ecam_base, ecam_size, buses, cfg.gic_msi.is_some())?
            }
            FdtDevice::GpioPowerKey { base, spi } => gpio_keys(&mut f, base, spi)?,
        }
    }
    f.end_node(root)?;
    let blob = f.finish()?;
    if blob.len() > FDT_MAX_SIZE {
        return Err(vm_fdt::Error::TotalSizeTooLarge);
    }
    Ok(blob)
}

fn chosen(f: &mut FdtWriter, cfg: &FdtConfig) -> FdtWriterResult<()> {
    let n = f.begin_node("chosen")?;
    f.property_string("bootargs", &cfg.cmdline)?;
    f.property_string("stdout-path", &format!("/pl011@{UART_BASE:x}"))?;
    if let Some((start, size)) = cfg.initrd {
        f.property_u64("linux,initrd-start", start)?;
        f.property_u64("linux,initrd-end", start + size)?;
    }
    // Per-boot entropy: KASLR offset and early RNG seed come from the host
    // CSPRNG, so two VMs from the same image don't share layouts or keys.
    let mut seed = [0u8; 8];
    host_random(&mut seed);
    f.property_u64("kaslr-seed", u64::from_le_bytes(seed))?;
    let mut rng = [0u8; 64];
    host_random(&mut rng);
    f.property("rng-seed", &rng)?;
    f.end_node(n)
}

fn cpus(f: &mut FdtWriter, cfg: &FdtConfig) -> FdtWriterResult<()> {
    let n = f.begin_node("cpus")?;
    f.property_u32("#address-cells", 1)?;
    f.property_u32("#size-cells", 0)?;
    for &mpidr in &cfg.cpu_mpidrs {
        let aff = (mpidr & 0xff_ffff) as u32;
        let c = f.begin_node(&format!("cpu@{aff:x}"))?;
        f.property_string("device_type", "cpu")?;
        f.property_string("compatible", "arm,arm-v8")?;
        f.property_string("enable-method", "psci")?;
        f.property_u32("reg", aff)?;
        f.end_node(c)?;
    }
    f.end_node(n)
}

fn psci(f: &mut FdtWriter) -> FdtWriterResult<()> {
    let n = f.begin_node("psci")?;
    f.property_string_list("compatible", vec!["arm,psci-1.0".into(), "arm,psci-0.2".into()])?;
    f.property_string("method", "hvc")?;
    f.end_node(n)
}

fn gic(f: &mut FdtWriter, cfg: &FdtConfig) -> FdtWriterResult<()> {
    let redist_total = cfg.gic_redist_size * cfg.cpu_mpidrs.len() as u64;
    let n = f.begin_node(&format!("intc@{GIC_DIST_BASE:x}"))?;
    f.property_string("compatible", "arm,gic-v3")?;
    f.property_null("interrupt-controller")?;
    f.property_u32("#interrupt-cells", 3)?;
    f.property_u32("#address-cells", 2)?;
    f.property_u32("#size-cells", 2)?;
    f.property_null("ranges")?;
    f.property_u32("#redistributor-regions", 1)?;
    f.property_array_u64(
        "reg",
        &[GIC_DIST_BASE, cfg.gic_dist_size, GIC_REDIST_BASE, redist_total],
    )?;
    f.property_array_u32("interrupts", &[GIC_PPI, 9, IRQ_LEVEL_HIGH])?;
    f.property_u32("phandle", PHANDLE_GIC)?;
    if let Some((base, size)) = cfg.gic_msi {
        let m = f.begin_node(&format!("v2m@{base:x}"))?;
        f.property_string("compatible", "arm,gic-v2m-frame")?;
        f.property_null("msi-controller")?;
        f.property_array_u64("reg", &[base, size])?;
        f.property_u32("arm,msi-base-spi", MSI_INTID_BASE)?;
        f.property_u32("arm,msi-num-spis", MSI_INTID_COUNT)?;
        f.property_u32("phandle", PHANDLE_MSI)?;
        f.end_node(m)?;
    }
    f.end_node(n)
}

fn timer(f: &mut FdtWriter) -> FdtWriterResult<()> {
    let n = f.begin_node("timer")?;
    f.property_string("compatible", "arm,armv8-timer")?;
    f.property_null("always-on")?;
    // secure phys, non-secure phys, virtual, hyp (PPI numbers).
    let mut irqs = Vec::new();
    for ppi in [13, 14, 11, 10] {
        irqs.extend_from_slice(&[GIC_PPI, ppi, IRQ_LEVEL_HIGH]);
    }
    f.property_array_u32("interrupts", &irqs)?;
    f.end_node(n)
}

fn clock(f: &mut FdtWriter) -> FdtWriterResult<()> {
    let n = f.begin_node("apb-pclk")?;
    f.property_string("compatible", "fixed-clock")?;
    f.property_u32("#clock-cells", 0)?;
    f.property_u32("clock-frequency", 24_000_000)?;
    f.property_string("clock-output-names", "clk24mhz")?;
    f.property_u32("phandle", PHANDLE_CLK)?;
    f.end_node(n)
}

fn uart(f: &mut FdtWriter) -> FdtWriterResult<()> {
    let n = f.begin_node(&format!("pl011@{UART_BASE:x}"))?;
    f.property_string_list("compatible", vec!["arm,pl011".into(), "arm,primecell".into()])?;
    f.property_array_u64("reg", &[UART_BASE, MMIO_DEV_SIZE])?;
    f.property_array_u32("interrupts", &[GIC_SPI, UART_SPI, IRQ_LEVEL_HIGH])?;
    f.property_array_u32("clocks", &[PHANDLE_CLK, PHANDLE_CLK])?;
    f.property_string_list("clock-names", vec!["uartclk".into(), "apb_pclk".into()])?;
    f.end_node(n)
}

fn rtc(f: &mut FdtWriter) -> FdtWriterResult<()> {
    let n = f.begin_node(&format!("pl031@{RTC_BASE:x}"))?;
    f.property_string_list("compatible", vec!["arm,pl031".into(), "arm,primecell".into()])?;
    f.property_array_u64("reg", &[RTC_BASE, MMIO_DEV_SIZE])?;
    f.property_array_u32("interrupts", &[GIC_SPI, RTC_SPI, IRQ_LEVEL_HIGH])?;
    f.property_u32("clocks", PHANDLE_CLK)?;
    f.property_string("clock-names", "apb_pclk")?;
    f.end_node(n)
}

fn virtio_mmio(f: &mut FdtWriter, base: u64, size: u64, spi: u32) -> FdtWriterResult<()> {
    let n = f.begin_node(&format!("virtio_mmio@{base:x}"))?;
    f.property_string("compatible", "virtio,mmio")?;
    f.property_array_u64("reg", &[base, size])?;
    // Level: line stays up while InterruptStatus != 0, drops on ACK.
    f.property_array_u32("interrupts", &[GIC_SPI, spi, IRQ_LEVEL_HIGH])?;
    f.property_null("dma-coherent")?;
    f.end_node(n)
}

fn gpio_keys(f: &mut FdtWriter, base: u64, spi: u32) -> FdtWriterResult<()> {
    const PHANDLE_GPIO: u32 = 4;
    let n = f.begin_node(&format!("pl061@{base:x}"))?;
    f.property_string_list("compatible", vec!["arm,pl061".into(), "arm,primecell".into()])?;
    f.property_array_u64("reg", &[base, MMIO_DEV_SIZE])?;
    f.property_array_u32("interrupts", &[GIC_SPI, spi, IRQ_LEVEL_HIGH])?;
    f.property_u32("#gpio-cells", 2)?;
    f.property_null("gpio-controller")?;
    f.property_u32("clocks", PHANDLE_CLK)?;
    f.property_string("clock-names", "apb_pclk")?;
    f.property_u32("phandle", PHANDLE_GPIO)?;
    f.end_node(n)?;

    let k = f.begin_node("gpio-keys")?;
    f.property_string("compatible", "gpio-keys")?;
    let p = f.begin_node("poweroff")?;
    f.property_string("label", "GPIO Key Poweroff")?;
    f.property_u32("linux,code", 116)?; // KEY_POWER
    f.property_array_u32("gpios", &[PHANDLE_GPIO, 3, 0])?;
    f.end_node(p)?;
    f.end_node(k)
}

fn pci(f: &mut FdtWriter, ecam: u64, ecam_size: u64, buses: u32, msi: bool) -> FdtWriterResult<()> {
    let n = f.begin_node(&format!("pcie@{ecam:x}"))?;
    f.property_string("compatible", "pci-host-ecam-generic")?;
    f.property_string("device_type", "pci")?;
    f.property_array_u64("reg", &[ecam, ecam_size])?;
    f.property_array_u32("bus-range", &[0, buses - 1])?;
    f.property_u32("#address-cells", 3)?;
    f.property_u32("#size-cells", 2)?;
    f.property_u32("#interrupt-cells", 1)?;
    f.property_u32("linux,pci-domain", 0)?;
    f.property_null("dma-coherent")?;
    // ranges: <pci-space pci-addr(2) cpu-addr(2) size(2)>
    let hi = |x: u64| (x >> 32) as u32;
    let lo = |x: u64| x as u32;
    let ranges = [
        0x0100_0000, 0, 0, hi(PCI_PIO_BASE), lo(PCI_PIO_BASE), 0, lo(PCI_PIO_SIZE),
        0x0200_0000, hi(PCI_MMIO_BASE), lo(PCI_MMIO_BASE), hi(PCI_MMIO_BASE), lo(PCI_MMIO_BASE),
        hi(PCI_MMIO_SIZE), lo(PCI_MMIO_SIZE),
        0x0300_0000, hi(PCI_MMIO64_BASE), lo(PCI_MMIO64_BASE), hi(PCI_MMIO64_BASE),
        lo(PCI_MMIO64_BASE), hi(PCI_MMIO64_SIZE), lo(PCI_MMIO64_SIZE),
    ];
    f.property_array_u32("ranges", &ranges)?;
    if msi {
        f.property_u32("msi-parent", PHANDLE_MSI)?;
    }
    // Legacy INTx: swizzle over 4 SPIs for 32 slots.
    let mut map = Vec::new();
    for slot in 0..32u32 {
        for pin in 1..=4u32 {
            let spi = PCI_INTX_SPI + (slot + pin - 1) % 4;
            map.extend_from_slice(&[slot << 11, 0, 0, pin, PHANDLE_GIC, 0, 0, GIC_SPI, spi, IRQ_LEVEL_HIGH]);
        }
    }
    f.property_array_u32("interrupt-map", &map)?;
    f.property_array_u32("interrupt-map-mask", &[0xf800, 0, 0, 7])?;
    f.end_node(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> FdtConfig {
        FdtConfig {
            cpu_mpidrs: vec![0x8000_0000, 0x8000_0001],
            ram: vec![(RAM_BASE, 512 << 20)],
            cmdline: "console=ttyAMA0".into(),
            initrd: Some((0x4800_0000, 0x1000)),
            gic_dist_size: 0x1_0000,
            gic_redist_size: 0x2_0000,
            gic_msi: Some((GIC_MSI_BASE, 0x1_0000)),
            devices: vec![
                FdtDevice::VirtioMmio { base: VIRTIO_MMIO_BASE, size: 0x200, spi: 16 },
                FdtDevice::PciEcam { ecam_base: PCI_ECAM_BASE, ecam_size: PCI_ECAM_SIZE, buses: 16 },
                FdtDevice::GpioPowerKey { base: GPIO_BASE, spi: GPIO_SPI },
            ],
        }
    }

    #[test]
    fn builds_valid_blob() {
        let b = create_fdt(&cfg()).unwrap();
        assert_eq!(&b[..4], &[0xd0, 0x0d, 0xfe, 0xed]);
        assert!(b.len() < FDT_MAX_SIZE);
    }

    #[test]
    fn seeds_differ_per_boot() {
        assert_ne!(create_fdt(&cfg()).unwrap(), create_fdt(&cfg()).unwrap());
    }
}

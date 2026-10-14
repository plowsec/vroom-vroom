//! Guest physical memory map.
//!
//! ```text
//! 0x0000_0000  flash0 (UEFI code, read-only)     64M
//! 0x0400_0000  flash1 (UEFI vars)                64M
//! 0x0800_0000  GICv3 distributor                 64K
//! 0x0808_0000  GIC MSI frame (v2m)               64K
//! 0x080a_0000  GICv3 redistributors              128K per vCPU
//! 0x0900_0000  PL011 UART                        4K
//! 0x0901_0000  PL031 RTC                         4K
//! 0x0902_0000  fw_cfg                            4K (reserved)
//! 0x0903_0000  PL061 GPIO                        4K
//! 0x0a00_0000  virtio-mmio slots                 32 x 512
//! 0x1000_0000  PCIe 32-bit MMIO window           ~750M
//! 0x3eff_0000  PCIe PIO window                   64K
//! 0x3f00_0000  PCIe ECAM (16 buses)              16M
//! 0x4000_0000  RAM
//! 0x80_0000_0000 PCIe 64-bit MMIO window         512G
//! ```

pub const FLASH0_BASE: u64 = 0x0000_0000;
pub const FLASH_SIZE: u64 = 0x0400_0000;
pub const FLASH1_BASE: u64 = 0x0400_0000;

pub const GIC_DIST_BASE: u64 = 0x0800_0000;
pub const GIC_MSI_BASE: u64 = 0x0808_0000;
pub const GIC_REDIST_BASE: u64 = 0x080a_0000;
/// Redistributor space ends where the UART block starts.
pub const GIC_REDIST_LIMIT: u64 = 0x0900_0000;

pub const UART_BASE: u64 = 0x0900_0000;
pub const RTC_BASE: u64 = 0x0901_0000;
pub const FW_CFG_BASE: u64 = 0x0902_0000;
pub const GPIO_BASE: u64 = 0x0903_0000;
pub const MMIO_DEV_SIZE: u64 = 0x1000;

/// TPM TIS registers (same spot as QEMU's platform bus TPM).
pub const TPM_BASE: u64 = 0x0c00_0000;
pub const TPM_SIZE: u64 = 0x5000;
pub const VIRTIO_MMIO_BASE: u64 = 0x0a00_0000;
pub const VIRTIO_MMIO_SIZE: u64 = 0x200;
pub const VIRTIO_MMIO_SLOTS: u32 = 32;

pub const PCI_MMIO_BASE: u64 = 0x1000_0000;
pub const PCI_MMIO_SIZE: u64 = 0x2eff_0000;
pub const PCI_PIO_BASE: u64 = 0x3eff_0000;
pub const PCI_PIO_SIZE: u64 = 0x1_0000;
pub const PCI_ECAM_BASE: u64 = 0x3f00_0000;
pub const PCI_ECAM_SIZE: u64 = 0x0100_0000;
pub const PCI_MMIO64_BASE: u64 = 0x80_0000_0000;
pub const PCI_MMIO64_SIZE: u64 = 0x80_0000_0000;

pub const RAM_BASE: u64 = 0x4000_0000;
/// Shared memory windows of virtio-mmio devices (GPU host-visible memory).
/// Not RAM: the hypervisor maps host buffers in on demand.
pub const SHM_BASE: u64 = 0x70_0000_0000;
pub const SHM_SIZE: u64 = 0x10_0000_0000;
/// RAM must end below the shared memory windows.
pub const RAM_MAX: u64 = SHM_BASE - RAM_BASE;

/// SPI assignments (0-based; GIC intid = 32 + n).
pub const UART_SPI: u32 = 1;
pub const RTC_SPI: u32 = 2;
pub const PCI_INTX_SPI: u32 = 3; // 3..=6
pub const GPIO_SPI: u32 = 7;
pub const VIRTIO_MMIO_SPI: u32 = 16; // 16..48

/// GIC intids reserved for MSIs (absolute intid numbers).
pub const MSI_INTID_BASE: u32 = 256;
pub const MSI_INTID_COUNT: u32 = 256;

/// Max vCPUs that fit the redistributor window.
pub fn max_vcpus(redist_size_per_cpu: u64) -> u32 {
    ((GIC_REDIST_LIMIT - GIC_REDIST_BASE) / redist_size_per_cpu.max(1)) as u32
}

pub fn virtio_mmio_slot(i: u32) -> (u64, u32) {
    (VIRTIO_MMIO_BASE + i as u64 * VIRTIO_MMIO_SIZE, VIRTIO_MMIO_SPI + i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_sane() {
        assert_eq!(max_vcpus(0x20000), 123);
        let (last, irq) = virtio_mmio_slot(VIRTIO_MMIO_SLOTS - 1);
        assert!(last + VIRTIO_MMIO_SIZE <= PCI_MMIO_BASE);
        assert!(irq < MSI_INTID_BASE - 32);
        assert!(PCI_ECAM_BASE + PCI_ECAM_SIZE <= RAM_BASE);
        assert!(PCI_PIO_BASE + PCI_PIO_SIZE <= PCI_ECAM_BASE);
        assert!(PCI_MMIO_BASE + PCI_MMIO_SIZE <= PCI_PIO_BASE);
    }
}

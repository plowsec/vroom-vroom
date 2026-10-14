use std::sync::Arc;

use bulwark_devices::IrqLine;
use bulwark_hv::Vm;

/// A device interrupt wired to one irqchip input line.
pub struct SpiLine {
    vm: Arc<dyn Vm>,
    spi: u32,
}

impl SpiLine {
    pub fn new(vm: Arc<dyn Vm>, spi: u32) -> Arc<Self> {
        Arc::new(SpiLine { vm, spi })
    }
}

impl IrqLine for SpiLine {
    fn set_level(&self, level: bool) {
        if let Err(e) = self.vm.set_irq_line(self.spi, level) {
            log::warn!("irq {}: {e}", self.spi);
        }
    }
}

/// MSI delivery through the irqchip (GICv2m frame on ARM).
pub struct GicMsi {
    vm: Arc<dyn Vm>,
}

impl GicMsi {
    pub fn new(vm: Arc<dyn Vm>) -> Arc<Self> {
        Arc::new(GicMsi { vm })
    }
}

impl bulwark_devices::pci::MsiSender for GicMsi {
    fn send(&self, addr: u64, data: u32) {
        if let Err(e) = self.vm.signal_msi(addr, data) {
            log::debug!("msi {addr:#x}/{data}: {e}");
        }
    }
}

/// GICv2m MSI frame in userspace, for backends without one (KVM).
/// Writing an SPI number to SETSPI_NS raises that SPI.
pub struct V2mFrame {
    vm: Arc<dyn Vm>,
}

impl V2mFrame {
    pub fn new(vm: Arc<dyn Vm>) -> Self {
        V2mFrame { vm }
    }
}

const V2M_TYPER: u64 = 0x008;
const V2M_SETSPI_NS: u64 = 0x040;
const V2M_IIDR: u64 = 0xfcc;

impl bulwark_devices::BusDevice for V2mFrame {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        use bulwark_arch::aarch64::layout::{MSI_INTID_BASE, MSI_INTID_COUNT};
        let v: u64 = match offset {
            V2M_TYPER => ((MSI_INTID_BASE as u64) << 16) | MSI_INTID_COUNT as u64,
            V2M_IIDR => 0x0000_043b,
            _ => 0,
        };
        bulwark_devices::put_le(data, v);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        use bulwark_arch::aarch64::layout::{MSI_INTID_BASE, MSI_INTID_COUNT};
        if offset == V2M_SETSPI_NS {
            let intid = bulwark_devices::get_le(data) as u32;
            // Only the SPIs this frame owns.
            if (MSI_INTID_BASE..MSI_INTID_BASE + MSI_INTID_COUNT).contains(&intid) {
                let _ = self.vm.signal_msi(offset, intid);
            }
        }
    }
}

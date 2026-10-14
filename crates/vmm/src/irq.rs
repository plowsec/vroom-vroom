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

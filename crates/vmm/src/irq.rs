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

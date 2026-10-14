use std::sync::Arc;

/// An interrupt line a device can drive. Implemented by the VMM on top of
/// the hypervisor's irqchip; devices don't know which one.
pub trait IrqLine: Send + Sync {
    fn set_level(&self, level: bool);

    /// Edge-style notification.
    fn trigger(&self) {
        self.set_level(true);
        self.set_level(false);
    }
}

/// Line that goes nowhere (tests, devices with IRQs disabled).
#[derive(Debug, Default)]
pub struct NoIrq;

impl IrqLine for NoIrq {
    fn set_level(&self, _level: bool) {}
}

impl<T: IrqLine + ?Sized> IrqLine for Arc<T> {
    fn set_level(&self, level: bool) {
        (**self).set_level(level)
    }
}

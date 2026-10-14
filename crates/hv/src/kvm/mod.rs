//! Linux KVM backend. Work in progress.

use std::sync::Arc;

use crate::{Arch, Error, Hypervisor, IrqChipInfo, Result, Vm, VmParams};

#[derive(Debug)]
pub struct Kvm {
    _priv: (),
}

impl Kvm {
    pub fn new() -> Result<Self> {
        Ok(Kvm { _priv: () })
    }
}

impl Hypervisor for Kvm {
    fn name(&self) -> &'static str {
        "kvm"
    }
    fn arch(&self) -> Arch {
        Arch::host()
    }
    fn irqchip_info(&self) -> Result<IrqChipInfo> {
        Err(Error::Unsupported("kvm backend not done yet"))
    }
    fn create_vm(&self, _p: &VmParams) -> Result<Arc<dyn Vm>> {
        Err(Error::Unsupported("kvm backend not done yet"))
    }
}

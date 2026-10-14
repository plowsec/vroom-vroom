//! KVM on x86_64. Filled in with the x86 board.

use kvm_ioctls::{Kvm, VmFd};

use super::Chip;
use crate::{Error, IrqChipInfo, Result, VmParams};

pub(super) fn irqchip_info() -> IrqChipInfo {
    IrqChipInfo { dist_size: 0, redist_size_per_cpu: 0, msi_size: 0, spi_base: 0, spi_count: 24, msi_frame_in_kernel: true }
}

pub(super) fn create_vm(_kvm: &Kvm, _p: &VmParams) -> Result<(VmFd, Chip)> {
    Err(Error::Unsupported("x86_64 board not done yet"))
}

#[derive(Debug)]
pub struct KvmVcpu;

impl KvmVcpu {
    pub(super) fn new(_vm: &VmFd, _index: u32, _n: u32) -> Result<Self> {
        Err(Error::Unsupported("x86_64 board not done yet"))
    }
}

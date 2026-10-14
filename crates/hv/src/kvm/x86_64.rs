//! KVM on x86_64. Filled in with the x86 board.

use std::sync::Arc;

use kvm_ioctls::{Kvm, VmFd};

use super::Chip;
use crate::{BootRegs, Error, IrqChipInfo, Result, Vcpu, VcpuExit, VcpuKicker, VcpuState, VmParams};

pub(super) fn irqchip_info() -> IrqChipInfo {
    IrqChipInfo { dist_size: 0, redist_size_per_cpu: 0, msi_size: 0, spi_base: 0, spi_count: 24, msi_frame_in_kernel: true }
}

pub(super) fn create_vm(_kvm: &Kvm, _p: &VmParams) -> Result<(VmFd, Chip)> {
    Err(Error::Unsupported("x86_64 board not done yet"))
}

/// No x86 vCPU can exist yet: `new` always fails, so the type has no values.
#[derive(Debug)]
pub enum KvmVcpu {}

impl KvmVcpu {
    pub(super) fn new(_vm: &VmFd, _index: u32, _n: u32) -> Result<Self> {
        Err(Error::Unsupported("x86_64 board not done yet"))
    }
}

impl Vcpu for KvmVcpu {
    fn index(&self) -> u32 {
        match *self {}
    }
    fn run(&mut self) -> Result<VcpuExit> {
        match *self {}
    }
    fn complete_mmio_read(&mut self, _data: &[u8]) -> Result<()> {
        match *self {}
    }
    fn complete_pio_in(&mut self, _data: &[u8]) -> Result<()> {
        match *self {}
    }
    fn complete_hypercall(&mut self, _ret: [u64; 4]) -> Result<()> {
        match *self {}
    }
    fn kicker(&self) -> Arc<dyn VcpuKicker> {
        match *self {}
    }
    fn set_boot_regs(&mut self, _regs: &BootRegs) -> Result<()> {
        match *self {}
    }
    fn arch_id(&self) -> u64 {
        match *self {}
    }
    fn save_state(&mut self) -> Result<VcpuState> {
        match *self {}
    }
    fn restore_state(&mut self, _s: &VcpuState) -> Result<()> {
        match *self {}
    }
}

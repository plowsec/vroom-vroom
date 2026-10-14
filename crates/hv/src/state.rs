use serde::{Deserialize, Serialize};

/// Saved vCPU state. Backend-tagged register list: ids are only meaningful
/// to the backend that wrote them, so a snapshot can't be restored on a
/// different backend by accident.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcpuState {
    pub backend: String,
    pub index: u32,
    pub regs: Vec<(u32, u64)>,
    /// 128-bit SIMD/FP registers.
    pub vregs: Vec<(u32, [u8; 16])>,
    /// Guest-visible virtual counter value at save time.
    pub vcounter: u64,
    /// Extra opaque per-vcpu blob (e.g. per-cpu GIC regs).
    pub extra: Vec<(u32, u64)>,
    pub powered_on: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrqChipState {
    pub backend: String,
    pub blob: Vec<u8>,
}

//! Minimal Hypervisor.framework bindings. Only what we use.
#![allow(non_camel_case_types, dead_code)]

use std::ffi::c_void;

pub type hv_return_t = i32;
pub type hv_vcpu_t = u64;
pub type hv_ipa_t = u64;
pub type hv_memory_flags_t = u64;
pub type os_object_t = *mut c_void;

pub const HV_SUCCESS: hv_return_t = 0;

pub const HV_MEMORY_READ: u64 = 1 << 0;
pub const HV_MEMORY_WRITE: u64 = 1 << 1;
pub const HV_MEMORY_EXEC: u64 = 1 << 2;

pub const HV_EXIT_REASON_CANCELED: u32 = 0;
pub const HV_EXIT_REASON_EXCEPTION: u32 = 1;
pub const HV_EXIT_REASON_VTIMER_ACTIVATED: u32 = 2;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct hv_vcpu_exit_exception_t {
    pub syndrome: u64,
    pub virtual_address: u64,
    pub physical_address: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct hv_vcpu_exit_t {
    pub reason: u32,
    pub exception: hv_vcpu_exit_exception_t,
}

// General purpose register ids.
pub const HV_REG_X0: u32 = 0;
pub const HV_REG_PC: u32 = 31;
pub const HV_REG_FPCR: u32 = 32;
pub const HV_REG_FPSR: u32 = 33;
pub const HV_REG_CPSR: u32 = 34;

pub const HV_SYS_REG_MPIDR_EL1: u16 = 0xc005;
pub const HV_SYS_REG_ID_AA64DFR0_EL1: u16 = 0xc028;
pub const HV_SYS_REG_SP_EL1: u16 = 0xe208;
pub const HV_SYS_REG_CNTV_CTL_EL0: u16 = 0xdf19;
pub const HV_SYS_REG_CNTV_CVAL_EL0: u16 = 0xdf1a;

/// System registers saved in snapshots. Everything HVF exposes except the
/// read-only ID registers (those come from the host CPU).
pub const SNAPSHOT_SYS_REGS: &[u16] = &[
    0x8004, 0x8005, 0x8006, 0x8007, 0x800c, 0x800d, 0x800e, 0x800f, 0x8010, 0x8012, 0x8014,
    0x8015, 0x8016, 0x8017, 0x801c, 0x801d, 0x801e, 0x801f, 0x8024, 0x8025, 0x8026, 0x8027,
    0x802c, 0x802d, 0x802e, 0x802f, 0x8034, 0x8035, 0x8036, 0x8037, 0x803c, 0x803d, 0x803e,
    0x803f, 0x8044, 0x8045, 0x8046, 0x8047, 0x804c, 0x804d, 0x804e, 0x804f, 0x8054, 0x8055,
    0x8056, 0x8057, 0x805c, 0x805d, 0x805e, 0x805f, 0x8064, 0x8065, 0x8066, 0x8067, 0x806c,
    0x806d, 0x806e, 0x806f, 0x8074, 0x8075, 0x8076, 0x8077, 0x807c, 0x807d, 0x807e, 0x807f,
    0xc005, // MPIDR
    0xc080, 0xc082, 0xc100, 0xc101, 0xc102, 0xc108, 0xc109, 0xc10a, 0xc10b, 0xc110, 0xc111,
    0xc112, 0xc113, 0xc118, 0xc119, 0xc200, 0xc201, 0xc208, 0xc288, 0xc289, 0xc290, 0xc300,
    0xc3a0, 0xc510, 0xc518, 0xc600, 0xc681, 0xc684, 0xc708, 0xd000, 0xde82, 0xde83, 0xdf19,
    0xdf1a, 0xe208,
];

// GIC
pub const HV_GIC_INT_EL1_VIRTUAL_TIMER: u16 = 27;
pub const GICR_ISENABLER0: u32 = 0x10100;
pub const GICR_ISPENDR0: u32 = 0x10200;
pub const GICR_ICPENDR0: u32 = 0x10280;

/// Per-vCPU GIC CPU interface registers we save (HVF GIC blob may not have them).
pub const SNAPSHOT_ICC_REGS: &[u16] =
    &[0xc230, 0xc643, 0xc644, 0xc648, 0xc663, 0xc664, 0xc665, 0xc666, 0xc667];

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct mach_timebase_info_data_t {
    pub numer: u32,
    pub denom: u32,
}

#[link(name = "Hypervisor", kind = "framework")]
extern "C" {
    pub fn hv_vm_config_create() -> os_object_t;
    pub fn hv_vm_config_get_max_ipa_size(bits: *mut u32) -> hv_return_t;
    pub fn hv_vm_config_set_ipa_size(cfg: os_object_t, bits: u32) -> hv_return_t;
    pub fn hv_vm_create(cfg: os_object_t) -> hv_return_t;
    pub fn hv_vm_config_get_el2_supported(supported: *mut bool) -> hv_return_t;
    pub fn hv_vm_config_set_el2_enabled(cfg: os_object_t, enabled: bool) -> hv_return_t;
    pub fn hv_vm_destroy() -> hv_return_t;
    pub fn hv_vm_map(addr: *mut c_void, ipa: hv_ipa_t, size: usize, flags: hv_memory_flags_t) -> hv_return_t;
    pub fn hv_vm_unmap(ipa: hv_ipa_t, size: usize) -> hv_return_t;
    pub fn hv_vm_get_max_vcpu_count(count: *mut u32) -> hv_return_t;

    pub fn hv_vcpu_create(vcpu: *mut hv_vcpu_t, exit: *mut *const hv_vcpu_exit_t, cfg: os_object_t) -> hv_return_t;
    pub fn hv_vcpu_destroy(vcpu: hv_vcpu_t) -> hv_return_t;
    pub fn hv_vcpu_run(vcpu: hv_vcpu_t) -> hv_return_t;
    pub fn hv_vcpu_set_trap_debug_reg_accesses(vcpu: hv_vcpu_t, value: bool) -> hv_return_t;
    pub fn hv_vcpu_set_trap_debug_exceptions(vcpu: hv_vcpu_t, value: bool) -> hv_return_t;
    pub fn hv_vcpus_exit(vcpus: *const hv_vcpu_t, count: u32) -> hv_return_t;
    pub fn hv_vcpu_get_reg(vcpu: hv_vcpu_t, reg: u32, value: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_set_reg(vcpu: hv_vcpu_t, reg: u32, value: u64) -> hv_return_t;
            pub fn hv_vcpu_get_sys_reg(vcpu: hv_vcpu_t, reg: u16, value: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_set_sys_reg(vcpu: hv_vcpu_t, reg: u16, value: u64) -> hv_return_t;
    pub fn hv_vcpu_get_vtimer_mask(vcpu: hv_vcpu_t, masked: *mut bool) -> hv_return_t;
    pub fn hv_vcpu_set_vtimer_mask(vcpu: hv_vcpu_t, masked: bool) -> hv_return_t;
    pub fn hv_vcpu_get_vtimer_offset(vcpu: hv_vcpu_t, off: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_set_vtimer_offset(vcpu: hv_vcpu_t, off: u64) -> hv_return_t;

    pub fn hv_gic_config_create() -> os_object_t;
    pub fn hv_gic_config_set_distributor_base(cfg: os_object_t, base: hv_ipa_t) -> hv_return_t;
    pub fn hv_gic_config_set_redistributor_base(cfg: os_object_t, base: hv_ipa_t) -> hv_return_t;
    pub fn hv_gic_config_set_msi_region_base(cfg: os_object_t, base: hv_ipa_t) -> hv_return_t;
    pub fn hv_gic_config_set_msi_interrupt_range(cfg: os_object_t, base: u32, count: u32) -> hv_return_t;
    pub fn hv_gic_create(cfg: os_object_t) -> hv_return_t;
    pub fn hv_gic_get_distributor_size(sz: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_distributor_base_alignment(a: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_redistributor_size(sz: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_redistributor_base_alignment(a: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_msi_region_size(sz: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_msi_region_base_alignment(a: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_spi_interrupt_range(base: *mut u32, count: *mut u32) -> hv_return_t;
    pub fn hv_gic_set_spi(intid: u32, level: bool) -> hv_return_t;
    pub fn hv_gic_send_msi(addr: hv_ipa_t, intid: u32) -> hv_return_t;
    pub fn hv_gic_get_redistributor_reg(vcpu: hv_vcpu_t, reg: u32, v: *mut u64) -> hv_return_t;
    pub fn hv_gic_set_redistributor_reg(vcpu: hv_vcpu_t, reg: u32, v: u64) -> hv_return_t;
    pub fn hv_gic_get_icc_reg(vcpu: hv_vcpu_t, reg: u16, v: *mut u64) -> hv_return_t;
    pub fn hv_gic_set_icc_reg(vcpu: hv_vcpu_t, reg: u16, v: u64) -> hv_return_t;
    pub fn hv_gic_state_create() -> os_object_t;
    pub fn hv_gic_state_get_size(state: os_object_t, sz: *mut usize) -> hv_return_t;
    pub fn hv_gic_state_get_data(state: os_object_t, data: *mut c_void) -> hv_return_t;
    pub fn hv_gic_set_state(data: *const c_void, sz: usize) -> hv_return_t;
}

// csrc/hvf_shim.c
extern "C" {
    pub fn bw_hv_vcpu_get_simd_fp_reg(vcpu: hv_vcpu_t, reg: u32, bytes: *mut u8) -> hv_return_t;
    pub fn bw_hv_vcpu_set_simd_fp_reg(vcpu: hv_vcpu_t, reg: u32, bytes: *const u8) -> hv_return_t;
}

extern "C" {
    pub fn os_release(obj: *mut c_void);
    pub fn mach_absolute_time() -> u64;
    pub fn mach_timebase_info(info: *mut mach_timebase_info_data_t) -> i32;
}

/// Turn an hv_return_t into our error type.
pub fn check(call: &'static str, rc: hv_return_t) -> crate::Result<()> {
    if rc == HV_SUCCESS {
        Ok(())
    } else {
        Err(crate::Error::Backend { call, code: rc as u32 as i64 })
    }
}

/// Owned os_object; released on drop.
pub struct OsObject(pub os_object_t);

impl Drop for OsObject {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: we own one reference to a valid os_object.
            unsafe { os_release(self.0) }
        }
    }
}

/// Host ticks -> nanoseconds.
pub fn ticks_to_ns(ticks: u64) -> u64 {
    let mut tb = mach_timebase_info_data_t { numer: 0, denom: 0 };
    // SAFETY: valid out pointer.
    unsafe { mach_timebase_info(&mut tb) };
    if tb.denom == 0 {
        return ticks;
    }
    (ticks as u128 * tb.numer as u128 / tb.denom as u128) as u64
}


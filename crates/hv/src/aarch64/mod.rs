//! Pure aarch64 helpers shared by backends: syndrome decoding, MPIDR layout.
//! No hardware access here, so it's all unit tested.

/// MPIDR_EL1 value for vCPU `index`. 16 CPUs per cluster (GICv3 SGI limit).
pub fn mpidr_for(index: u32) -> u64 {
    let aff0 = (index % 16) as u64;
    let aff1 = ((index / 16) % 256) as u64;
    let aff2 = ((index / 4096) % 256) as u64;
    (1 << 31) | (aff2 << 16) | (aff1 << 8) | aff0
}

/// Affinity bits only (what PSCI and GIC compare).
pub fn mpidr_affinity(mpidr: u64) -> u64 {
    mpidr & 0xff_00ff_ffff
}

/// Exception classes we care about (ESR_ELx.EC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExceptionClass {
    WfxTrap,
    Hvc64,
    Smc64,
    SysReg,
    DataAbortLower,
    InstAbortLower,
    Other(u8),
}

pub fn exception_class(esr: u64) -> ExceptionClass {
    match ((esr >> 26) & 0x3f) as u8 {
        0x01 => ExceptionClass::WfxTrap,
        0x16 => ExceptionClass::Hvc64,
        0x17 => ExceptionClass::Smc64,
        0x18 => ExceptionClass::SysReg,
        0x20 => ExceptionClass::InstAbortLower,
        0x24 => ExceptionClass::DataAbortLower,
        x => ExceptionClass::Other(x),
    }
}

/// A stage-2 fault on a write: a data write (WnR), or a stage-1 table walk
/// updating a descriptor (S1PTW, data or instruction side). HVF reports a
/// write to a read-only page as a translation fault, so both count.
pub fn is_stage2_write_fault(esr: u64) -> bool {
    let class = exception_class(esr);
    let status = esr & 0x3c; // DFSC/IFSC without the level
    let translation_or_permission = status == 0x04 || status == 0x0c;
    let s1ptw = (esr >> 7) & 1 == 1;
    let wnr = (esr >> 6) & 1 == 1;
    translation_or_permission
        && match class {
            ExceptionClass::DataAbortLower => wnr || s1ptw,
            ExceptionClass::InstAbortLower => s1ptw,
            _ => false,
        }
}

/// Decoded MMIO data abort (only valid when ISV=1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataAbort {
    pub write: bool,
    pub len: usize,
    /// Register number (31 = XZR).
    pub reg: u8,
    pub sign_extend: bool,
    /// Destination is a 64-bit X register (vs W).
    pub sf: bool,
}

pub fn decode_data_abort(esr: u64) -> Option<DataAbort> {
    let isv = (esr >> 24) & 1 == 1;
    if !isv {
        return None;
    }
    let sas = (esr >> 22) & 3;
    Some(DataAbort {
        write: (esr >> 6) & 1 == 1,
        len: 1 << sas,
        reg: ((esr >> 16) & 0x1f) as u8,
        sign_extend: (esr >> 21) & 1 == 1,
        sf: (esr >> 15) & 1 == 1,
    })
}

impl DataAbort {
    /// Value to put in the destination register for an MMIO read of `data`.
    pub fn read_value(&self, data: &[u8]) -> u64 {
        let mut b = [0u8; 8];
        let n = data.len().min(self.len).min(8);
        b[..n].copy_from_slice(&data[..n]);
        let mut v = u64::from_le_bytes(b);
        if self.sign_extend && self.len < 8 {
            let shift = 64 - 8 * self.len as u32;
            v = (((v << shift) as i64) >> shift) as u64;
        }
        if !self.sf {
            v &= 0xffff_ffff;
        }
        v
    }
}

/// Decoded MSR/MRS trap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SysRegAccess {
    pub op0: u8,
    pub op1: u8,
    pub crn: u8,
    pub crm: u8,
    pub op2: u8,
    pub reg: u8,
    pub read: bool,
}

pub fn decode_sysreg(esr: u64) -> SysRegAccess {
    SysRegAccess {
        op0: ((esr >> 20) & 3) as u8,
        op2: ((esr >> 17) & 7) as u8,
        op1: ((esr >> 14) & 7) as u8,
        crn: ((esr >> 10) & 0xf) as u8,
        reg: ((esr >> 5) & 0x1f) as u8,
        crm: ((esr >> 1) & 0xf) as u8,
        read: esr & 1 == 1,
    }
}

impl SysRegAccess {
    /// Same encoding the ESR uses, handy as a map key / log id.
    pub fn key(&self) -> u32 {
        (self.op0 as u32) << 14
            | (self.op1 as u32) << 11
            | (self.crn as u32) << 7
            | (self.crm as u32) << 3
            | self.op2 as u32
    }
}

/// PSTATE for "EL1h, all of DAIF masked" (Linux boot requirement).
pub const PSTATE_EL1H_DAIF: u64 = 0x3c5;
/// Same, at EL2h (nested virtualization).
pub const PSTATE_EL2H_DAIF: u64 = 0x3c9;

/// PSCI / SMCCC function ids.
pub mod psci {
    pub const VERSION: u32 = 0x8400_0000;
    pub const CPU_SUSPEND_32: u32 = 0x8400_0001;
    pub const CPU_SUSPEND_64: u32 = 0xc400_0001;
    pub const CPU_OFF: u32 = 0x8400_0002;
    pub const CPU_ON_32: u32 = 0x8400_0003;
    pub const CPU_ON_64: u32 = 0xc400_0003;
    pub const AFFINITY_INFO_32: u32 = 0x8400_0004;
    pub const AFFINITY_INFO_64: u32 = 0xc400_0004;
    pub const MIGRATE_INFO_TYPE: u32 = 0x8400_0006;
    pub const SYSTEM_OFF: u32 = 0x8400_0008;
    pub const SYSTEM_RESET: u32 = 0x8400_0009;
    pub const FEATURES: u32 = 0x8400_000a;
    pub const SMCCC_VERSION: u32 = 0x8000_0000;

    pub const SUCCESS: i64 = 0;
    pub const NOT_SUPPORTED: i64 = -1;
    pub const INVALID_PARAMS: i64 = -2;
    pub const DENIED: i64 = -3;
    pub const ALREADY_ON: i64 = -4;
    pub const ON_PENDING: i64 = -5;
    pub const INTERNAL_FAILURE: i64 = -6;
    pub const NOT_PRESENT: i64 = -7;
    pub const DISABLED: i64 = -8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage2_write_faults() {
        // Data abort, translation fault level 2, write (what HVF reports
        // for a write to a read-only page).
        assert!(is_stage2_write_fault(0x92000046));
        // Same, a read.
        assert!(!is_stage2_write_fault(0x92000006));
        // Permission fault level 3, write.
        assert!(is_stage2_write_fault(0x9200004f));
        // Instruction abort, translation fault during a stage-1 walk.
        assert!(is_stage2_write_fault(0x82000086));
        // Instruction abort, plain fetch.
        assert!(!is_stage2_write_fault(0x82000006));
        // Alignment fault on a write.
        assert!(!is_stage2_write_fault(0x92000061));
    }

    #[test]
    fn mpidr_layout() {
        assert_eq!(mpidr_for(0), 0x8000_0000);
        assert_eq!(mpidr_for(17), 0x8000_0101);
        assert_eq!(mpidr_affinity(mpidr_for(17)), 0x101);
    }

    #[test]
    fn decode_ldr_w() {
        // ldr w1, [x0]: EC=0x24, IL=1, ISV=1, SAS=2, SRT=1, SF=0, WnR=0
        let esr = (0x24u64 << 26) | (1 << 25) | (1 << 24) | (2 << 22) | (1 << 16);
        assert_eq!(exception_class(esr), ExceptionClass::DataAbortLower);
        let d = decode_data_abort(esr).unwrap();
        assert_eq!(d, DataAbort { write: false, len: 4, reg: 1, sign_extend: false, sf: false });
        assert_eq!(d.read_value(&[0xff, 0xff, 0xff, 0xff]), 0xffff_ffff);
    }

    #[test]
    fn sign_extend_byte_into_x() {
        // ldrsb x2, [x0]
        let esr = (0x24u64 << 26) | (1 << 24) | (1 << 21) | (2 << 16) | (1 << 15);
        let d = decode_data_abort(esr).unwrap();
        assert_eq!(d.len, 1);
        assert_eq!(d.read_value(&[0x80]), 0xffff_ffff_ffff_ff80);
    }

    #[test]
    fn no_isv_is_none() {
        let esr = 0x24u64 << 26;
        assert!(decode_data_abort(esr).is_none());
    }

    #[test]
    fn sysreg_decode() {
        // mrs x3, OSLSR_EL1 : op0=2 op1=0 crn=1 crm=1 op2=4, read
        let esr = (0x18u64 << 26) | (2 << 20) | (4 << 17) | (1 << 10) | (3 << 5) | (1 << 1) | 1;
        let s = decode_sysreg(esr);
        assert_eq!((s.op0, s.op1, s.crn, s.crm, s.op2, s.reg, s.read), (2, 0, 1, 1, 4, 3, true));
    }
}

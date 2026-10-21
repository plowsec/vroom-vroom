//! Small, OS-independent debugger controls. No debugger transport or UI.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DebugConfig {
    pub enabled: bool,
    /// Arm one instruction step; the caller must rearm after each stop.
    pub single_step: bool,
    /// Execution addresses, applied to this CPU. ARM64 addresses must be aligned.
    pub breakpoints: Vec<u64>,
    pub watchpoints: Vec<Watchpoint>,
    /// Stop on guest BRK instructions. Otherwise deliver them to the guest
    /// exception vector, so its own debug services keep working.
    pub trap_software_breakpoints: bool,
    /// Stop on guest BRKs with this immediate only; others still go to the
    /// guest (a fuzzer's coverage breakpoints next to Windows' own BRKs).
    pub trap_brk_immediate: Option<u16>,
}

impl DebugConfig {
    /// Whether a BRK with syndrome `esr` stops for the host.
    pub fn traps_brk(&self, esr: u64) -> bool {
        self.trap_software_breakpoints || self.trap_brk_immediate == Some(esr as u16)
    }
}

/// A byte range within one aligned 8-byte granule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Watchpoint {
    pub address: u64,
    pub size: u8,
    pub read: bool,
    pub write: bool,
}
impl Watchpoint {
    pub fn arm64_control(&self) -> crate::Result<u64> {
        if self.size == 0
            || self.size > 8
            || (self.address & 7) + self.size as u64 > 8
            || (!self.read && !self.write)
        {
            return Err(crate::Error::Invalid(
                "watchpoint: 1..8 bytes within one 8-byte granule, read and/or write".into(),
            ));
        }
        let bas = ((1u64 << self.size) - 1) << (self.address & 7);
        Ok(1 | (3 << 1) | (u64::from(self.read) << 3) | (u64::from(self.write) << 4) | (bas << 5))
    }
}

/// Architectural AArch64 synchronous exception entry into EL1. Only the
/// EL0/EL1, non-NMI path used by the HVF backend is supported.
pub fn arm64_el1_exception_entry(
    pstate: u64,
    sctlr: u64,
    vbar: u64,
    pan: bool,
    mte: bool,
) -> crate::Result<(u64, u64)> {
    let offset = match pstate & 31 {
        0 => 0x400,
        4 => 0,
        5 => 0x200,
        _ => {
            return Err(crate::Error::Unsupported(
                "exception entry from this CPU mode",
            ))
        }
    };
    if vbar & 0x7ff != 0 || sctlr & (1 << 61) != 0 {
        return Err(crate::Error::Unsupported(
            "unaligned vector base or NMI exception entry",
        ));
    }
    // NZCV and DIT survive; BTYPE, SS, IL and UAO are cleared. Exception
    // entry selects SP_EL1 and masks D,A,I,F. PAN and SSBS follow SCTLR.
    let mut next = (pstate & ((15 << 28) | (1 << 24) | (1 << 22))) | 5 | 0x3c0;
    if pan && sctlr & (1 << 23) == 0 {
        next |= 1 << 22;
    }
    if sctlr & (1 << 44) != 0 {
        next |= 1 << 12;
    }
    if mte {
        next |= 1 << 25;
    }
    Ok((vbar + offset, next))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugReason {
    Breakpoint,
    SoftwareBreakpoint,
    SingleStep,
    Watchpoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DebugStop {
    pub cpu: u32,
    pub pc: u64,
    pub syndrome: u64,
    pub reason: DebugReason,
    /// A watchpoint stop's faulting virtual address (FAR): within the
    /// naturally aligned block the instruction accessed, not necessarily
    /// inside the watched bytes.
    pub fault_address: Option<u64>,
}

/// A watchpoint syndrome's WnR bit: the access was a write.
pub fn arm64_watchpoint_write(esr: u64) -> bool {
    esr & (1 << 6) != 0
}

/// Both lower-EL and same-EL architectural syndromes.
pub fn arm64_reason(esr: u64) -> Option<DebugReason> {
    match (esr >> 26) & 0x3f {
        0x30 | 0x31 => Some(DebugReason::Breakpoint),
        0x32 | 0x33 => Some(DebugReason::SingleStep),
        0x34 | 0x35 => Some(DebugReason::Watchpoint),
        0x3c => Some(DebugReason::SoftwareBreakpoint),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn one_brk_immediate_can_trap_alone() {
        let brk = |imm: u64| 0x3c << 26 | 1 << 25 | imm;
        let only = DebugConfig {
            trap_brk_immediate: Some(0x766d),
            ..Default::default()
        };
        assert!(only.traps_brk(brk(0x766d)));
        assert!(!only.traps_brk(brk(0xf000)));
        assert!(!DebugConfig::default().traps_brk(brk(0x766d)));
        let all = DebugConfig {
            trap_software_breakpoints: true,
            ..Default::default()
        };
        assert!(all.traps_brk(brk(0xf000)));
    }

    #[test]
    fn debug_syndromes_do_not_consume_memory_faults() {
        assert_eq!(
            arm64_reason(0x3c << 26 | 0x1234),
            Some(DebugReason::SoftwareBreakpoint)
        );
        assert_eq!(arm64_reason(0x32 << 26), Some(DebugReason::SingleStep));
        assert_eq!(arm64_reason(0x30 << 26), Some(DebugReason::Breakpoint));
        assert_eq!(arm64_reason(0x24 << 26), None);
        assert_eq!(arm64_reason(0x20 << 26), None);
    }

    #[test]
    fn synchronous_exception_entry_preserves_return_context_separately() {
        let old = 0xa0000000 | 4 | (1 << 21) | (1 << 23) | (1 << 24) | (3 << 10);
        let (pc, ps) = arm64_el1_exception_entry(old, 1 << 44, 0x8000, true, false).unwrap();
        assert_eq!(pc, 0x8000);
        assert_eq!(ps, 0xa14013c5);
        assert_eq!(
            arm64_el1_exception_entry(0, 0, 0x8000, false, false)
                .unwrap()
                .0,
            0x8400
        );
        assert_eq!(
            arm64_el1_exception_entry(5, 0, 0x8000, false, false)
                .unwrap()
                .0,
            0x8200
        );
        assert!(arm64_el1_exception_entry(9, 0, 0x8000, true, false).is_err());
    }
}

/// Architectural scalar registers exposed by the paused debugger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Register {
    X(u8),
    Pc,
    Sp,
    Pstate,
    Ttbr0,
    Ttbr1,
    Tcr,
    Sctlr,
    Vbar,
}
impl Register {
    pub fn parse(name: &str) -> crate::Result<Self> {
        Ok(match name.to_ascii_lowercase().as_str() {
            "pc" => Self::Pc,
            "sp" => Self::Sp,
            "pstate" | "cpsr" => Self::Pstate,
            "fp" => Self::X(29),
            "lr" => Self::X(30),
            "ttbr0" => Self::Ttbr0,
            "ttbr1" => Self::Ttbr1,
            "tcr" => Self::Tcr,
            "sctlr" => Self::Sctlr,
            "vbar" => Self::Vbar,
            name => match name.strip_prefix('x').and_then(|n| n.parse::<u8>().ok()) {
                Some(n @ 0..=30) => Self::X(n),
                _ => return Err(crate::Error::Invalid("unknown ARM64 register".into())),
            },
        })
    }
    pub fn validate(self, value: u64) -> crate::Result<()> {
        let valid = match self {
            Self::X(n) => n <= 30,
            Self::Pc => value & 3 == 0,
            Self::Sp => value & 15 == 0,
            Self::Vbar => value & 2047 == 0,
            Self::Pstate => {
                matches!(value & 31, 0 | 4 | 5) && value >> 32 == 0 && value & (1 << 21) == 0
            }
            _ => true,
        };
        if valid {
            Ok(())
        } else {
            Err(crate::Error::Invalid(
                "invalid register value/alignment or CPU mode".into(),
            ))
        }
    }
}

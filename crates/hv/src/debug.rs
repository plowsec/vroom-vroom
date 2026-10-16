//! Small, OS-independent debugger controls. No debugger transport or UI.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DebugConfig {
    pub enabled: bool,
    /// Arm one instruction step; the caller must rearm after each stop.
    pub single_step: bool,
    /// Execution addresses, applied to this CPU. ARM64 addresses must be aligned.
    pub breakpoints: Vec<u64>,
    /// Stop on guest BRK instructions. Otherwise deliver them to the guest
    /// exception vector, so its own debug services keep working.
    pub trap_software_breakpoints: bool,
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

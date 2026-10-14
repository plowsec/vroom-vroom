//! PSCI 1.0 over HVC. Pure logic: the vCPU threads apply the outcome.

use bulwark_hv::aarch64::{mpidr_affinity, psci::*};

/// Power state of every vCPU, indexed like the vCPU threads.
#[derive(Debug, Clone)]
pub struct CpuTable {
    mpidrs: Vec<u64>,
    on: Vec<bool>,
}

impl CpuTable {
    pub fn new(mpidrs: Vec<u64>) -> Self {
        let n = mpidrs.len();
        let mut on = vec![false; n];
        if n > 0 {
            on[0] = true;
        }
        CpuTable { mpidrs, on }
    }

    fn find(&self, mpidr: u64) -> Option<usize> {
        let want = mpidr_affinity(mpidr);
        self.mpidrs.iter().position(|&m| mpidr_affinity(m) == want)
    }

    pub fn is_on(&self, i: usize) -> bool {
        self.on.get(i).copied().unwrap_or(false)
    }

    pub fn set_on(&mut self, i: usize, v: bool) {
        if let Some(s) = self.on.get_mut(i) {
            *s = v;
        }
    }

    /// Reset to "only the boot CPU is on".
    pub fn reset(&mut self) {
        for (i, s) in self.on.iter_mut().enumerate() {
            *s = i == 0;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PsciOutcome {
    /// Put this in X0 and continue.
    Return(i64),
    /// Start `target` at `entry` with X0 = `ctx`. Caller gets SUCCESS.
    CpuOn { target: usize, entry: u64, ctx: u64 },
    /// Calling CPU powers down.
    CpuOff,
    SystemOff,
    SystemReset,
}

pub fn handle(func: u64, args: [u64; 3], caller: usize, t: &mut CpuTable) -> PsciOutcome {
    use PsciOutcome::*;
    // SMC32 ids live in the low 32 bits; SMC64 ids too.
    let f = func as u32;
    match f {
        VERSION => Return(0x1_0000),
        SMCCC_VERSION => Return(0x1_0001),
        FEATURES => {
            let q = args[0] as u32;
            let known = [
                VERSION, CPU_SUSPEND_32, CPU_SUSPEND_64, CPU_OFF, CPU_ON_32, CPU_ON_64,
                AFFINITY_INFO_32, AFFINITY_INFO_64, MIGRATE_INFO_TYPE, SYSTEM_OFF, SYSTEM_RESET,
                FEATURES, SMCCC_VERSION,
            ];
            Return(if known.contains(&q) { SUCCESS } else { NOT_SUPPORTED })
        }
        CPU_ON_32 | CPU_ON_64 => {
            let (mpidr, entry, ctx) = if f == CPU_ON_32 {
                (args[0] & 0xffff_ffff, args[1] & 0xffff_ffff, args[2] & 0xffff_ffff)
            } else {
                (args[0], args[1], args[2])
            };
            match t.find(mpidr) {
                None => Return(INVALID_PARAMS),
                Some(i) if t.is_on(i) => Return(ALREADY_ON),
                Some(i) => {
                    t.set_on(i, true);
                    CpuOn { target: i, entry, ctx }
                }
            }
        }
        CPU_OFF => {
            t.set_on(caller, false);
            CpuOff
        }
        AFFINITY_INFO_32 | AFFINITY_INFO_64 => {
            if args[1] != 0 {
                return Return(INVALID_PARAMS);
            }
            match t.find(args[0]) {
                None => Return(INVALID_PARAMS),
                Some(i) => Return(if t.is_on(i) { 0 } else { 1 }),
            }
        }
        // "Trusted OS not present, no migration needed".
        MIGRATE_INFO_TYPE => Return(2),
        // Standby: treat as a no-op; the guest WFIs after.
        CPU_SUSPEND_32 | CPU_SUSPEND_64 => Return(SUCCESS),
        SYSTEM_OFF => SystemOff,
        SYSTEM_RESET => SystemReset,
        _ => Return(NOT_SUPPORTED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bulwark_hv::aarch64::mpidr_for;

    fn table(n: u32) -> CpuTable {
        CpuTable::new((0..n).map(mpidr_for).collect())
    }

    #[test]
    fn cpu_on_lifecycle() {
        let mut t = table(4);
        let m2 = mpidr_for(2);
        assert_eq!(
            handle(CPU_ON_64 as u64, [m2, 0x4008_0000, 7], 0, &mut t),
            PsciOutcome::CpuOn { target: 2, entry: 0x4008_0000, ctx: 7 }
        );
        assert_eq!(handle(CPU_ON_64 as u64, [m2, 0, 0], 0, &mut t), PsciOutcome::Return(ALREADY_ON));
        assert_eq!(handle(AFFINITY_INFO_64 as u64, [m2, 0, 0], 0, &mut t), PsciOutcome::Return(0));
        assert_eq!(handle(CPU_OFF as u64, [0; 3], 2, &mut t), PsciOutcome::CpuOff);
        assert_eq!(handle(AFFINITY_INFO_64 as u64, [m2, 0, 0], 0, &mut t), PsciOutcome::Return(1));
        assert_eq!(handle(CPU_ON_64 as u64, [0x99, 0, 0], 0, &mut t), PsciOutcome::Return(INVALID_PARAMS));
    }

    #[test]
    fn misc_calls() {
        let mut t = table(1);
        assert_eq!(handle(VERSION as u64, [0; 3], 0, &mut t), PsciOutcome::Return(0x10000));
        assert_eq!(handle(SYSTEM_OFF as u64, [0; 3], 0, &mut t), PsciOutcome::SystemOff);
        assert_eq!(handle(0xdead, [0; 3], 0, &mut t), PsciOutcome::Return(NOT_SUPPORTED));
        assert_eq!(handle(FEATURES as u64, [CPU_ON_64 as u64, 0, 0], 0, &mut t), PsciOutcome::Return(0));
        // SMC64 id with junk in the upper half still decodes.
        assert_eq!(handle(0xffff_ffff_0000_0000 | SYSTEM_RESET as u64, [0; 3], 0, &mut t), PsciOutcome::SystemReset);
    }
}

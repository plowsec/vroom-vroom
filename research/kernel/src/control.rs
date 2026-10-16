//! Pure ARM64 stepping policy; execution and all-stop live in the lab session.
use crate::arm64::Registers;

pub fn is_call(instruction: u32) -> bool {
    // BL, BLR, BLRAA/BLRAB and their zero-modifier forms.
    instruction & 0xfc00_0000 == 0x9400_0000
        || instruction & 0xffff_fc1f == 0xd63f_0000
        || instruction & 0xffff_f800 == 0xd73f_0800
        || instruction & 0xffff_f81f == 0xd63f_081f
}

#[derive(Debug)]
pub struct RunTo {
    pub address: u64,
    pub sp: u64,
    pub ttbr0: u64,
    pub reason: &'static str,
}
impl RunTo {
    pub fn matches(&self, r: &Registers) -> bool {
        self.address == r.pc
            && self.sp == r.sp
            && self.ttbr0 & 0x0000_ffff_ffff_f000 == r.ttbr0 & 0x0000_ffff_ffff_f000
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn call_decoder_does_not_treat_branches_or_syscalls_as_calls() {
        for op in [
            0x94000001, 0x97ffffff, 0xd63f0220, 0xd73f0a20, 0xd73f0e20, 0xd63f0a3f, 0xd63f0e3f,
        ] {
            assert!(is_call(op), "{op:x}");
        }
        for op in [0x14000001, 0xd61f0220, 0xd65f03c0, 0xd4000021, 0xd503201f] {
            assert!(!is_call(op), "{op:x}");
        }
    }
    #[test]
    fn return_breakpoints_ignore_recursion_and_other_address_spaces() {
        let mut r = Registers {
            cpu: 1,
            x: [0; 31],
            pc: 0x1000,
            sp: 0x8000,
            pstate: 5,
            ttbr0: 0xabcd000000002000,
            ttbr1: 0,
            tcr: 0,
            sctlr: 0,
            vbar: 0,
            sve_vl: None,
        };
        let target = RunTo {
            address: r.pc,
            sp: r.sp,
            ttbr0: 0x2000,
            reason: "StepOver",
        };
        assert!(target.matches(&r));
        r.sp -= 0x40;
        assert!(!target.matches(&r));
        r.sp += 0x40;
        r.ttbr0 = 0x3000;
        assert!(!target.matches(&r));
    }
}

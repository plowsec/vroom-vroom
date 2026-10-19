//! Comparison logging (Redqueen / AFL++ cmplog style): the operands of the
//! comparisons an input reaches, and inputs that would turn them the other
//! way. Exact values such as IOCTL codes and magic numbers, which random
//! mutation never hits, come out of the code itself.

use std::time::Duration;

use vroom_hv::debug::{DebugConfig, DebugReason};
use vroom_kernel::arm64::{AddressSpace, Registers};
use vroom_vmm::VmEvent;

use crate::{elr_el1, is_exception_vector, step_debug_config, Engine, Error, Result};

/// One comparison, as the CPU was about to run it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cmp {
    pub pc: u64,
    /// The register side.
    pub a: u64,
    /// What it is compared with (register, immediate; for a bit test, `a`
    /// with that bit flipped).
    pub b: u64,
    /// Operand width.
    pub bits: u8,
}

fn reg(r: &Registers, n: u32, sp: bool) -> u64 {
    match n {
        31 if sp => r.sp,
        31 => 0,
        n => r.x[n as usize],
    }
}

fn mask(v: u64, bits: u8) -> u64 {
    if bits == 32 { v & 0xffff_ffff } else { v }
}

fn shifted(v: u64, kind: u32, amount: u32, bits: u8) -> u64 {
    let v = mask(v, bits);
    let amount = amount % u32::from(bits);
    mask(
        match kind {
            0 => v << amount,
            1 => v >> amount,
            2 if bits == 32 => ((v as u32 as i32) >> amount) as u32 as u64,
            2 => ((v as i64) >> amount) as u64,
            _ if bits == 32 => (v as u32).rotate_right(amount) as u64,
            _ => v.rotate_right(amount),
        },
        bits,
    )
}

fn extended(v: u64, option: u32, shift: u32, bits: u8) -> u64 {
    let x = match option {
        0 => v & 0xff,
        1 => v & 0xffff,
        2 => v & 0xffff_ffff,
        3 => v,
        4 => v as u8 as i8 as i64 as u64,
        5 => v as u16 as i16 as i64 as u64,
        6 => v as u32 as i32 as i64 as u64,
        _ => v,
    };
    mask(x << shift, bits)
}

/// The comparison `word` makes with `r`'s registers, if it is one.
pub fn decode(word: u32, pc: u64, r: &Registers) -> Option<Cmp> {
    let bits = if word >> 31 == 1 { 64 } else { 32 };
    let rn = (word >> 5) & 31;
    let rm = (word >> 16) & 31;
    let cmp = |a: u64, b: u64| Some(Cmp { pc, a: mask(a, bits), b: mask(b, bits), bits });
    let negated = |v: u64| mask(v.wrapping_neg(), bits);
    match word & 0x7f80_0000 {
        // SUBS / ADDS (immediate): CMP / CMN.
        0x7100_0000 | 0x3100_0000 => {
            let imm = u64::from((word >> 10) & 0xfff) << if word & (1 << 22) != 0 { 12 } else { 0 };
            let b = if word & 0x4000_0000 != 0 { imm } else { negated(imm) };
            return cmp(reg(r, rn, true), b);
        }
        _ => {}
    }
    match word & 0x7f20_0000 {
        // SUBS / ADDS (shifted register).
        0x6b00_0000 | 0x2b00_0000 => {
            let m = shifted(reg(r, rm, false), (word >> 22) & 3, (word >> 10) & 63, bits);
            let b = if word & 0x4000_0000 != 0 { m } else { negated(m) };
            return cmp(reg(r, rn, false), b);
        }
        // SUBS / ADDS (extended register).
        0x6b20_0000 | 0x2b20_0000 if word & 0x00c0_0000 == 0 => {
            let m = extended(reg(r, rm, false), (word >> 13) & 7, (word >> 10) & 7, bits);
            let b = if word & 0x4000_0000 != 0 { m } else { negated(m) };
            return cmp(reg(r, rn, true), b);
        }
        _ => {}
    }
    match word & 0x7fe0_0c10 {
        // CCMP / CCMN (register).
        0x7a40_0000 => return cmp(reg(r, rn, false), reg(r, rm, false)),
        0x3a40_0000 => return cmp(reg(r, rn, false), negated(reg(r, rm, false))),
        // CCMP / CCMN (immediate).
        0x7a40_0800 => return cmp(reg(r, rn, false), u64::from(rm)),
        0x3a40_0800 => return cmp(reg(r, rn, false), negated(u64::from(rm))),
        _ => {}
    }
    match word & 0x7e00_0000 {
        // CBZ / CBNZ.
        0x3400_0000 => cmp(reg(r, word & 31, false), 0),
        // TBZ / TBNZ: the other value of that bit.
        0x3600_0000 => {
            let bit = ((word >> 26) & 0x20) | ((word >> 19) & 0x1f);
            let a = reg(r, word & 31, false);
            Some(Cmp { pc, a, b: a ^ (1 << bit), bits: 64 })
        }
        _ => None,
    }
}

/// Inputs that would make `cmps` go the other way, at most `cap`:
/// - an operand's bytes found in the input (4- or 8-byte, not tiny values)
///   replaced by the other operand, and by it ±1 (for `<`, `>`);
/// - each 4-byte field at `fields` shifted by the operands' difference
///   (and ±1): a field the code transforms before comparing (`code -
///   0x222000 == 3`) lands on the value that matches.
pub fn candidates(input: &[u8], cmps: &[Cmp], fields: &[usize], cap: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |c: Vec<u8>, out: &mut Vec<Vec<u8>>| {
        if c != input && seen.insert(c.clone()) && out.len() < cap {
            out.push(c);
        }
    };
    let mut unique: Vec<Cmp> = Vec::new();
    for c in cmps {
        if c.a != c.b && !unique.iter().any(|u| (u.a, u.b, u.bits) == (c.a, c.b, c.bits)) {
            unique.push(*c);
        }
    }
    // Direct matches first: they are the precise ones.
    for c in &unique {
        for (from, to) in [(c.a, c.b), (c.b, c.a)] {
            if from <= 0xff {
                continue;
            }
            let width = if c.bits == 64 && from > u64::from(u32::MAX) { 8 } else { 4 };
            let needle = &from.to_le_bytes()[..width];
            let mut at = 0;
            while let Some(i) = input[at..].windows(width).position(|w| w == needle).map(|i| i + at) {
                for t in [to, to.wrapping_add(1), to.wrapping_sub(1)] {
                    let mut x = input.to_vec();
                    x[i..i + width].copy_from_slice(&t.to_le_bytes()[..width]);
                    push(x, &mut out);
                }
                at = i + 1;
            }
        }
    }
    for &f in fields {
        if f + 4 > input.len() {
            continue;
        }
        let v = u32::from_le_bytes(input[f..f + 4].try_into().expect("four bytes"));
        for c in &unique {
            let delta = (c.b as u32).wrapping_sub(c.a as u32);
            for t in [0u32, 1, u32::MAX] {
                let mut x = input.to_vec();
                x[f..f + 4].copy_from_slice(&v.wrapping_add(delta).wrapping_add(t).to_le_bytes());
                push(x, &mut out);
            }
        }
    }
    out
}

fn is_call(word: u32) -> bool {
    word & 0xfc00_0000 == 0x9400_0000 || word & 0xffff_fc1f == 0xd63f_0000
}

/// How long a callee outside the target, or an exception handler, gets to
/// come back.
const RUN_UNTIL: Duration = Duration::from_secs(2);

impl Engine {
    fn registers_of(&self, cpu: usize) -> Result<(vroom_hv::VcpuState, Registers)> {
        let state = self.vm.debug_registers(cpu)?;
        let regs = Registers::from_state(&state).map_err(|e| Error::Invalid(format!("ARM64 registers: {e}")))?;
        Ok((state, regs))
    }

    /// The next stop of `cpu` (the VM paused again), or None if it didn't
    /// stop in `wait` or the VM ended.
    fn stop_of(&mut self, cpu: usize, wait: Duration) -> Result<Option<vroom_hv::debug::DebugStop>> {
        let stop = loop {
            match self.vm.next_event(wait) {
                Some(VmEvent::Debug(stop)) if stop.cpu as usize == cpu => break Some(stop),
                Some(VmEvent::Debug(_) | VmEvent::Ui(_) | VmEvent::ViewerClosed | VmEvent::ViewerAttached) => {}
                _ => break None,
            }
        };
        self.vm.pause()?;
        Ok(stop)
    }

    /// Let `cpu` run until it reaches `address` (a call's return, the
    /// instruction an exception interrupted). False if it ended elsewhere
    /// (stop_pc, the bugcheck, too long).
    fn run_until(&mut self, cpu: usize, address: u64) -> Result<bool> {
        let mut breakpoints = vec![address];
        breakpoints.extend(self.harness.stop_pc);
        breakpoints.extend(self.harness.bugcheck_pc);
        let config = DebugConfig { enabled: true, breakpoints, trap_brk_immediate: self.always().brk, ..Default::default() };
        self.vm.debug_configure(cpu, config).map_err(|e| Error::Invalid(format!("cmplog: breakpoint at {address:#x}: {e}")))?;
        self.vm.debug_resume_cpu(cpu).map_err(|e| Error::Invalid(format!("cmplog: resume: {e}")))?;
        let stop = self.stop_of(cpu, RUN_UNTIL)?;
        if std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
            eprintln!("cmplog: run until {address:#x}: {stop:?}");
        }
        Ok(stop.is_some_and(|s| s.pc == address && s.reason == DebugReason::Breakpoint))
    }

    /// The comparisons `input` makes in the target's own code (the image
    /// `entry_pc` is in), from the snapshot until the target returns, the
    /// bugcheck, `stop_pc`, `max_steps` instructions or `budget` (a step
    /// costs ~0.5 ms: a copy loop would eat a fuzzing session). Calls into
    /// other images and exception handlers run at full speed: only the
    /// target is stepped.
    pub fn compare_log(&mut self, input: &[u8], max_steps: usize, budget: Duration) -> Result<Vec<Cmp>> {
        let started = std::time::Instant::now();
        self.load_case(input)?;
        let cpu = self.harness.cpu;
        let (_, first) = self.registers_of(cpu)?;
        if self.harness.isolate_cpu {
            // As a run: nothing preempts the testcase's thread.
            self.vm
                .debug_write_register(cpu, vroom_hv::debug::Register::Pstate, crate::isolated_pstate(first.pstate))
                .map_err(|e| Error::Invalid(format!("cmplog: mask interrupts: {e}")))?;
        }
        let (lo, hi) = self.target_range(&first)?;
        let always = self.always();
        let mut cmps = Vec::new();
        for _ in 0..max_steps {
            if started.elapsed() > budget {
                break;
            }
            let (_, regs) = self.registers_of(cpu)?;
            let pc = regs.pc;
            if !(lo..hi).contains(&pc) || self.harness.stop_pc == Some(pc) || self.harness.bugcheck_pc == Some(pc) {
                if std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
                    eprintln!("cmplog: done at {pc:#x} (target {lo:#x}..{hi:#x})");
                }
                break;
            }
            let mut bytes = [0u8; 4];
            let space = AddressSpace { memory: &self.vm, registers: &regs };
            // Not mapped yet (pageable code): the step faults it in, and
            // the instruction runs (and is read) after the handler.
            let word = space.read(pc, &mut bytes).ok().map(|()| u32::from_le_bytes(bytes));
            if let Some(c) = word.and_then(|w| decode(w, pc, &regs)) {
                cmps.push(c);
            }
            self.vm
                .debug_step(cpu, step_debug_config(&[], always))
                .map_err(|e| Error::Invalid(format!("cmplog: step at {pc:#x}: {e}")))?;
            let Some(stop) = self.stop_of(cpu, RUN_UNTIL)? else {
                if std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
                    eprintln!("cmplog: the step at {pc:#x} didn't finish");
                }
                break;
            };
            if stop.reason == DebugReason::SoftwareBreakpoint {
                // A coverage BRK (blocks mode): this input reaches the block;
                // its instruction runs on the next step.
                if let Some(&index) = self.block_by_pc.get(&stop.pc) {
                    if self.unplant(index)? {
                        let block = self.harness.coverage[index].clone();
                        self.coverage.observe(&block, "cmplog");
                    }
                    continue;
                }
                if std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
                    eprintln!("cmplog: a guest BRK at {:#x}", stop.pc);
                }
                break;
            }
            let (state, after) = self.registers_of(cpu)?;
            let back = if is_exception_vector(after.pc, after.vbar) {
                // A fault (or an interrupt) before the instruction ran.
                match elr_el1(&state) {
                    Some(elr) => self.run_until(cpu, elr)?,
                    None => false,
                }
            } else if word.is_some_and(is_call) && !(lo..hi).contains(&after.pc) {
                self.run_until(cpu, pc + 4)?
            } else {
                true
            };
            if !back {
                if std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
                    eprintln!("cmplog: lost the thread after {pc:#x} (now {:#x})", after.pc);
                }
                break;
            }
        }
        // The reset doesn't clear a pending software step (PSTATE.SS): a
        // run without isolation would trap on its first instruction.
        let (_, last) = self.registers_of(cpu)?;
        if last.pstate & (1 << 21) != 0 {
            self.vm.debug_write_register(cpu, vroom_hv::debug::Register::Pstate, last.pstate & !(1 << 21))?;
        }
        Ok(cmps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regs(x: &[(usize, u64)]) -> Registers {
        let mut r = Registers { cpu: 0, x: [0; 31], pc: 0, sp: 0x1000, pstate: 0, ttbr0: 0, ttbr1: 0, tcr: 0, sctlr: 0, vbar: 0, sve_vl: None };
        for &(i, v) in x {
            r.x[i] = v;
        }
        r
    }

    #[test]
    fn decodes_hevd_dispatch_comparisons() {
        // cmp w3, w8   (w3 = the IOCTL, w8 = a literal)
        let r = regs(&[(3, 0x222003), (8, 0x22203b)]);
        assert_eq!(decode(0x6b08_007f, 0, &r), Some(Cmp { pc: 0, a: 0x222003, b: 0x22203b, bits: 32 }));
        // cmp w8, #0x3b   (w8 = IOCTL - 0x222000)
        let r = regs(&[(8, 3)]);
        assert_eq!(decode(0x7100_ed1f, 0, &r), Some(Cmp { pc: 0, a: 3, b: 0x3b, bits: 32 }));
        // sub w8, w3, #0x222, lsl #12 isn't a comparison.
        assert_eq!(decode(0x5148_8868, 0, &r), None);
        // cbz x19
        let r = regs(&[(19, 0xffff)]);
        assert_eq!(decode(0xb400_3953, 0, &r), Some(Cmp { pc: 0, a: 0xffff, b: 0, bits: 64 }));
        // cmn w1, #1 is cmp w1, #-1
        let r = regs(&[(1, 5)]);
        assert_eq!(decode(0x3100_043f, 0, &r).map(|c| c.b), Some(0xffff_ffff));
    }

    #[test]
    fn a_transformed_field_lands_on_the_compared_value() {
        let input = [0x03, 0x20, 0x22, 0x00, b'A', b'A'];
        // The code compared (IOCTL - 0x222000) = 3 with 0x3b.
        let c = candidates(&input, &[Cmp { pc: 0, a: 3, b: 0x3b, bits: 32 }], &[0], 64);
        assert!(c.iter().any(|x| x[..4] == 0x22203bu32.to_le_bytes()), "{c:x?}");
    }

    #[test]
    fn an_operand_in_the_input_is_replaced() {
        let mut input = 0x222003u32.to_le_bytes().to_vec();
        input.extend_from_slice(b"AAAA");
        let c = candidates(&input, &[Cmp { pc: 0, a: 0x222003, b: 0x22203b, bits: 32 }], &[], 64);
        for v in [0x22203bu32, 0x22203c, 0x22203a] {
            assert!(c.iter().any(|x| x[..4] == v.to_le_bytes()), "{v:#x}");
        }
        assert_eq!(c.len(), 3, "and nothing else: {c:x?}");
    }
}

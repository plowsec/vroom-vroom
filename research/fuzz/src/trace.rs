//! `vroom-fuzz trace`: one input again, from the snapshot, one instruction
//! at a time, each named (`module!function+0xoff`), then how it ended, the
//! call tree and the stack there. Like what-the-fuzz's rip traces.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use vroom_hv::debug::DebugConfig;
use vroom_kernel::arm64::{AddressSpace, Registers};
use vroom_kernel::symbolize::Symbolizer;
use vroom_vmm::VmEvent;

use crate::{is_exception_vector, Engine, Error, Result};

#[derive(Debug, Clone)]
pub struct TraceOptions {
    /// Instructions at most.
    pub max: u64,
    /// The general registers after each instruction's line.
    pub registers: bool,
    /// PDB cache (the symbol server fills it).
    pub symbols: PathBuf,
    /// Function names by start address, for modules without a PDB.
    pub names: Vec<(u64, String)>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TraceSummary {
    pub end: String,
    pub instructions: u64,
    /// Calls, returns folded: one line per callee, indented by depth.
    pub calls: Vec<String>,
    pub stack: Vec<String>,
}

/// What an instruction does to the call depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Call,
    Return,
    ExceptionReturn,
    Other,
}

fn flow(word: u32) -> Flow {
    if word & 0xfc00_0000 == 0x9400_0000 || word & 0xffff_fc1f == 0xd63f_0000 {
        Flow::Call // BL, BLR
    } else if word & 0xffff_fc1f == 0xd65f_0000 || word & 0xffff_fbff == 0xd65f_0bff {
        Flow::Return // RET, RETAA/RETAB
    } else if word == 0xd69f_03e0 || word & 0xffff_fbff == 0xd69f_0bff {
        Flow::ExceptionReturn // ERET, ERETAA/ERETAB
    } else {
        Flow::Other
    }
}

/// The call tree, consecutive repeats folded (`×N`).
#[derive(Debug, Default)]
struct Calls {
    depth: usize,
    lines: Vec<(usize, String, u64)>,
}

impl Calls {
    fn enter(&mut self, what: String) {
        self.depth += 1;
        match self.lines.last_mut() {
            Some((d, l, n)) if *d == self.depth && *l == what => *n += 1,
            _ => self.lines.push((self.depth, what, 1)),
        }
    }
    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }
    fn render(&self) -> Vec<String> {
        let base = self.lines.iter().map(|(d, _, _)| *d).min().unwrap_or(0);
        self.lines
            .iter()
            .map(|(d, l, n)| {
                let pad = "  ".repeat(d - base);
                if *n > 1 { format!("{pad}{l} ×{n}") } else { format!("{pad}{l}") }
            })
            .collect()
    }
}

fn registers(engine: &Engine) -> Result<(vroom_hv::VcpuState, Registers)> {
    let state = engine.vm.debug_registers(engine.harness.cpu)?;
    let regs = Registers::from_state(&state).map_err(|e| Error::Invalid(format!("ARM64 registers: {e}")))?;
    Ok((state, regs))
}

impl Engine {
    /// Replay `input` like a run, but single-step its CPU and write every
    /// instruction to `out`. Ends at `stop_pc`, the bugcheck, `max`
    /// instructions, or when the VM stops.
    pub fn trace(&mut self, input: &[u8], out: &mut dyn Write, options: &TraceOptions) -> Result<TraceSummary> {
        self.load_case(input)?;
        let cpu = self.harness.cpu;
        let (_, first) = registers(self)?;
        if self.harness.isolate_cpu {
            // As a run: nothing preempts the testcase's thread.
            self.vm.debug_write_register(cpu, vroom_hv::debug::Register::Pstate, first.pstate | 0xc0)?;
        }
        let mut symbols = {
            let space = AddressSpace { memory: &self.vm, registers: &first };
            Symbolizer::new(&space, &first, &options.symbols).map_err(|e| Error::Invalid(format!("symbols: {e}")))?
        };
        symbols.add_names(options.names.iter().cloned());
        let mut calls = Calls::default();
        let mut count = 0u64;
        let end = loop {
            let (_, regs) = registers(self)?;
            let space = AddressSpace { memory: &self.vm, registers: &regs };
            let pc = regs.pc;
            let name = symbols.name(&space, pc);
            if count == 0 {
                calls.enter(name.clone());
            }
            write!(out, "{count:>8} {pc:#018x} {name}")?;
            if options.registers {
                for (i, x) in regs.x.iter().enumerate() {
                    write!(out, " x{i}={x:#x}")?;
                }
                write!(out, " sp={:#x} pstate={:#x}", regs.sp, regs.pstate)?;
            }
            writeln!(out)?;
            if self.harness.stop_pc == Some(pc) {
                break "reached stop_pc".to_string();
            }
            if self.harness.bugcheck_pc == Some(pc) {
                let x = &regs.x;
                break format!("bugcheck {:#x} ({:#x}, {:#x}, {:#x}, {:#x}) from {:#x}", x[0], x[1], x[2], x[3], x[4], x[30]);
            }
            if count >= options.max {
                break format!("{} instructions (--max)", options.max);
            }
            let mut word = [0u8; 4];
            let kind = space.read(pc, &mut word).map_or(Flow::Other, |_| flow(u32::from_le_bytes(word)));
            // One instruction, this CPU only.
            self.vm.debug_step(cpu, DebugConfig::default())?;
            let stopped = loop {
                match self.vm.next_event(Duration::from_secs(5)) {
                    Some(VmEvent::Debug(stop)) if stop.cpu as usize == cpu => break None,
                    Some(VmEvent::Debug(_)) | Some(VmEvent::Ui(_)) | Some(VmEvent::ViewerClosed) | Some(VmEvent::ViewerAttached) => {}
                    Some(VmEvent::Shutdown) => break Some("the guest shut down".to_string()),
                    Some(VmEvent::Reset) => break Some("the guest reset".to_string()),
                    Some(VmEvent::Crashed(e)) => break Some(format!("the VM failed: {e}")),
                    None => break Some(format!("the step at {pc:#x} didn't finish (the CPU waits on something)")),
                }
            };
            self.vm.pause()?;
            count += 1;
            if let Some(why) = stopped {
                break why;
            }
            let (_, after) = registers(self)?;
            let space = AddressSpace { memory: &self.vm, registers: &after };
            if is_exception_vector(after.pc, after.vbar) {
                // The stepped instruction faulted (or an exception came in
                // first): the handler runs now, traced like the rest.
                let into = symbols.name(&space, after.pc);
                calls.enter(format!("exception at {} -> {into}", symbols.name(&space, pc)));
            } else {
                match kind {
                    Flow::Call => {
                        let into = symbols.name(&space, after.pc);
                        calls.enter(into);
                    }
                    Flow::Return | Flow::ExceptionReturn => calls.leave(),
                    Flow::Other => {}
                }
            }
        };
        let (_, last) = registers(self)?;
        let space = AddressSpace { memory: &self.vm, registers: &last };
        let stack = symbols.stack(&space, &last);
        let summary = TraceSummary { end, instructions: count, calls: calls.render(), stack };
        writeln!(out, "\n# end: {} after {} instructions", summary.end, summary.instructions)?;
        writeln!(out, "\n# calls")?;
        for l in &summary.calls {
            writeln!(out, "{l}")?;
        }
        writeln!(out, "\n# stack")?;
        for (i, f) in summary.stack.iter().enumerate() {
            writeln!(out, "{i:>3} {f}")?;
        }
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_and_returns_are_told_apart() {
        assert_eq!(flow(0x9400_0010), Flow::Call); // bl
        assert_eq!(flow(0xd63f_0100), Flow::Call); // blr x8
        assert_eq!(flow(0xd65f_03c0), Flow::Return); // ret
        assert_eq!(flow(0xd65f_0bff), Flow::Return); // retaa
        assert_eq!(flow(0xd69f_03e0), Flow::ExceptionReturn); // eret
        assert_eq!(flow(0x1400_0010), Flow::Other); // b
        assert_eq!(flow(0xd61f_0100), Flow::Other); // br x8
    }

    #[test]
    fn repeated_calls_fold() {
        let mut c = Calls::default();
        c.enter("nt!A".into());
        for _ in 0..3 {
            c.enter("nt!B".into());
            c.leave();
        }
        c.enter("nt!C".into());
        assert_eq!(c.render(), ["nt!A", "  nt!B ×3", "  nt!C"]);
    }
}

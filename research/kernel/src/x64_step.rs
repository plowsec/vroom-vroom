//! AMD64 call decoding, image lookup, and temporary return traps.
//! Caller recovery uses the shared analysis unwinder, never a stack scan.
use super::*;
use vroom_hv::debug::DebugStop;
use vroom_kernel::windows::VirtualMemory;

const CR3_MASK: u64 = 0x000f_ffff_ffff_f000;

#[derive(Debug)]
pub(super) struct RunTo {
    pub(super) address: u64,
    cpu: u32,
    sp: u64,
    cr3: u64,
    reason: &'static str,
}
impl RunTo {
    fn matches(&self, r: &Registers) -> bool {
        self.cpu == r.cpu
            && self.address == r.regs.rip
            && self.sp == r.regs.rsp
            && self.cr3 & CR3_MASK == r.sregs.cr3 & CR3_MASK
    }
}

fn add(at: u64, n: u64) -> Result<u64, String> {
    at.checked_add(n)
        .filter(|&v| canonical(v))
        .ok_or_else(|| "stepping address overflow".into())
}
fn read<const N: usize>(m: &impl VirtualMemory, at: u64) -> Result<[u8; N], String> {
    let mut bytes = [0; N];
    m.read(at, &mut bytes).map_err(string)?;
    Ok(bytes)
}

/// Decode only near CALLs in 64-bit mode. Everything else is single-stepped.
/// ModRM/SIB lengths include address-size overrides; instruction size is <=15.
fn call_len(code: &[u8]) -> Result<Option<usize>, String> {
    let byte = |i: usize| {
        code.get(i)
            .copied()
            .ok_or_else(|| "truncated AMD64 instruction".to_string())
    };
    let mut i = 0;
    let mut lock = false;
    loop {
        match byte(i)? {
            0xf0 => lock = true,
            0x26 | 0x2e | 0x36 | 0x3e | 0x64 | 0x65 | 0x66 | 0x67 | 0xf2 | 0xf3 | 0x40..=0x4f => {}
            _ => break,
        }
        i += 1;
        if i >= 15 {
            return Err("AMD64 instruction exceeds 15 bytes".into());
        }
    }
    match byte(i)? {
        0xe8 => i += 5,
        0xff => {
            i += 1;
            let modrm = byte(i)?;
            if (modrm >> 3) & 7 != 2 {
                return Ok(None);
            }
            i += 1;
            let mode = modrm >> 6;
            let rm = modrm & 7;
            if mode != 3 && rm == 4 {
                let sib = byte(i)?;
                i += 1;
                if mode == 0 && sib & 7 == 5 {
                    i += 4;
                }
            } else if mode == 0 && rm == 5 {
                i += 4;
            }
            i += match mode {
                1 => 1,
                2 => 4,
                _ => 0,
            };
        }
        _ => return Ok(None),
    }
    if lock {
        return Err("LOCK CALL is not a valid AMD64 instruction".into());
    }
    if i > 15 || i > code.len() {
        return Err("truncated or oversized AMD64 CALL".into());
    }
    Ok(Some(i))
}

/// Do not require all 15 bytes to be mapped (a CALL may end at a page boundary).
fn code(m: &impl VirtualMemory, pc: u64) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for i in 0..15 {
        match add(pc, i).and_then(|at| read::<1>(m, at)) {
            Ok(b) => out.push(b[0]),
            Err(e) if out.is_empty() => return Err(e),
            Err(_) => break,
        }
    }
    Ok(out)
}

fn caller(m: &impl VirtualMemory, r: &Registers) -> Result<(u64, u64), String> {
    let top = r.regs.rip & !0xffff;
    for n in 0..4096 {
        let Some(base) = top.checked_sub(n * 65536) else {
            break;
        };
        if read::<2>(m, base).is_ok_and(|b| b == *b"MZ") {
            if let Ok(image) = Image::read_amd64(m, base) {
                if r.regs.rip - base < image.size as u64 {
                    return super::analysis::caller_in_image(m, r, base);
                }
            }
        }
    }
    Err("no AMD64 PE image for current PC within 256 MiB; step_out requires Windows unwind metadata".into())
}

impl Session {
    pub(super) fn cancel_step(&mut self) -> Result<(), String> {
        // Preserve the old DR-slot layout before removing the private target.
        // An acknowledged pause can still have debug exits queued for delivery.
        self.run_to = None;
        self.debug(&self.breakpoints, &self.watches)
    }
    fn resume_return(&mut self) -> Result<(), String> {
        let mut points = self.breakpoints.clone();
        if let Some(run) = &self.run_to {
            if !points.contains(&run.address) {
                points.push(run.address);
            }
        }
        self.debug(&points, &self.watches)?;
        for cpu in 0..self.vm.config().cpus as usize {
            let r = self.registers(cpu, false)?;
            if points.contains(&r.regs.rip)
                || self.stopped.get(&cpu) == Some(&DebugReason::Breakpoint)
            {
                self.vm
                    .debug_write_register(
                        cpu,
                        Register::X86(X86Register::Rflags),
                        r.regs.rflags | (1 << 16),
                    )
                    .map_err(string)?;
            }
        }
        self.stopped.clear();
        self.vm.resume().map_err(string)
    }
    pub(super) fn step_return(
        &mut self,
        cpu: usize,
        over: bool,
        q: &Value,
    ) -> Result<Value, String> {
        if q.get("vector_length").is_some_and(|v| !v.is_null()) {
            return Err("vector_length is ARM64-only; AMD64 stepping does not accept it".into());
        }
        // pause() is an all-CPU acknowledgement. Drain events produced before
        // that acknowledgement, so an older exit cannot complete a new step.
        self.vm.pause().map_err(string)?;
        let r = self.registers(cpu, false)?;
        // The current register interface does not expose CS.L; Windows native
        // 64-bit selectors are 0x10/0x33. Never decode WOW64/firmware as AMD64.
        if r.sregs.efer & (1 << 10) == 0 || !matches!(r.sregs.cs.selector, 0x10 | 0x33) {
            return Err(
                "step_over/out requires a native Windows 64-bit CPU context (not WOW64/firmware)"
                    .into(),
            );
        }
        let space = AddressSpace {
            memory: &self.vm,
            registers: &r,
        };
        let (address, sp) = if over {
            let Some(len) = call_len(&code(&space, r.regs.rip)?)? else {
                return self.command(&json!({"op":"step","cpu":cpu}));
            };
            (add(r.regs.rip, len as u64)?, r.regs.rsp)
        } else {
            caller(&space, &r)?
        };
        if address == 0
            || !canonical(address)
            || !canonical(sp)
            || sp & 7 != 0
            || (address == r.regs.rip && sp == r.regs.rsp)
        {
            return Err("invalid caller/return location".into());
        }
        if !over && (sp <= r.regs.rsp || sp - r.regs.rsp > 16 * 1024 * 1024) {
            return Err("caller stack must advance within the bounded 16 MiB frame".into());
        }
        if !space.translate(address).map_err(string)?.executable {
            return Err("return address is not executable".into());
        }
        // debug() merges the temporary address with tracing/user slots and
        // validates the shared four-slot budget before any CPU resumes.
        self.run_to = Some(RunTo {
            address,
            sp,
            cpu: cpu as u32,
            cr3: r.sregs.cr3,
            reason: if over { "StepOver" } else { "StepOut" },
        });
        match self.pending_step_events() {
            Ok(false) => {}
            Ok(true) => {
                self.cancel_step()?;
                return Err(
                    "pending VM stops were reported; inspect them and retry stepping".into(),
                );
            }
            Err(e) => {
                let _ = self.cancel_step();
                return Err(e);
            }
        }
        if let Err(e) = self.resume_return() {
            let _ = self.cancel_step();
            return Err(e);
        }
        Ok(
            json!({"state":"Running","until":address,"note":"all CPUs/devices running; poll events.poll; pause cancels; no timeout"}),
        )
    }
    pub(super) fn pending_step_events(&mut self) -> Result<bool, String> {
        let mut pending = false;
        while let Some(event) = self.vm.next_event(Duration::ZERO) {
            pending = true;
            match event {
                VmEvent::Debug(stop) => {
                    self.stopped.insert(stop.cpu as usize, stop.reason);
                    super::super::output(
                        json!({"event":"stopped","cpu":stop.cpu,"pc":stop.pc,"reason":format!("{:?}",stop.reason),"syndrome":stop.syndrome}),
                    )?;
                }
                VmEvent::Shutdown | VmEvent::Reset => {
                    self.vm.shutdown();
                    return Err("VM stopped while preparing return step".into());
                }
                VmEvent::Crashed(error) => return Err(format!("guest: {error}")),
                other => {
                    super::super::output(json!({"event":"vm","message":format!("{other:?}")}))?
                }
            }
        }
        Ok(pending)
    }
    /// None means an unrelated thread/recursive invocation hit our private
    /// return trap and has been continued with RF. Persistent traps still stop.
    pub(super) fn step_stopped(&mut self, stop: &DebugStop) -> Result<Option<String>, String> {
        self.vm.pause().map_err(string)?;
        self.stopped.insert(stop.cpu as usize, stop.reason);
        let mut reason = format!("{:?}", stop.reason);
        if let Some(run) = &self.run_to {
            // Do not hide a simultaneous DR6 cause behind our synthetic reason.
            if stop.reason == DebugReason::Breakpoint
                && stop.pc == run.address
                && (stop.syndrome & 15).count_ones() == 1
                && stop.syndrome & (1 << 14) == 0
            {
                let r = self.registers(stop.cpu as usize, false)?;
                if run.matches(&r) {
                    reason = run.reason.into();
                } else if !self.breakpoints.contains(&stop.pc) {
                    if self.pending_step_events()? {
                        self.cancel_step()?;
                        return Ok(Some(reason));
                    }
                    self.resume_return()?;
                    return Ok(None);
                }
            }
        }
        self.cancel_step()?;
        Ok(Some(reason))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vroom_kernel::x86_64::{General, Segment, System, Table};
    struct Memory(Vec<u8>);
    impl VirtualMemory for Memory {
        fn read(&self, at: u64, out: &mut [u8]) -> vroom_kernel::Result<()> {
            let data = self
                .0
                .get(at as usize..at as usize + out.len())
                .ok_or_else(|| vroom_kernel::Error::Invalid("outside test memory".into()))?;
            out.copy_from_slice(data);
            Ok(())
        }
    }
    fn put(m: &mut Memory, at: usize, b: &[u8]) {
        m.0[at..at + b.len()].copy_from_slice(b);
    }
    fn u32_at(m: &mut Memory, at: usize, n: u32) {
        put(m, at, &n.to_le_bytes());
    }
    fn u64_at(m: &mut Memory, at: usize, n: u64) {
        put(m, at, &n.to_le_bytes());
    }
    fn fixture() -> (Memory, Registers) {
        let mut m = Memory(vec![0; 0x10000]);
        put(&mut m, 0, b"MZ");
        u32_at(&mut m, 60, 0x80);
        put(&mut m, 0x80, b"PE\0\0");
        put(&mut m, 0x84, &0x8664u16.to_le_bytes());
        put(&mut m, 0x86, &1u16.to_le_bytes());
        put(&mut m, 0x94, &240u16.to_le_bytes());
        put(&mut m, 0x98, &0x20bu16.to_le_bytes());
        u32_at(&mut m, 0x80 + 80, 0x10000);
        u32_at(&mut m, 0x80 + 132, 16);
        u32_at(&mut m, 0x80 + 136, 0x5000);
        u32_at(&mut m, 0x80 + 140, 40);
        u32_at(&mut m, 0x5000 + 12, 0x5100);
        u32_at(&mut m, 0x5000 + 20, 1);
        u32_at(&mut m, 0x5000 + 24, 1);
        u32_at(&mut m, 0x5000 + 28, 0x5200);
        u32_at(&mut m, 0x5000 + 32, 0x5210);
        u32_at(&mut m, 0x5000 + 36, 0x5220);
        put(&mut m, 0x5100, b"step-test.dll\0");
        u32_at(&mut m, 0x5200, 0x1000);
        u32_at(&mut m, 0x5210, 0x5230);
        put(&mut m, 0x5230, b"Target\0");
        u32_at(&mut m, 0x80 + 160, 0x2000);
        u32_at(&mut m, 0x80 + 164, 12);
        u32_at(&mut m, 0x2000, 0x1000);
        u32_at(&mut m, 0x2004, 0x1100);
        u32_at(&mut m, 0x2008, 0x3000);
        put(&mut m, 0x1020, &[0x90]);
        let r = Registers {
            cpu: 1,
            regs: General {
                rip: 0x1020,
                rsp: 0x9000,
                ..Default::default()
            },
            msrs: vec![],
            sregs: System {
                cr0: 1 << 31,
                cr2: 0,
                cr3: 0x5000,
                cr4: 1 << 5,
                efer: 1 << 10,
                cs: Segment {
                    base: 0,
                    selector: 0x10,
                },
                gs: Segment {
                    base: 0,
                    selector: 0,
                },
                idt: Table { base: 0, limit: 0 },
            },
        };
        (m, r)
    }
    #[test]
    fn near_calls_include_prefixes_modrm_sib_and_displacements() {
        for (b, n) in [
            (&[0xe8, 0, 0, 0, 0][..], 5),
            (&[0x66, 0xe8, 0, 0, 0, 0][..], 6),
            (&[0x41, 0xff, 0xd4][..], 3),
            (&[0xff, 0x15, 0, 0, 0, 0][..], 6),
            (&[0xff, 0x54, 0x24, 0x20][..], 4),
            (&[0xff, 0x94, 0x24, 0, 0, 0, 0][..], 7),
            (&[0x67, 0xff, 0x14, 0x25, 0, 0, 0, 0][..], 8),
        ] {
            assert_eq!(call_len(b), Ok(Some(n)), "{b:x?}");
        }
        for b in [
            &[0x90][..],
            &[0xc3][..],
            &[0xeb, 0][..],
            &[0xff, 0xe0][..],
            &[0x0f, 0x05][..],
        ] {
            assert_eq!(call_len(b), Ok(None));
        }
        for b in [
            &[0xe8][..],
            &[0xff, 0x94, 0x24][..],
            &[0xf0, 0xe8, 0, 0, 0, 0][..],
            &[0x66; 15][..],
        ] {
            assert!(call_len(b).is_err());
        }
    }
    #[test]
    fn image_lookup_delegates_caller_recovery_to_shared_unwinder() {
        let (mut m, mut r) = fixture();
        let image = Image::read_amd64(&m, 0).unwrap();
        assert_eq!(image.name, "step-test.dll");
        assert_eq!(image.exports.get("Target"), Some(&0x1000));
        // push rbp; sub rsp,32. Reverse order in UNWIND_CODE.
        put(&mut m, 0x3000, &[1, 4, 2, 0, 4, 0x32, 1, 0x50]);
        u64_at(&mut m, 0x9028, 0x4567);
        assert_eq!(caller(&m, &r).unwrap(), (0x4567, 0x9030));
        r.regs.rip = 0x1002;
        u64_at(&mut m, 0x9008, 0x5678);
        assert_eq!(caller(&m, &r).unwrap(), (0x5678, 0x9010));
    }
    #[test]
    fn caller_lookup_rejects_invalid_images_and_unsupported_shared_unwind() {
        let (mut m, r) = fixture();
        put(&mut m, 0x3000, &[3, 4, 0, 0]); // Unsupported version, not v2.
        assert!(caller(&m, &r).unwrap_err().contains("version"));
        put(&mut m, 0x3000, &[1, 4, 1, 0, 4, 0x32]);
        u64_at(&mut m, 0x9020, 0x4567);
        assert_eq!(caller(&m, &r).unwrap(), (0x4567, 0x9028));
        // The lookup and shared helper both require production PE validation.
        put(&mut m, 0x5220, &1u16.to_le_bytes()); // Invalid export ordinal.
        assert!(caller(&m, &r).is_err());
        assert!(super::super::analysis::caller_in_image(&m, &r, 0).is_err());
        put(&mut m, 0x5220, &0u16.to_le_bytes());
        put(&mut m, 0x86, &0u16.to_le_bytes()); // Missing sections.
        assert!(caller(&m, &r).is_err());
        put(&mut m, 0x86, &1u16.to_le_bytes());
        put(&mut m, 0x84, &0xaa64u16.to_le_bytes()); // Wrong architecture.
        assert!(caller(&m, &r).is_err());
    }
    #[test]
    fn temporary_return_requires_cpu_stack_and_address_space() {
        let (_, mut r) = fixture();
        let target = RunTo {
            address: r.regs.rip,
            sp: r.regs.rsp,
            cpu: r.cpu,
            cr3: r.sregs.cr3,
            reason: "StepOut",
        };
        assert!(target.matches(&r));
        r.sregs.cr3 |= 5;
        assert!(target.matches(&r));
        r.regs.rsp -= 8;
        assert!(!target.matches(&r));
        r.regs.rsp += 8;
        r.cpu += 1;
        assert!(!target.matches(&r));
        r.cpu -= 1;
        r.sregs.cr3 += 0x1000;
        assert!(!target.matches(&r));
    }
    #[test]
    fn call_fetch_does_not_require_following_page() {
        let m = Memory(vec![0x90, 0xe8, 0, 0, 0, 0]);
        assert_eq!(call_len(&code(&m, 1).unwrap()), Ok(Some(5)));
    }
}

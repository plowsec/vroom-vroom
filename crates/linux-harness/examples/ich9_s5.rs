//! Native normal S5: exact OVMF PM1 word RMW sequence, typed owner stop.
//! One CPU, no OS/firmware/media; 15 seconds aggregate, outer bound20 seconds.

#[cfg(any(test, all(target_os = "linux", target_arch = "x86_64")))]
#[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64")), allow(dead_code))]
mod fixture {
    use std::time::Instant;
    use vroom_devices::{Bus, BusRange};
    use vroom_devices::pci::{LpcIoContext, LpcPioLayout, LpcPioWriteOutcome, MchPci};
    use vroom_hv::{Vcpu, VcpuExit};
    use vroom_memory::GuestMemory;
    const RAM: u64 = 0x10000;
    const ROM: u64 = 0xffff0000;
    const SIZE: usize = 0x10000;
    const ECAM: u64 = 0xe0000000;
    const PM1: u16 = 0x604;
    const ECHO: u16 = 0x710;
    const FORBIDDEN: u16 = 0x714;
    type Result<T> = std::result::Result<T, String>;

    #[derive(Debug)]
    enum Step { Config(VcpuExit), Read, Echo, Power(u16) }
    #[derive(Debug, PartialEq, Eq)]
    enum Stop { AcpiS5 }

    fn output(port: u16, len: usize, value: u32) -> VcpuExit {
        let mut data = [0; 4]; data[..len].copy_from_slice(&value.to_le_bytes()[..len]);
        VcpuExit::PioOut { port, len, data }
    }
    fn expected(step: &Step) -> VcpuExit {
        match step {
            Step::Config(exit) => exit.clone(), Step::Read => VcpuExit::PioIn { port: PM1, len: 2 },
            Step::Echo => output(ECHO, 2, 0), Step::Power(value) => output(PM1, 2, u32::from(*value)),
        }
    }
    fn mov_dx(code: &mut Vec<u8>, port: u16) { code.extend([0x66, 0xba]); code.extend(port.to_le_bytes()); }
    fn config(code: &mut Vec<u8>, steps: &mut Vec<Step>, slot: u8, reg: u8, len: usize, value: u32) {
        mov_dx(code, 0xcf8); code.push(0xb8);
        let selector = 0x80000000 | (u32::from(slot) << 11) | u32::from(reg & !3);
        code.extend(selector.to_le_bytes()); code.push(0xef);
        steps.push(Step::Config(output(0xcf8, 4, selector)));
        mov_dx(code, 0xcfc + u16::from(reg & 3));
        code.push(if len == 1 { 0xb0 } else { 0xb8 }); code.extend(&value.to_le_bytes()[..len]);
        code.push(if len == 1 { 0xee } else { 0xef });
        steps.push(Step::Config(output(0xcfc + u16::from(reg & 3), len, value)));
    }
    fn rmw(code: &mut Vec<u8>, steps: &mut Vec<Step>, shutdown: bool) {
        mov_dx(code, PM1); code.extend([0x66, 0xed]); steps.push(Step::Read); // IN AX,DX
        mov_dx(code, ECHO); code.extend([0x66, 0xef]); steps.push(Step::Echo); // preserves AX
        // IoBitFieldWrite16(...,10,13,0), then IoOr16(...,BIT13).
        code.extend(if shutdown { [0x66, 0x0d, 0, 0x20] } else { [0x66, 0x25, 0xff, 0xc3] });
        mov_dx(code, PM1); code.extend([0x66, 0xef]);
        steps.push(Step::Power(if shutdown { 0x2000 } else { 0 }));
    }
    fn program() -> (Vec<u8>, Vec<Step>) {
        let mut code = vec![0xfa, 0xb8, 0, 0x10, 0x8e, 0xd8, 0x0f, 0x01, 0x16, 0, 0x10,
            0x0f, 0x20, 0xc0, 0x66, 0x83, 0xc8, 1, 0x0f, 0x22, 0xc0, 0x66, 0xea];
        let target = RAM as u32 + code.len() as u32 + 6;
        code.extend(target.to_le_bytes()); code.extend(8u16.to_le_bytes());
        code.extend([0x66, 0xb8, 0x10, 0, 0x8e, 0xd8, 0x8e, 0xc0, 0x8e, 0xd0, 0xfc]);
        let mut steps = vec![];
        config(&mut code, &mut steps, 31, 0x40, 4, 0x600);
        config(&mut code, &mut steps, 31, 0x44, 1, 0x80); // PM decode works with IOSE0
        rmw(&mut code, &mut steps, false); rmw(&mut code, &mut steps, true);
        // No expected Step for this marker: a correct owner never RUNs it.
        mov_dx(&mut code, FORBIDDEN); code.extend([0xb0, 0xee, 0xee, 0xeb, 0xfe]);
        (code, steps)
    }
    fn ram_image() -> Vec<u8> {
        let mut image = vec![0; SIZE]; let code = program().0; image[..code.len()].copy_from_slice(&code);
        image[0x1000..0x1002].copy_from_slice(&23u16.to_le_bytes());
        image[0x1002..0x1006].copy_from_slice(&(RAM as u32 + 0x1010).to_le_bytes());
        image[0x1010..0x1028].copy_from_slice(&[0u64, 0x00cf9a000000ffff, 0x00cf92000000ffff]
            .into_iter().flat_map(u64::to_le_bytes).collect::<Vec<_>>()); image
    }
    fn reset_rom() -> Vec<u8> {
        let mut rom = vec![0xf4; SIZE]; rom[0xfff0..0xfff5].copy_from_slice(&[0xea, 0, 0, 0, 0x10]); rom
    }
    fn router(ram: &GuestMemory) -> Result<MchPci> {
        MchPci::with_q35_lpc(vec![], Bus::default(), ram, BusRange { base: ECAM, len: 256 << 20 }, vec![], vec![
            BusRange { base: 0x30000, len: 0x20000 }, BusRange { base: 0x1f800000, len: 0x800000 },
            BusRange { base: ROM, len: SIZE as u64 }, BusRange { base: 0xffbfc000, len: 0x4000 },
            BusRange { base: 0xfec00000, len: 0x1000 }, BusRange { base: 0xfee00000, len: 0x1000 },
        ], LpcPioLayout::new(vec![BusRange { base: 0x600, len: 0x80 }],
            vec![BusRange { base: ECHO as u64, len: 8 }]).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
    }

    // This private seam lets pure tests prove the *real consumer's* RUN count
    // and failure classification without creating a VM or claiming runtime.
    trait Cpu {
        fn run(&mut self) -> Result<VcpuExit>;
        fn complete(&mut self, data: &[u8]) -> Result<()>;
    }
    impl Cpu for Box<dyn Vcpu> {
        fn run(&mut self) -> Result<VcpuExit> { (**self).run().map_err(|e| e.to_string()) }
        fn complete(&mut self, data: &[u8]) -> Result<()> { (**self).complete_pio_in(data).map_err(|e| e.to_string()) }
    }
    fn consume(cpu: &mut impl Cpu, router: &mut MchPci, steps: &[Step]) -> Result<Stop> {
        let epoch = Instant::now();
        let mut pending_echo = false;
        let mut completed_reads = 0;
        let mut echoes = 0;
        let mut clear_writes = 0;
        for (index, step) in steps.iter().enumerate() {
            let got = cpu.run()?;
            if got != expected(step) { return Err(format!("unexpected exit instead of S5 sequence: {got:?}")); }
            let ns = u64::try_from(epoch.elapsed().as_nanos()).map_err(|_| "clock overflow")?;
            match step {
                Step::Config(_) => match got {
                    VcpuExit::PioOut { port, len, data } => router.pio_write(port, &data[..len]).map_err(|e| e.to_string())?,
                    _ => return Err("invalid config step".into()),
                }
                Step::Read => {
                    if pending_echo { return Err("missing read echo".into()); }
                    let mut data = [0xa5; 2];
                    if !router.lpc_pio_read(PM1, &mut data, ns).map_err(|e| e.to_string())? || data != [0; 2] {
                        return Err("wrong PM1 completion".into());
                    }
                    cpu.complete(&data)?; pending_echo = true; completed_reads += 1;
                }
                Step::Echo => {
                    if !pending_echo { return Err("unsolicited read echo".into()); }
                    pending_echo = false; echoes += 1;
                }
                Step::Power(value) => {
                    if pending_echo { return Err("power write before echoed completion".into()); }
                    // Derive Normal only from THIS actual, matched PioOut.
                    let VcpuExit::PioOut { port, len, data } = got else { return Err("not normal power context".into()) };
                    let outcome = router.lpc_pio_write_with_power(port, &data[..len], ns, LpcIoContext::Normal).map_err(|e| e.to_string())?;
                    match outcome {
                        LpcPioWriteOutcome::Handled if *value == 0 => clear_writes += 1,
                        LpcPioWriteOutcome::AcpiS5 if *value == 0x2000 && index + 1 == steps.len()
                            && clear_writes == 1 && completed_reads == 2 && echoes == 2 => {
                                // Terminal return: no loop continuation or more RUN.
                                return Ok(Stop::AcpiS5);
                            }
                        _ => return Err("wrong typed power outcome".into()),
                    }
                }
            }
        }
        Err("sequence ended without typed S5".into())
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    pub fn run() -> Result<()> {
        use std::sync::Arc;
        use vroom_hv::{BootRegs, IrqChipConfig, VmParams};
        use vroom_memory::{GuestAddress, MmapRegion};
        let hv = vroom_hv::default_hypervisor().map_err(|e| e.to_string())?;
        if hv.name() != "kvm" || hv.arch() != vroom_hv::Arch::X86_64 { return Err("requires native x86 KVM".into()); }
        let vm = hv.create_vm(&VmParams { vcpus: 1, irqchip: IrqChipConfig::Ioapic,
            nested: false, small_pages: false, helper_page: 0 }).map_err(|e| e.to_string())?;
        let caps = vm.configure_x86_q35_smram().map_err(|e| e.to_string())?;
        if !caps.vm_smm || caps.system_address_spaces < 2 { return Err("fixed Q35 backing requires SMM capabilities".into()); }
        let ram = GuestMemory::new(&[(GuestAddress(RAM), SIZE as u64)]).map_err(|e| e.to_string())?;
        ram.write(GuestAddress(RAM), &ram_image()).map_err(|e| e.to_string())?;
        let mut router = router(&ram)?;
        vm.map_memory(&ram).map_err(|e| e.to_string())?;
        let rom = Arc::new(MmapRegion::new(SIZE).map_err(|e| e.to_string())?);
        rom.load(&reset_rom()).map_err(|e| e.to_string())?; vm.map_readonly(ROM, rom).map_err(|e| e.to_string())?;
        let mut cpu = vm.create_vcpu(0).map_err(|e| e.to_string())?;
        cpu.set_boot_regs(&BootRegs::X86ResetVector).map_err(|e| e.to_string())?;
        vm.finalize().map_err(|e| e.to_string())?;
        let result = consume(&mut cpu, &mut router, &program().1);
        // S5 is a typed owner-consumed shutdown, not KVM's triple-fault exit.
        // Failure also disposes this owned VM without another RUN.
        drop(cpu); drop(router); drop(ram); drop(vm); drop(hv);
        match result { Ok(Stop::AcpiS5) => Ok(()), Err(error) => Err(error) }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use std::collections::VecDeque;
        struct Fake { exits: VecDeque<Result<VcpuExit>>, runs: usize, completions: usize, fail_complete: bool }
        impl Cpu for Fake {
            fn run(&mut self) -> Result<VcpuExit> { self.runs += 1; self.exits.pop_front().expect("unexpected extra RUN") }
            fn complete(&mut self, data: &[u8]) -> Result<()> {
                assert_eq!(data, [0, 0]); self.completions += 1;
                if self.fail_complete { Err("completion failure".into()) } else { Ok(()) }
            }
        }
        fn fake(steps: &[Step]) -> Fake {
            let mut exits = steps.iter().map(|s| Ok(expected(s))).collect::<VecDeque<_>>();
            exits.push_back(Ok(output(FORBIDDEN, 1, 0xee)));
            Fake { exits, runs: 0, completions: 0, fail_complete: false }
        }
        #[test]
        fn exact_word_rmw_codecs_keep_ax_live_between_read_and_write() {
            for shutdown in [false, true] {
                let mut code = vec![]; let mut steps = vec![]; rmw(&mut code, &mut steps, shutdown);
                assert_eq!(code, [vec![0x66, 0xba, 4, 6, 0x66, 0xed, 0x66, 0xba, 0x10, 7, 0x66, 0xef],
                    if shutdown { vec![0x66, 0x0d, 0, 0x20] } else { vec![0x66, 0x25, 0xff, 0xc3] },
                    vec![0x66, 0xba, 4, 6, 0x66, 0xef]].concat());
                assert!(matches!(steps.as_slice(), [Step::Read, Step::Echo, Step::Power(_)]));
            }
            let (code, steps) = program(); assert!(code.len() < 0x1000); assert_eq!(steps.len(), 10);
            assert_eq!(&code[code.len()-9..], &[0x66, 0xba, 0x14, 7, 0xb0, 0xee, 0xee, 0xeb, 0xfe]);
        }
        #[test]
        fn actual_consumer_stops_before_successor_and_requires_both_completed_reads() {
            let steps = program().1; let mut cpu = fake(&steps); let mut board = router(&GuestMemory::default()).unwrap();
            assert_eq!(consume(&mut cpu, &mut board, &steps), Ok(Stop::AcpiS5));
            assert_eq!(cpu.runs, steps.len()); assert_eq!(cpu.completions, 2); assert_eq!(cpu.exits.len(), 1);
            assert_eq!(cpu.exits.front(), Some(&Ok(output(FORBIDDEN, 1, 0xee))));
            let mut cpu = fake(&steps); cpu.fail_complete = true;
            assert!(consume(&mut cpu, &mut router(&GuestMemory::default()).unwrap(), &steps).is_err());
            assert_eq!(cpu.completions, 1); assert_eq!(cpu.runs, 5);
        }
        #[test]
        fn no_fault_halt_timeout_or_other_shutdown_can_satisfy_terminal_acceptance() {
            let steps = program().1;
            for replacement in [Err("triple fault".into()), Err("timeout".into()), Ok(VcpuExit::Halted),
                Ok(VcpuExit::SystemOff), Ok(VcpuExit::SystemReset), Ok(output(FORBIDDEN, 1, 0xee))] {
                let mut cpu = fake(&steps); cpu.exits[steps.len()-1] = replacement;
                assert!(consume(&mut cpu, &mut router(&GuestMemory::default()).unwrap(), &steps).is_err());
                assert_eq!(cpu.runs, steps.len()); assert_eq!(cpu.exits.len(), 1);
            }
            let mut truncated = program().1; truncated.pop(); let mut cpu = fake(&truncated);
            assert!(consume(&mut cpu, &mut router(&GuestMemory::default()).unwrap(), &truncated).is_err());
            assert_eq!(cpu.exits.len(), 1);
        }
        #[test]
        fn generated_reset_and_ram_stay_within_owned_bounds() {
            let image = ram_image(); let (code, _) = program(); assert_eq!(image.len(), SIZE);
            assert_eq!(u32::from_le_bytes(code[23..27].try_into().unwrap()), RAM as u32 + 29);
            assert_eq!(&image[0x1000..0x1006], &[23, 0, 0x10, 0x10, 1, 0]);
            assert_eq!(&reset_rom()[0xfff0..0xfff5], &[0xea, 0, 0, 0, 0x10]);
            assert!(0x20000 + 0x800000 + 2 * SIZE < 9 * 1024 * 1024);
        }
    }
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn main() {
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::Duration;
    if std::env::args_os().len() != 1 { eprintln!("ich9_s5 accepts no arguments"); std::process::exit(2); }
    let (done, deadline) = mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if matches!(deadline.recv_timeout(Duration::from_secs(15)), Err(RecvTimeoutError::Timeout)) {
            eprintln!("native ICH9 S5 timed out after 15 seconds"); std::process::exit(124);
        }
    });
    let result = fixture::run(); let _ = done.send(());
    if watchdog.join().is_err() { eprintln!("watchdog failed"); std::process::exit(2); }
    match result {
        Ok(()) => println!("{}", serde_json::json!({"schema":1,"test":"native_ich9_s5","status":"pass",
            "vm_count":1,"max_live_vcpus":1,"read_modify_write_sequence":true,"read_completions_echoed":true,
            "typed_shutdown_consumed":true,"shutdown_reason":"acpi_s5","no_run_after_shutdown":true,
            "os_or_firmware_boot":false,"sci":false,"vmm_profile":false})),
        Err(error) => { eprintln!("native ICH9 S5 failed: {error}"); std::process::exit(2); }
    }
}
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() {
    eprintln!("native ICH9 S5 requires Linux x86_64 KVM; no guest was run"); std::process::exit(2);
}

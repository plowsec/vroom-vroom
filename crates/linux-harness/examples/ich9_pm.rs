//! Native LPC PM decoder/timer prerequisite. One CPU, no OS or firmware.
//! Requires a separate outer 20-second watchdog; internal bound is 15 seconds.

#[cfg(any(test, all(target_os = "linux", target_arch = "x86_64")))]
#[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64")), allow(dead_code))]
mod fixture {
    use vroom_hv::VcpuExit;

    const RAM: u64 = 0x10000;
    const ROM: u64 = 0xffff0000;
    const SIZE: usize = 0x10000;
    const ECAM: u64 = 0xe0000000;
    const LPC: u64 = ECAM + (31 << 15);
    const ECHO: u16 = 0x710;
    const DONE: u16 = 0x714;
    type Result<T> = std::result::Result<T, String>;

    #[derive(Debug, PartialEq)]
    enum Read { Config(u32), Timer, Absent }
    #[derive(Debug)]
    enum Step { Write(VcpuExit), Read(VcpuExit, Read), Echo, Done }

    fn output(port: u16, len: usize, value: u32) -> VcpuExit {
        let mut data = [0; 4]; data[..len].copy_from_slice(&value.to_le_bytes()[..len]);
        VcpuExit::PioOut { port, len, data }
    }
    fn mov_dx(code: &mut Vec<u8>, port: u16) { code.extend([0x66, 0xba]); code.extend(port.to_le_bytes()); }
    fn out(code: &mut Vec<u8>, steps: &mut Vec<Step>, port: u16, len: usize, value: u32) {
        mov_dx(code, port);
        code.push(if len == 1 { 0xb0 } else { 0xb8 }); code.extend(&value.to_le_bytes()[..len]);
        code.push(if len == 1 { 0xee } else { 0xef });
        steps.push(Step::Write(output(port, len, value)));
    }
    fn select(code: &mut Vec<u8>, steps: &mut Vec<Step>, slot: u8, reg: u8) {
        out(code, steps, 0xcf8, 4, 0x80000000 | u32::from(slot) << 11 | u32::from(reg & !3));
    }
    fn config_write(code: &mut Vec<u8>, steps: &mut Vec<Step>, ecam: bool, slot: u8, reg: u8, len: usize, value: u32) {
        if ecam {
            let address = ECAM + (u64::from(slot) << 15) + u64::from(reg);
            code.extend([if len == 1 { 0xc6 } else { 0xc7 }, 0x05]); code.extend((address as u32).to_le_bytes());
            code.extend(&value.to_le_bytes()[..len]);
            let mut data = [0; 8]; data[..len].copy_from_slice(&value.to_le_bytes()[..len]);
            steps.push(Step::Write(VcpuExit::MmioWrite { addr: address, len, data }));
        } else {
            select(code, steps, slot, reg);
            out(code, steps, 0xcfc + u16::from(reg & 3), len, value);
        }
    }
    fn read(code: &mut Vec<u8>, steps: &mut Vec<Step>, exit: VcpuExit, expected: Read) {
        code.extend([0x31, 0xc0]); // zero EAX for byte reads, including the echo
        match exit {
            VcpuExit::PioIn { port, len } => { mov_dx(code, port); code.push(if len == 1 { 0xec } else { 0xed }); }
            VcpuExit::MmioRead { addr, len } => {
                code.push(if len == 1 { 0xa0 } else { 0xa1 }); code.extend((addr as u32).to_le_bytes());
            }
            _ => panic!("fixture read codec"),
        }
        steps.push(Step::Read(exit, expected));
        mov_dx(code, ECHO); code.push(0xef); steps.push(Step::Echo);
    }

    fn program() -> (Vec<u8>, Vec<Step>) {
        // Real reset entry at FFFFFFF0 jumps to1000:0000. Enter flat protected
        // mode with a GDT in common RAM; straight-line code needs no stack.
        let mut code = vec![0xfa, 0xb8, 0, 0x10, 0x8e, 0xd8, 0x0f, 0x01, 0x16, 0, 0x10,
            0x0f, 0x20, 0xc0, 0x66, 0x83, 0xc8, 1, 0x0f, 0x22, 0xc0, 0x66, 0xea];
        let target = RAM as u32 + code.len() as u32 + 6;
        code.extend(target.to_le_bytes()); code.extend(8u16.to_le_bytes());
        code.extend([0x66, 0xb8, 0x10, 0, 0x8e, 0xd8, 0x8e, 0xc0, 0x8e, 0xd0, 0xfc]);
        let mut steps = Vec::new();
        select(&mut code, &mut steps, 31, 0);
        read(&mut code, &mut steps, VcpuExit::PioIn { port: 0xcfc, len: 4 }, Read::Config(0x29188086));
        // Real MCH ECAM mapping; LPC config remains one source of truth.
        config_write(&mut code, &mut steps, false, 0, 0x60, 4, 0);
        config_write(&mut code, &mut steps, false, 0, 0x64, 4, 0);
        config_write(&mut code, &mut steps, false, 0, 0x60, 4, ECAM as u32 | 1);
        config_write(&mut code, &mut steps, false, 31, 0x40, 4, 0x600);
        config_write(&mut code, &mut steps, true, 31, 0x44, 1, 0x80);
        read(&mut code, &mut steps, VcpuExit::MmioRead { addr: LPC + 4, len: 1 }, Read::Config(0));
        read(&mut code, &mut steps, VcpuExit::MmioRead { addr: LPC + 0x40, len: 4 }, Read::Config(0x601));
        for _ in 0..2 { read(&mut code, &mut steps, VcpuExit::PioIn { port: 0x608, len: 4 }, Read::Timer); }
        config_write(&mut code, &mut steps, false, 31, 0x44, 1, 0);
        read(&mut code, &mut steps, VcpuExit::PioIn { port: 0x608, len: 4 }, Read::Absent);
        config_write(&mut code, &mut steps, false, 31, 0x40, 4, 0x800);
        config_write(&mut code, &mut steps, true, 31, 0x44, 1, 0x80);
        read(&mut code, &mut steps, VcpuExit::PioIn { port: 0x608, len: 4 }, Read::Absent);
        read(&mut code, &mut steps, VcpuExit::PioIn { port: 0x808, len: 4 }, Read::Timer);
        // Read-only writes cannot reset or supply a fake timer value.
        out(&mut code, &mut steps, 0x808, 4, u32::MAX);
        read(&mut code, &mut steps, VcpuExit::PioIn { port: 0x808, len: 4 }, Read::Timer);
        mov_dx(&mut code, DONE); code.extend([0xb0, 0x51, 0xee]); steps.push(Step::Done);
        code.extend([0xeb, 0xfe]); // controller must never RUN after the final marker
        (code, steps)
    }
    fn ram_image() -> Vec<u8> {
        let (code, _) = program(); let mut image = vec![0; SIZE];
        assert!(code.len() < 0x1000); image[..code.len()].copy_from_slice(&code);
        image[0x1000..0x1002].copy_from_slice(&23u16.to_le_bytes());
        image[0x1002..0x1006].copy_from_slice(&(RAM as u32 + 0x1010).to_le_bytes());
        image[0x1010..0x1028].copy_from_slice(&[0u64, 0x00cf9a000000ffff, 0x00cf92000000ffff]
            .into_iter().flat_map(u64::to_le_bytes).collect::<Vec<_>>()); image
    }
    fn reset_rom() -> Vec<u8> {
        let mut rom = vec![0xf4; SIZE]; rom[0xfff0..0xfff5].copy_from_slice(&[0xea, 0, 0, 0, 0x10]); rom
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    pub fn run() -> Result<()> {
        use std::sync::Arc;
        use std::time::{Duration, Instant};
        use vroom_devices::{Bus, BusRange};
        use vroom_devices::pci::{LpcPioLayout, MchPci};
        use vroom_hv::{BootRegs, IrqChipConfig, VmParams};
        use vroom_memory::{GuestAddress, GuestMemory, MmapRegion};
        let hv = vroom_hv::default_hypervisor().map_err(|e| e.to_string())?;
        if hv.name() != "kvm" || hv.arch() != vroom_hv::Arch::X86_64 { return Err("requires native x86 KVM".into()); }
        let vm = hv.create_vm(&VmParams { vcpus: 1, irqchip: IrqChipConfig::Ioapic,
            nested: false, small_pages: false, helper_page: 0 }).map_err(|e| e.to_string())?;
        let caps = vm.configure_x86_q35_smram().map_err(|e| e.to_string())?;
        if !caps.vm_smm || caps.system_address_spaces < 2 { return Err("fixed Q35 backing requires SMM capabilities".into()); }
        let ram = GuestMemory::new(&[(GuestAddress(RAM), SIZE as u64)]).map_err(|e| e.to_string())?;
        ram.write(GuestAddress(RAM), &ram_image()).map_err(|e| e.to_string())?;
        let layout = LpcPioLayout::new(vec![BusRange { base: 0x600, len: 0x80 }, BusRange { base: 0x800, len: 0x80 }],
            vec![BusRange { base: 0x710, len: 8 }]).map_err(|e| e.to_string())?;
        let mut router = MchPci::with_q35_lpc(vec![], Bus::default(), &ram,
            BusRange { base: ECAM, len: 256 << 20 }, vec![], vec![
                BusRange { base: 0x30000, len: 0x20000 }, BusRange { base: 0x1f800000, len: 0x800000 },
                BusRange { base: ROM, len: SIZE as u64 }, BusRange { base: 0xffbfc000, len: 0x4000 },
                BusRange { base: 0xfec00000, len: 0x1000 }, BusRange { base: 0xfee00000, len: 0x1000 },
            ], layout).map_err(|e| e.to_string())?;
        vm.map_memory(&ram).map_err(|e| e.to_string())?;
        let rom = Arc::new(MmapRegion::new(SIZE).map_err(|e| e.to_string())?);
        rom.load(&reset_rom()).map_err(|e| e.to_string())?;
        vm.map_readonly(ROM, rom).map_err(|e| e.to_string())?;
        let mut cpu = vm.create_vcpu(0).map_err(|e| e.to_string())?;
        cpu.set_boot_regs(&BootRegs::X86ResetVector).map_err(|e| e.to_string())?;
        vm.finalize().map_err(|e| e.to_string())?;
        let epoch = Instant::now();
        let mut echo = None;
        let mut samples = Vec::new();
        for step in program().1 {
            let got = cpu.run().map_err(|e| e.to_string())?;
            match step {
                Step::Write(expected) => {
                    if got != expected { return Err(format!("unexpected write exit: {got:?}")); }
                    match got {
                        VcpuExit::PioOut { port, len, data } if (0xcf8..0xd00).contains(&port) =>
                            router.pio_write(port, &data[..len]).map_err(|e| e.to_string())?,
                        VcpuExit::PioOut { port, len, data } => {
                            let ns = u64::try_from(epoch.elapsed().as_nanos()).map_err(|_| "clock overflow")?;
                            if !router.lpc_pio_write(port, &data[..len], ns).map_err(|e| e.to_string())? {
                                return Err("timer write did not decode".into());
                            }
                        }
                        VcpuExit::MmioWrite { addr, len, data } => {
                            if !router.mmio_write(addr, &data[..len]).map_err(|e| e.to_string())? { return Err("config write unmapped".into()); }
                        }
                        _ => return Err("invalid write step".into()),
                    }
                }
                Step::Read(expected, read) => {
                    if got != expected || echo.is_some() { return Err("unexpected read or missing echo".into()); }
                    // Real elapsed time, not a synthetic timer increment. This
                    // bounded delay makes positive progress unambiguous on fast hosts.
                    if read == Read::Timer { std::thread::sleep(Duration::from_millis(2)); }
                    let ns = u64::try_from(epoch.elapsed().as_nanos()).map_err(|_| "clock overflow")?;
                    let mut data = [0u8; 4];
                    let len = match got {
                        VcpuExit::PioIn { port, len } => {
                            if matches!(read, Read::Config(_)) { router.pio_read(port, &mut data[..len]).map_err(|e| e.to_string())?; }
                            else {
                                let mapped = router.lpc_pio_read(port, &mut data[..len], ns).map_err(|e| e.to_string())?;
                                if mapped != (read == Read::Timer) { return Err("wrong LPC decoder outcome".into()); }
                                if !mapped { data[..len].fill(0xff); } // explicit absent-PC-port completion
                            }
                            cpu.complete_pio_in(&data[..len]).map_err(|e| e.to_string())?; len
                        }
                        VcpuExit::MmioRead { addr, len } => {
                            if !router.mmio_read(addr, &mut data[..len]).map_err(|e| e.to_string())? { return Err("config read unmapped".into()); }
                            cpu.complete_mmio_read(&data[..len]).map_err(|e| e.to_string())?; len
                        }
                        _ => return Err("invalid read step".into()),
                    };
                    if !matches!(len, 1 | 4) { return Err("unexpected read width".into()); }
                    let value = u32::from_le_bytes(data);
                    let wanted = match read {
                        Read::Config(value) => value, Read::Absent => u32::MAX,
                        Read::Timer => {
                            samples.push(value);
                            ((u128::from(ns) * 3_579_545 / 1_000_000_000) & 0xff_ffff) as u32
                        }
                    };
                    if value != wanted { return Err("incorrect register/timer completion".into()); }
                    echo = Some(value);
                }
                Step::Echo => {
                    if got != output(ECHO, 4, echo.take().ok_or("unsolicited echo")?) { return Err("guest did not echo completed read".into()); }
                }
                Step::Done => {
                    if got != output(DONE, 1, 0x51) || echo.is_some() { return Err("missing final guest marker".into()); }
                }
            }
        }
        if samples.len() != 4 || samples.iter().any(|&v| v > 0xff_ffff)
            || samples.windows(2).any(|p| p[1].wrapping_sub(p[0]) & 0xff_ffff == 0) {
            return Err("timer progression not observed".into());
        }
        // Dropping the CPU and its owning mappings is a controller stop, not
        // guest S5. There is no RUN after the completed evidence marker.
        drop(cpu); drop(router); drop(ram); drop(vm); drop(hv);
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn sequence_covers_early_enable_disable_rebase_and_every_read_echo() {
            let (code, steps) = program(); assert!(code.len() < 0x1000);
            assert_eq!(steps.iter().filter(|s| matches!(s, Step::Read(_, Read::Timer))).count(), 4);
            assert_eq!(steps.iter().filter(|s| matches!(s, Step::Read(_, Read::Absent))).count(), 2);
            for (i, step) in steps.iter().enumerate() {
                if matches!(step, Step::Read(..)) { assert!(matches!(steps[i + 1], Step::Echo)); }
            }
            assert!(steps.iter().any(|s| matches!(s, Step::Read(VcpuExit::MmioRead { addr, len: 1 }, Read::Config(0)) if *addr == LPC + 4)));
            assert!(matches!(steps.last(), Some(Step::Done)));
            assert_eq!(&code[code.len() - 2..], &[0xeb, 0xfe]);
        }
        #[test]
        fn generated_guest_is_bounded_flat_protected_mode_with_scalar_io() {
            let image = ram_image(); let (code, _) = program();
            assert_eq!(image.len(), SIZE);
            assert_eq!(&image[0x1000..0x1006], &[23, 0, 0x10, 0x10, 1, 0]);
            assert_eq!(&image[0x1018..0x1020], &0x00cf9a000000ffffu64.to_le_bytes());
            assert_eq!(u32::from_le_bytes(code[23..27].try_into().unwrap()), RAM as u32 + 29);
            assert_eq!(&reset_rom()[0xfff0..0xfff5], &[0xea, 0, 0, 0, 0x10]);
            assert!(0x20000 + 0x800000 + 2 * SIZE < 9 * 1024 * 1024);
        }
        #[test]
        fn scalar_read_codecs_echo_zero_extended_byte_or_full_dword() {
            let mut code = vec![]; let mut steps = vec![];
            read(&mut code, &mut steps, VcpuExit::PioIn { port: 0x608, len: 4 }, Read::Timer);
            assert_eq!(code, [0x31, 0xc0, 0x66, 0xba, 8, 6, 0xed, 0x66, 0xba, 0x10, 7, 0xef]);
            code.clear(); steps.clear();
            read(&mut code, &mut steps, VcpuExit::MmioRead { addr: LPC + 4, len: 1 }, Read::Config(0));
            assert_eq!(code, [vec![0x31, 0xc0, 0xa0], ((LPC + 4) as u32).to_le_bytes().to_vec(), vec![0x66, 0xba, 0x10, 7, 0xef]].concat());
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn main() {
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::Duration;
    if std::env::args_os().len() != 1 { eprintln!("ich9_pm accepts no arguments"); std::process::exit(2); }
    let (done, deadline) = mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if matches!(deadline.recv_timeout(Duration::from_secs(15)), Err(RecvTimeoutError::Timeout)) {
            eprintln!("native ICH9 PM timed out after 15 seconds"); std::process::exit(124);
        }
    });
    let result = fixture::run(); let _ = done.send(());
    if watchdog.join().is_err() { eprintln!("watchdog failed"); std::process::exit(2); }
    match result {
        Ok(()) => println!("{}", serde_json::json!({"schema":1,"test":"native_ich9_pm","status":"pass",
            "vm_count":1,"max_live_vcpus":1,"cf8_ecam_binding":true,"timer_before_iose":true,
            "timer_elapsed_ns":true,"guest_read_echo":true,"disabled_and_old_ports_absent":true,
            "rebased_timer_progress":true,"readonly_timer":true,"controller_stop":true,
            "smi_delivery":false,"guest_s5":false,"rtc":false,"os_or_firmware_boot":false})),
        Err(error) => { eprintln!("native ICH9 PM failed: {error}"); std::process::exit(2); }
    }
}
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() { eprintln!("native ICH9 PM requires Linux x86_64 KVM; no guest was run"); std::process::exit(2); }

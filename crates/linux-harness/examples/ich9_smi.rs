//! Native APM -> stopped-vCPU SMI request -> private handler -> RSM acceptance.
//! Two sequential VMs, one CPU at a time; no OS, firmware, disks or network.

#[cfg(any(test, all(target_os = "linux", target_arch = "x86_64")))]
#[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64")), allow(dead_code))]
mod fixture {
    use std::sync::Arc;
    use std::time::Instant;
    use vroom_hv::{BootRegs, IrqChipConfig, Vcpu, VcpuExit, VmParams,
        X86IoExit, X86SmramState, X86TsegState};
    use vroom_memory::{GuestAddress, GuestMemory, MmapRegion};
    use vroom_devices::{Bus, BusRange};
    use vroom_devices::pci::{LpcError, LpcIoContext, LpcPioLayout, LpcSmiRequest,
        MchError, MchPci, MchSmramState, MchTsegState, PciConfigError};

    const ECAM: u64 = 0xe0000000;
    const RAM: u64 = 0x10000;
    const ROM: u64 = 0xffff0000;
    const WINDOW: usize = 0x10000;
    const SMBASE: u32 = 0x30000;
    const TSEG: u32 = 0x1f800000;
    const MAILBOX: u32 = RAM as u32 + 0x2000;
    const HANDLER: u32 = RAM as u32 + 0x4000;
    const OPEN: X86SmramState = X86SmramState { tseg: X86TsegState::Open, default_smbase_locked: false };
    const CLOSED: X86SmramState = X86SmramState { tseg: X86TsegState::Closed, ..OPEN };
    const LOCKED: X86SmramState = X86SmramState { tseg: X86TsegState::Locked, default_smbase_locked: true };
    type Result<T> = std::result::Result<T, String>;

    #[derive(Clone, Copy)]
    enum Case { Positive, InSmm }
    #[derive(Debug)]
    enum Action {
        ConfigWrite(Option<X86SmramState>),
        LpcWrite,
        Command { request: Option<LpcSmiRequest>, reject: bool },
        Read(u32),
        Observe,
    }
    #[derive(Debug)]
    struct Step { exit: VcpuExit, action: Action }

    fn pio_out(port: u16, len: usize, value: u32) -> VcpuExit {
        let mut data = [0; 4]; data[..len].copy_from_slice(&value.to_le_bytes()[..len]);
        VcpuExit::PioOut { port, len, data }
    }
    fn smm_out(port: u16, value: u8) -> VcpuExit {
        VcpuExit::X86SmmIo(X86IoExit::PioOut { port, len: 1, data: [value, 0, 0, 0] })
    }
    fn port_write(code: &mut Vec<u8>, steps: &mut Vec<Step>, port: u16, len: usize, value: u32, action: Action) {
        code.extend([0x66, 0xba]); code.extend(port.to_le_bytes());
        if len == 2 { code.push(0x66); }
        code.push(if len == 1 { 0xb0 } else { 0xb8 }); code.extend(&value.to_le_bytes()[..len]);
        if len == 2 { code.push(0x66); }
        code.push(if len == 1 { 0xee } else { 0xef });
        steps.push(Step { exit: pio_out(port, len, value), action });
    }
    fn config_write(code: &mut Vec<u8>, steps: &mut Vec<Step>, ecam: bool, slot: u8, reg: u8,
        len: usize, value: u32, state: Option<X86SmramState>) {
        if ecam {
            if len == 2 { code.push(0x66); }
            code.extend([if len == 1 { 0xc6 } else { 0xc7 }, 0x05]);
            let addr = ECAM + (u64::from(slot) << 15) + u64::from(reg);
            code.extend((addr as u32).to_le_bytes()); code.extend(&value.to_le_bytes()[..len]);
            let mut data = [0; 8]; data[..len].copy_from_slice(&value.to_le_bytes()[..len]);
            steps.push(Step { exit: VcpuExit::MmioWrite { addr, len, data }, action: Action::ConfigWrite(state) });
        } else {
            port_write(code, steps, 0xcf8, 4, 0x80000000 | (u32::from(slot) << 11) | u32::from(reg & !3), Action::ConfigWrite(None));
            port_write(code, steps, 0xcfc + u16::from(reg & 3), len, value, Action::ConfigWrite(state));
        }
    }
    fn read_echo(code: &mut Vec<u8>, steps: &mut Vec<Step>, exit: VcpuExit, value: u32) {
        code.extend([0x31, 0xc0]); // zero EAX before partial-width read
        match exit {
            VcpuExit::PioIn { port, len } => {
                code.extend([0x66, 0xba]); code.extend(port.to_le_bytes());
                if len == 2 { code.push(0x66); }
                code.push(if len == 1 { 0xec } else { 0xed });
            }
            VcpuExit::MmioRead { addr, len } => {
                if len == 2 { code.push(0x66); }
                code.push(if len == 1 { 0xa0 } else { 0xa1 }); code.extend((addr as u32).to_le_bytes());
            }
            _ => panic!("invalid codec read"),
        }
        steps.push(Step { exit, action: Action::Read(value) });
        code.extend([0x66, 0xba, 0x12, 0x07, 0xef]);
        steps.push(Step { exit: pio_out(0x712, 4, value), action: Action::Observe });
    }
    fn mailbox_echo(code: &mut Vec<u8>, steps: &mut Vec<Step>, value: u8) {
        code.push(0xa0); code.extend(MAILBOX.to_le_bytes());
        code.extend([0x66, 0xba, 0x13, 0x07, 0xee]);
        steps.push(Step { exit: pio_out(0x713, 1, u32::from(value)), action: Action::Observe });
    }
    fn handler(case: Case) -> Vec<u8> {
        // SMM entry is 16-bit, with DS.base0/limit4GiB. Read actual APM bytes
        // through PIO; host completes each read before its explicit echo.
        let mut code = vec![];
        for (input, echo) in [(0xb2u16, 0x720u16), (0xb3, 0x721)] {
            code.push(0xba); code.extend(input.to_le_bytes()); code.push(0xec);
            code.push(0xba); code.extend(echo.to_le_bytes()); code.push(0xee);
        }
        match case {
            Case::Positive => {
                code.extend([0x67, 0xfe, 0x05]); code.extend(MAILBOX.to_le_bytes()); // INC byte [addr32]
                code.extend([0x67, 0xa0]); code.extend(MAILBOX.to_le_bytes());
                code.extend([0xba, 0x22, 0x07, 0xee, 0x0f, 0xaa]); // echo, RSM
            }
            Case::InSmm => code.extend([0xba, 0xb2, 0, 0xb0, 0x44, 0xee, 0xeb, 0xfe]),
        }
        code
    }
    fn command(code: &mut Vec<u8>, steps: &mut Vec<Step>, case: Case, cmd: u8, data: u8, count: Option<u8>) {
        port_write(code, steps, 0xb3, 1, u32::from(data), Action::LpcWrite);
        let request = count.map(|_| LpcSmiRequest { command: cmd, data });
        port_write(code, steps, 0xb2, 1, u32::from(cmd), Action::Command { request, reject: false });
        if let Some(count) = count {
            for (port, value, echo) in [(0xb2, cmd, 0x720), (0xb3, data, 0x721)] {
                steps.push(Step { exit: VcpuExit::X86SmmIo(X86IoExit::PioIn { port, len: 1 }), action: Action::Read(u32::from(value)) });
                steps.push(Step { exit: smm_out(echo, value), action: Action::Observe });
            }
            match case {
                Case::Positive => {
                    steps.push(Step { exit: smm_out(0x722, count), action: Action::Observe });
                    mailbox_echo(code, steps, count); // ordinary exit proves RSM continuation
                }
                Case::InSmm => steps.push(Step { exit: smm_out(0xb2, 0x44), action: Action::Command { request: None, reject: true } }),
            }
        } else { mailbox_echo(code, steps, 0); }
    }
    fn normal_code(case: Case) -> (Vec<u8>, usize, Vec<Step>) {
        let mut code = vec![0xfa, 0xb8, 0, 0x10, 0x8e, 0xd8,
            0x0f, 0x01, 0x16, 0, 0x10, 0x0f, 0x20, 0xc0, 0x66, 0x83, 0xc8, 1,
            0x0f, 0x22, 0xc0, 0x66, 0xea];
        let protected = code.len() + 6;
        code.extend((RAM as u32 + protected as u32).to_le_bytes()); code.extend(8u16.to_le_bytes());
        code.extend([0x66, 0xb8, 0x10, 0, 0x8e, 0xd8, 0x8e, 0xc0, 0x8e, 0xd0, 0xfc]);
        let mut steps = vec![];
        for (reg, len, value) in [(0x50, 2, 0xffff), (0x9c, 1, 255), (0x52, 2, 2),
            (0xb0, 2, 0x2000), (0xa4, 4, 0x20000000), (0xa8, 4, 0x20000000),
            (0xac, 4, TSEG), (0x9e, 1, 0x3c), (0x9d, 1, 0xa)] {
            config_write(&mut code, &mut steps, false, 0, reg, len, value, Some(OPEN));
        }
        // Copy the private handler through actual CPU-open SMRAM, never DMA.
        code.push(0xbe); code.extend(HANDLER.to_le_bytes());
        code.push(0xbf); code.extend((SMBASE + 0x8000).to_le_bytes());
        code.push(0xb9); code.extend((handler(case).len() as u32).to_le_bytes()); code.extend([0xf3, 0xa4]);
        config_write(&mut code, &mut steps, false, 0, 0x9e, 1, 0x3d, Some(CLOSED));
        config_write(&mut code, &mut steps, false, 0, 0x9d, 1, 0x1a,
            Some(X86SmramState { tseg: X86TsegState::Locked, ..OPEN }));
        config_write(&mut code, &mut steps, false, 0, 0x9c, 1, 2, Some(LOCKED));
        for (reg, value) in [(0x60, 0), (0x64, 0), (0x60, ECAM as u32 | 1)] {
            config_write(&mut code, &mut steps, false, 0, reg, 4, value, None);
        }
        config_write(&mut code, &mut steps, false, 31, 0x40, 4, 0x600, None);
        config_write(&mut code, &mut steps, true, 31, 0x44, 1, 0x80, None);
        let smi_read = VcpuExit::PioIn { port: 0x630, len: 4 };
        read_echo(&mut code, &mut steps, smi_read.clone(), 0);
        command(&mut code, &mut steps, case, 0x40, 0xa0, None);
        port_write(&mut code, &mut steps, 0x630, 4, 0x21, Action::LpcWrite);
        config_write(&mut code, &mut steps, false, 31, 0xa0, 2, 0x10, None);
        let lock_read = VcpuExit::MmioRead { addr: ECAM + (31 << 15) + 0xa0, len: 2 };
        read_echo(&mut code, &mut steps, lock_read.clone(), 0x10);
        port_write(&mut code, &mut steps, 0x630, 4, 0x20, Action::LpcWrite);
        read_echo(&mut code, &mut steps, smi_read.clone(), 0x21);
        config_write(&mut code, &mut steps, true, 31, 0xa0, 2, 0, None);
        read_echo(&mut code, &mut steps, lock_read, 0x10);
        port_write(&mut code, &mut steps, 0x630, 4, 0, Action::LpcWrite);
        read_echo(&mut code, &mut steps, smi_read.clone(), 1);
        command(&mut code, &mut steps, case, 0x41, 0xa1, None);
        port_write(&mut code, &mut steps, 0x630, 4, 0x21, Action::LpcWrite);
        read_echo(&mut code, &mut steps, smi_read, 0x21);
        command(&mut code, &mut steps, case, 0x42, 0xa5, Some(1));
        if matches!(case, Case::Positive) {
            command(&mut code, &mut steps, case, 0x43, 0x5a, Some(2));
            port_write(&mut code, &mut steps, 0x714, 1, 0x51, Action::Observe);
        }
        code.extend([0xeb, 0xfe]);
        (code, protected, steps)
    }
    fn ram_image(case: Case) -> Vec<u8> {
        let mut image = vec![0; WINDOW];
        let code = normal_code(case).0; image[..code.len()].copy_from_slice(&code);
        image[0x1000..0x1002].copy_from_slice(&23u16.to_le_bytes());
        image[0x1002..0x1006].copy_from_slice(&(RAM as u32 + 0x1010).to_le_bytes());
        image[0x1010..0x1028].copy_from_slice(&[0u64, 0x00cf9a000000ffff, 0x00cf92000000ffff]
            .into_iter().flat_map(u64::to_le_bytes).collect::<Vec<_>>());
        let handler = handler(case); image[0x4000..0x4000 + handler.len()].copy_from_slice(&handler); image
    }
    fn reset_rom() -> Vec<u8> {
        let mut rom = vec![0xf4; WINDOW]; rom[0xfff0..0xfff5].copy_from_slice(&[0xea, 0, 0, 0, 0x10]); rom
    }
    fn cpu(case: Case) -> Result<(Box<dyn Vcpu>, MchPci)> {
        let hv = vroom_hv::default_hypervisor().map_err(|e| e.to_string())?;
        if hv.name() != "kvm" || hv.arch() != vroom_hv::Arch::X86_64 { return Err("requires native x86 KVM".into()); }
        let vm = hv.create_vm(&VmParams { vcpus: 1, irqchip: IrqChipConfig::Ioapic,
            nested: false, small_pages: false, helper_page: 0 }).map_err(|e| e.to_string())?;
        let caps = vm.configure_x86_q35_smram().map_err(|e| e.to_string())?;
        if !caps.vm_smm || caps.system_address_spaces < 2 { return Err("missing SMM capabilities".into()); }
        println!("{}", serde_json::json!({"test":"ich9_smi_capabilities","vm_smm":true,
            "system_address_spaces":caps.system_address_spaces}));
        let ram = GuestMemory::new(&[(GuestAddress(RAM), WINDOW as u64)]).map_err(|e| e.to_string())?;
        ram.write(GuestAddress(RAM), &ram_image(case)).map_err(|e| e.to_string())?;
        for (base, size) in [(SMBASE as u64, 0x20000), (TSEG as u64, 0x800000)] {
            if ram.check_range(GuestAddress(base), size) || ram.host_ranges(GuestAddress(base), size).is_some()
                || ram.write(GuestAddress(base), &[0]).is_ok() { return Err("private memory leaked into DMA".into()); }
        }
        let router = MchPci::with_q35_lpc(vec![], Bus::default(), &ram,
            BusRange { base: ECAM, len: 256 << 20 }, vec![], vec![
                BusRange { base: SMBASE as u64, len: 0x20000 }, BusRange { base: TSEG as u64, len: 0x800000 },
                BusRange { base: ROM, len: WINDOW as u64 }, BusRange { base: 0xffbfc000, len: 0x4000 },
                BusRange { base: 0xfec00000, len: 0x1000 }, BusRange { base: 0xfee00000, len: 0x1000 },
            ], LpcPioLayout::new(vec![BusRange { base: 0x600, len: 0x80 }],
                vec![BusRange { base: 0x700, len: 0x30 }]).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        vm.map_memory(&ram).map_err(|e| e.to_string())?;
        let rom = Arc::new(MmapRegion::new(WINDOW).map_err(|e| e.to_string())?);
        rom.load(&reset_rom()).map_err(|e| e.to_string())?; vm.map_readonly(ROM, rom).map_err(|e| e.to_string())?;
        let mut cpu = vm.create_vcpu(0).map_err(|e| e.to_string())?;
        cpu.set_boot_regs(&BootRegs::X86ResetVector).map_err(|e| e.to_string())?;
        vm.finalize().map_err(|e| e.to_string())?;
        drop(ram); drop(vm); drop(hv); Ok((cpu, router))
    }
    fn native(state: MchSmramState) -> X86SmramState {
        X86SmramState { tseg: match state.tseg {
            MchTsegState::Open => X86TsegState::Open, MchTsegState::Closed => X86TsegState::Closed,
            MchTsegState::Locked => X86TsegState::Locked,
        }, default_smbase_locked: state.default_smbase_locked }
    }
    fn exercise(case: Case) -> Result<()> {
        let (mut cpu, mut router) = cpu(case)?;
        let epoch = Instant::now();
        let mut injection_count = 0;
        let mut refused_in_smm = false;
        for step in normal_code(case).2 {
            let got = cpu.run().map_err(|e| e.to_string())?;
            if got != step.exit { return Err(format!("unexpected exit {got:?}; expected {:?}", step.exit)); }
            let elapsed_ns = u64::try_from(epoch.elapsed().as_nanos()).map_err(|_| "clock overflow")?;
            match step.action {
                Action::Observe => {}
                Action::Read(value) => {
                    let mut data = [0; 4];
                    match got {
                        VcpuExit::PioIn { port, len } | VcpuExit::X86SmmIo(X86IoExit::PioIn { port, len }) => {
                            if !router.lpc_pio_read(port, &mut data[..len], elapsed_ns).map_err(|e| e.to_string())? { return Err("unmapped LPC read".into()); }
                            if data[..len] != value.to_le_bytes()[..len] { return Err("wrong LPC readback".into()); }
                            cpu.complete_pio_in(&data[..len]).map_err(|e| e.to_string())?;
                        }
                        VcpuExit::MmioRead { addr, len } => {
                            if !router.mmio_read(addr, &mut data[..len]).map_err(|e| e.to_string())? { return Err("unmapped ECAM read".into()); }
                            if data[..len] != value.to_le_bytes()[..len] { return Err("wrong ECAM readback".into()); }
                            cpu.complete_mmio_read(&data[..len]).map_err(|e| e.to_string())?;
                        }
                        _ => return Err("invalid read step".into()),
                    }
                }
                Action::ConfigWrite(expected) => {
                    let mut applications = 0;
                    let mut apply = |requested| {
                        applications += 1; let target = native(requested);
                        if Some(target) != expected { return Err(PciConfigError::SmramBackend); }
                        let actual = cpu.apply_x86_smram(target).map_err(|_| PciConfigError::SmramBackend)?;
                        if actual != target { return Err(PciConfigError::SmramBackend); } Ok(requested)
                    };
                    match got {
                        VcpuExit::PioOut { port, len, data } => router.pio_write_with_smram(port, &data[..len], &mut apply).map_err(|e| e.to_string())?,
                        VcpuExit::MmioWrite { addr, len, data } => {
                            if !router.mmio_write_with_smram(addr, &data[..len], &mut apply).map_err(|e| e.to_string())? { return Err("unmapped ECAM write".into()); }
                        }
                        _ => return Err("invalid config write".into()),
                    }
                    if applications != usize::from(expected.is_some()) { return Err("wrong mapping callback count".into()); }
                }
                Action::LpcWrite => match got {
                    VcpuExit::PioOut { port, len, data } => {
                        if !router.lpc_pio_write(port, &data[..len], elapsed_ns).map_err(|e| e.to_string())? { return Err("unmapped LPC write".into()); }
                    }
                    _ => return Err("invalid LPC write".into()),
                }
                Action::Command { request, reject } => {
                    // Context comes only from this actual, already-matched RUN.
                    let (port, len, data, context) = match got {
                        VcpuExit::PioOut { port, len, data } => (port, len, data, LpcIoContext::Normal),
                        VcpuExit::X86SmmIo(X86IoExit::PioOut { port, len, data }) => (port, len, data, LpcIoContext::Smm),
                        _ => return Err("invalid APM write".into()),
                    };
                    let mut calls = 0;
                    let result = router.lpc_pio_write_with_smi(port, &data[..len], elapsed_ns, context, &mut |actual| {
                        calls += 1;
                        if request != Some(actual) || context != LpcIoContext::Normal { return Err(LpcError::SmiBackend); }
                        // The only injection site: no RUN, read completion or retry.
                        cpu.inject_smi().map_err(|_| LpcError::SmiBackend)
                    });
                    if calls != usize::from(request.is_some()) { return Err("wrong SMI callback count".into()); }
                    injection_count += calls;
                    if reject {
                        if result != Err(MchError::Lpc(LpcError::SmiContext)) { return Err("in-SMM APM command was not refused".into()); }
                        refused_in_smm = true;
                    } else if result.map_err(|e| e.to_string())? != true { return Err("unmapped APM command".into()); }
                }
            }
        }
        match case {
            Case::Positive if injection_count == 2 && !refused_in_smm => Ok(()),
            Case::InSmm if injection_count == 1 && refused_in_smm => Ok(()),
            _ => Err("wrong final request/refusal counts".into()),
        }
        // No RUN after final controller marker or the negative refusal.
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    pub fn run() -> Result<()> { exercise(Case::Positive)?; exercise(Case::InSmm) }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn codecs_keep_width_context_and_apm_reads_echoed() {
            let mut code = vec![]; let mut steps = vec![];
            port_write(&mut code, &mut steps, 0x630, 4, 0x21, Action::LpcWrite);
            assert_eq!(code, [0x66, 0xba, 0x30, 6, 0xb8, 0x21, 0, 0, 0, 0xef]);
            code.clear(); steps.clear();
            config_write(&mut code, &mut steps, true, 31, 0xa0, 2, 0x10, None);
            assert_eq!(code, [0x66, 0xc7, 5, 0xa0, 0x80, 0xf, 0xe0, 0x10, 0]);
            let handler = handler(Case::Positive);
            assert_eq!(&handler[..16], &[0xba, 0xb2, 0, 0xec, 0xba, 0x20, 7, 0xee, 0xba, 0xb3, 0, 0xec, 0xba, 0x21, 7, 0xee]);
            assert_eq!(&handler[handler.len()-2..], &[0x0f, 0xaa]);
            assert!(handler.windows(7).any(|b| b == [0x67, 0xfe, 5, 0, 0x20, 1, 0]));
        }
        #[test]
        fn plan_requires_actual_handler_rsm_and_contextual_negative_without_extra_injection() {
            for case in [Case::Positive, Case::InSmm] {
                let steps = normal_code(case).2;
                let commands = steps.iter().filter_map(|s| if let Action::Command { request, reject } = s.action {
                    Some((request, reject, &s.exit))
                } else { None }).collect::<Vec<_>>();
                assert_eq!(commands.iter().filter(|(r, _, _)| r.is_some()).count(), if matches!(case, Case::Positive) { 2 } else { 1 });
                assert_eq!(commands.iter().filter(|(r, no, _)| r.is_none() && !no).count(), 2);
                assert_eq!(commands.iter().filter(|(_, no, _)| *no).count(), usize::from(matches!(case, Case::InSmm)));
                for (index, step) in steps.iter().enumerate() {
                    if let Action::Read(value) = step.action {
                        let echo = &steps[index + 1]; assert!(matches!(echo.action, Action::Observe));
                        assert!(echo.exit == pio_out(0x712, 4, value)
                            || echo.exit == smm_out(0x720, value as u8) || echo.exit == smm_out(0x721, value as u8));
                    }
                }
                if matches!(case, Case::Positive) {
                    for count in [1, 2] {
                        let index = steps.iter().position(|s| s.exit == smm_out(0x722, count)).unwrap();
                        assert_eq!(steps[index + 1].exit, pio_out(0x713, 1, u32::from(count)));
                    }
                    assert_eq!(steps.last().unwrap().exit, pio_out(0x714, 1, 0x51));
                } else {
                    assert_eq!(steps.last().unwrap().exit, smm_out(0xb2, 0x44));
                    assert!(matches!(steps.last().unwrap().action, Action::Command { request: None, reject: true }));
                }
            }
        }
        #[test]
        fn private_handler_bounds_and_reset_entry_are_exact() {
            for case in [Case::Positive, Case::InSmm] {
                let image = ram_image(case); let (code, protected, _) = normal_code(case);
                assert!(code.len() < 0x1000); assert!(0x8000 + handler(case).len() < 0xfe00);
                assert_eq!(image.len(), WINDOW);
                assert_eq!(u32::from_le_bytes(code[protected-6..protected-2].try_into().unwrap()), RAM as u32 + protected as u32);
                assert_eq!(&image[0x1000..0x1006], &[23, 0, 0x10, 0x10, 1, 0]);
                assert_eq!(&reset_rom()[0xfff0..0xfff5], &[0xea, 0, 0, 0, 0x10]);
                assert!(0x20000 + 0x800000 + 2 * WINDOW < 9 * 1024 * 1024);
            }
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn main() {
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::Duration;
    if std::env::args_os().len() != 1 { eprintln!("ich9_smi accepts no arguments"); std::process::exit(2); }
    let (done, deadline) = mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if matches!(deadline.recv_timeout(Duration::from_secs(15)), Err(RecvTimeoutError::Timeout)) {
            eprintln!("native ICH9 SMI timed out after 15 seconds"); std::process::exit(124);
        }
    });
    let result = fixture::run(); let _ = done.send(());
    if watchdog.join().is_err() { eprintln!("watchdog failed"); std::process::exit(2); }
    match result {
        Ok(()) => println!("{}", serde_json::json!({"schema":1,"test":"native_ich9_smi","status":"pass",
            "vm_count":2,"max_live_vcpus":1,"smi_request_count":3,"positive_smi_requests":2,"negative_entry_requests":1,
            "disabled_commands_no_request":true,"smi_lock_sequence":true,"apmc_writable_after_lock":true,
            "handler_command_data_match":true,"smm_read_completion":true,"rsm_normal_progress":true,
            "in_smm_command_refused":true,"dma_ranges_absent":true,"controller_stop":true,
            "guest_s5":false,"sci":false,"os_or_firmware_boot":false,"vmm_profile":false})),
        Err(error) => { eprintln!("native ICH9 SMI failed: {error}"); std::process::exit(2); }
    }
}
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() {
    eprintln!("native ICH9 SMI requires Linux x86_64 KVM; no guest was run"); std::process::exit(2);
}

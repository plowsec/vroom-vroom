//! MCH-to-native-Q35 SMRAM register binding acceptance; no OS, firmware, disk or network.
//! Three sequential VMs, one CPU at a time, 15 seconds for the invocation.

#[cfg(any(test, all(target_os = "linux", target_arch = "x86_64")))]
#[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64")), allow(dead_code))]
mod fixture {
    use std::sync::Arc;
    use vroom_hv::{BootRegs, Error, IrqChipConfig, Vcpu, VcpuExit, VmParams,
        X86IoExit, X86SmramState, X86TsegState};
    use vroom_memory::{GuestAddress, GuestMemory, MmapRegion};
    use vroom_devices::{Bus, BusRange};
    use vroom_devices::pci::{MchPci, MchSmramState, MchTsegState, PciConfigError};

    const ECAM: u64 = 0xe0000000;
    const RAM: u64 = 0x10000;
    const ROM: u64 = 0xffff0000;
    const WINDOW: usize = 0x10000;
    const SMBASE: u32 = 0x30000;
    const DEFAULT_SENTINEL: u32 = 0x41000; // covers the second default-SMBASE64KiB
    const TSEG: u32 = 0x1f800000;
    const TSEG_SENTINEL: u32 = TSEG + 0x1000;
    const MAILBOX: u32 = RAM as u32 + 0x2000;
    const HANDLER: u32 = RAM as u32 + 0x4000;
    const OPEN: X86SmramState = X86SmramState { tseg: X86TsegState::Open, default_smbase_locked: false };
    const CLOSED: X86SmramState = X86SmramState { tseg: X86TsegState::Closed, ..OPEN };
    const LOCKED: X86SmramState = X86SmramState { tseg: X86TsegState::Locked, default_smbase_locked: true };
    type Result<T> = std::result::Result<T, String>;

    #[derive(Clone, Copy)]
    enum Case { Positive, TsegRead, DefaultRead }

    fn out(code: &mut Vec<u8>, port: u16, value: Option<u8>, mode32: bool) {
        if mode32 { code.push(0x66); }
        code.push(0xba); code.extend(port.to_le_bytes()); // MOV DX,port
        if let Some(value) = value { code.extend([0xb0, value]); }
        code.push(0xee); // OUT DX,AL
    }
    fn read32(code: &mut Vec<u8>, address: u32, mode32: bool) {
        if !mode32 { code.push(0x67); }
        code.push(0xa0); code.extend(address.to_le_bytes());
    }
    fn write32(code: &mut Vec<u8>, address: u32, value: u8, mode32: bool) {
        if !mode32 { code.push(0x67); }
        code.extend([0xc6, 0x05]); code.extend(address.to_le_bytes()); code.push(value);
    }

    fn smm_handler() -> Vec<u8> {
        // KVM's SMM entry has DS.base0/limit4GiB. Address-size32 accesses the
        // upper default window and high TSEG before any DS reload. End in RSM.
        let mut code = Vec::new();
        read32(&mut code, DEFAULT_SENTINEL, false);
        out(&mut code, 0x720, None, false);
        read32(&mut code, TSEG_SENTINEL, false);
        out(&mut code, 0x721, None, false);
        write32(&mut code, TSEG_SENTINEL + 1, 0x69, false);
        read32(&mut code, TSEG_SENTINEL + 1, false);
        out(&mut code, 0x722, None, false);
        write32(&mut code, MAILBOX, 0x69, false);
        code.extend([0x0f, 0xaa]);
        code
    }

    #[derive(Debug)]
    enum Step {
        Route { exit: VcpuExit, read: Option<u32>, state: Option<X86SmramState> },
        Observe(VcpuExit),
        Inject,
    }
    fn pio_out(port: u16, len: usize, value: u32) -> VcpuExit {
        let mut data = [0; 4]; data[..len].copy_from_slice(&value.to_le_bytes()[..len]);
        VcpuExit::PioOut { port, len, data }
    }
    fn port_write(code: &mut Vec<u8>, steps: &mut Vec<Step>, port: u16, len: usize, value: u32, state: Option<X86SmramState>) {
        code.extend([0x66, 0xba]); code.extend(port.to_le_bytes()); // MOV DX
        if len == 2 { code.push(0x66); }
        code.push(if len == 1 { 0xb0 } else { 0xb8 });
        code.extend(&value.to_le_bytes()[..len]);
        if len == 2 { code.push(0x66); }
        code.push(if len == 1 { 0xee } else { 0xef });
        steps.push(Step::Route { exit: pio_out(port, len, value), read: None, state });
    }
    fn config_write(code: &mut Vec<u8>, steps: &mut Vec<Step>, ecam: bool, reg: u8, len: usize, value: u32, state: Option<X86SmramState>) {
        if ecam {
            if len == 2 { code.push(0x66); }
            code.extend([if len == 1 { 0xc6 } else { 0xc7 }, 0x05]);
            code.extend((ECAM as u32 + u32::from(reg)).to_le_bytes());
            code.extend(&value.to_le_bytes()[..len]);
            let mut data = [0; 8]; data[..len].copy_from_slice(&value.to_le_bytes()[..len]);
            steps.push(Step::Route { exit: VcpuExit::MmioWrite { addr: ECAM + u64::from(reg), len, data }, read: None, state });
        } else {
            port_write(code, steps, 0xcf8, 4, 0x80000000 | u32::from(reg & !3), None);
            port_write(code, steps, 0xcfc + u16::from(reg & 3), len, value, state);
        }
    }
    fn config_read(code: &mut Vec<u8>, steps: &mut Vec<Step>, ecam: bool, reg: u8, len: usize, value: u32) {
        if !ecam { port_write(code, steps, 0xcf8, 4, 0x80000000 | u32::from(reg & !3), None); }
        code.extend([0x31, 0xc0]); // zero EAX before byte/word read
        let exit = if ecam {
            if len == 2 { code.push(0x66); }
            code.push(if len == 1 { 0xa0 } else { 0xa1 });
            code.extend((ECAM as u32 + u32::from(reg)).to_le_bytes());
            VcpuExit::MmioRead { addr: ECAM + u64::from(reg), len }
        } else {
            code.extend([0x66, 0xba]); code.extend((0xcfc + u16::from(reg & 3)).to_le_bytes());
            if len == 2 { code.push(0x66); }
            code.push(if len == 1 { 0xec } else { 0xed });
            VcpuExit::PioIn { port: 0xcfc + u16::from(reg & 3), len }
        };
        steps.push(Step::Route { exit, read: Some(value), state: None });
        code.extend([0x66, 0xba, 0x12, 0x07, 0xef]); // OUT DX,EAX: prove CPU received exact completion
        steps.push(Step::Observe(pio_out(0x712, 4, value)));
    }

    fn normal_code(case: Case) -> (Vec<u8>, usize, Vec<Step>) {
        // Reset ROM jumps to1000:0000. Load a flat 32-bit GDT from common
        // RAM, then enter protected mode (no paging or interrupts). No stack
        // is needed by these straight-line snippets.
        let mut code = vec![0xfa, 0xb8, 0x00, 0x10, 0x8e, 0xd8,
            0x0f, 0x01, 0x16, 0x00, 0x10, // LGDT [DS:1000]
            0x0f, 0x20, 0xc0, 0x66, 0x83, 0xc8, 0x01, 0x0f, 0x22, 0xc0,
            0x66, 0xea]; // MOV CR0,OR PE; far JMP ptr16:32
        let protected = code.len() + 6;
        code.extend((RAM as u32 + protected as u32).to_le_bytes());
        code.extend(8u16.to_le_bytes());
        code.extend([0x66, 0xb8, 0x10, 0x00, 0x8e, 0xd8, 0x8e, 0xc0, 0x8e, 0xd0, 0xfc]);
        let mut steps = Vec::new();
        // Observed firmware query/geometry order through CF8, then enable real
        // ECAM. Each state write is checked against an actual backend callback.
        for (reg, len, value, read) in [(0x50, 2, 0xffff, 8), (0x9c, 1, 255, 1),
            (0x52, 2, 2, 2), (0xb0, 2, 0x2000, 0x2000),
            (0xa4, 4, 0x20000000, 0x20000000), (0xa8, 4, 0x20000000, 0x20000000),
            (0xac, 4, TSEG, TSEG), (0x9e, 1, 0x3d, 0x3d)] {
            config_write(&mut code, &mut steps, false, reg, len, value, Some(OPEN));
            config_read(&mut code, &mut steps, false, reg, len, read);
        }
        config_write(&mut code, &mut steps, false, 0x9d, 1, 0xa, Some(CLOSED));
        config_write(&mut code, &mut steps, false, 0x60, 4, 0, None);
        config_write(&mut code, &mut steps, false, 0x64, 4, 0, None);
        config_write(&mut code, &mut steps, false, 0x60, 4, ECAM as u32 | 1, None);
        config_write(&mut code, &mut steps, true, 0x9e, 1, 0x3c, Some(OPEN));
        config_read(&mut code, &mut steps, true, 0x9e, 1, 0x3c);
        // CPU initializes private backing only while its AS0 view is open.
        code.push(0xbe); code.extend(HANDLER.to_le_bytes());
        code.push(0xbf); code.extend((SMBASE + 0x8000).to_le_bytes());
        code.push(0xb9); code.extend((smm_handler().len() as u32).to_le_bytes());
        code.extend([0xf3, 0xa4]); // REP MOVSB, not string PIO
        write32(&mut code, DEFAULT_SENTINEL, 0x5a, true);
        write32(&mut code, TSEG_SENTINEL, 0x3c, true);
        config_write(&mut code, &mut steps, true, 0x9e, 1, 0x3d, Some(CLOSED));
        config_write(&mut code, &mut steps, true, 0x9e, 1, 0x3c, Some(OPEN));
        read32(&mut code, TSEG_SENTINEL, true);
        out(&mut code, 0x711, None, true);
        steps.push(Step::Observe(pio_out(0x711, 1, 0x3c)));
        write32(&mut code, TSEG_SENTINEL, 0xa6, true);
        config_write(&mut code, &mut steps, true, 0x9e, 1, 0x3d, Some(CLOSED));
        let tseg_locked = X86SmramState { tseg: X86TsegState::Locked, ..OPEN };
        config_write(&mut code, &mut steps, true, 0x9d, 1, 0x1a, Some(tseg_locked));
        config_read(&mut code, &mut steps, false, 0x9d, 1, 0x1a);
        config_write(&mut code, &mut steps, true, 0x9c, 1, 2, Some(LOCKED));
        // Ordinary guest writes cannot downgrade either backend lock.
        for (reg, value, read) in [(0x9c, 0, 2), (0x9d, 0x40, 0x1a), (0x9e, 0, 0x3d)] {
            config_write(&mut code, &mut steps, true, reg, 1, value, Some(LOCKED));
            config_read(&mut code, &mut steps, true, reg, 1, read);
        }
        out(&mut code, 0x710, Some(0x13), true);
        match case {
            Case::Positive => {
                steps.push(Step::Inject);
                for (port, value) in [(0x720, 0x5a), (0x721, 0xa6), (0x722, 0x69)] {
                    steps.push(Step::Observe(VcpuExit::X86SmmIo(X86IoExit::PioOut {
                        port, len: 1, data: [value, 0, 0, 0] })));
                }
                out(&mut code, 0x710, Some(0x14), true);
                steps.push(Step::Observe(pio_out(0x710, 1, 0x14)));
                read32(&mut code, MAILBOX, true);
                out(&mut code, 0x713, None, true);
                steps.push(Step::Observe(pio_out(0x713, 1, 0x69)));
            }
            Case::TsegRead | Case::DefaultRead => {
                steps.push(Step::Observe(pio_out(0x710, 1, 0x13)));
                read32(&mut code, if matches!(case, Case::TsegRead) { TSEG_SENTINEL } else { DEFAULT_SENTINEL }, true);
            }
        }
        code.extend([0xeb, 0xfe]);
        (code, protected, steps)
    }

    fn ram_image(case: Case) -> Vec<u8> {
        let mut image = vec![0; WINDOW];
        let (code, _, _) = normal_code(case);
        image[..code.len()].copy_from_slice(&code);
        image[0x1000..0x1002].copy_from_slice(&23u16.to_le_bytes());
        image[0x1002..0x1006].copy_from_slice(&(RAM as u32 + 0x1010).to_le_bytes());
        image[0x1010..0x1028].copy_from_slice(&[0u64, 0x00cf9a000000ffff, 0x00cf92000000ffff]
            .into_iter().flat_map(u64::to_le_bytes).collect::<Vec<_>>());
        let handler = smm_handler();
        image[0x4000..0x4000 + handler.len()].copy_from_slice(&handler);
        image
    }

    fn reset_rom() -> Vec<u8> {
        let mut rom = vec![0xf4; WINDOW];
        rom[0xfff0..0xfff5].copy_from_slice(&[0xea, 0, 0, 0, 0x10]); rom
    }

    fn cpu(case: Case) -> Result<(Box<dyn Vcpu>, MchPci)> {
        let hv = vroom_hv::default_hypervisor().map_err(|e| e.to_string())?;
        if hv.name() != "kvm" || hv.arch() != vroom_hv::Arch::X86_64 { return Err("requires native x86 KVM".into()); }
        let vm = hv.create_vm(&VmParams { vcpus: 1, irqchip: IrqChipConfig::Ioapic,
            nested: false, small_pages: false, helper_page: 0 }).map_err(|e| e.to_string())?;
        let caps = vm.configure_x86_q35_smram().map_err(|e| e.to_string())?;
        if !caps.vm_smm || caps.system_address_spaces < 2 { return Err("missing SMM capabilities".into()); }
        println!("{}", serde_json::json!({"test":"mch_smram_capabilities","vm_smm":true,
            "system_address_spaces":caps.system_address_spaces}));
        let ram = GuestMemory::new(&[(GuestAddress(RAM), WINDOW as u64)]).map_err(|e| e.to_string())?;
        ram.write(GuestAddress(RAM), &ram_image(case)).map_err(|e| e.to_string())?;
        for (base, size) in [(SMBASE as u64, 0x20000), (TSEG as u64, 0x800000)] {
            if ram.check_range(GuestAddress(base), size) || ram.host_ranges(GuestAddress(base), size).is_some()
                || ram.write(GuestAddress(base), &[0]).is_ok() { return Err("private memory leaked into DMA".into()); }
        }
        let router = MchPci::with_q35_smram(vec![], Bus::default(), &ram,
            BusRange { base: ECAM, len: 256 << 20 }, vec![], vec![
                BusRange { base: SMBASE as u64, len: 0x20000 },
                BusRange { base: TSEG as u64, len: 0x800000 },
                BusRange { base: ROM, len: WINDOW as u64 },
                BusRange { base: 0xffbfc000, len: 0x4000 },
                BusRange { base: 0xfec00000, len: 0x1000 },
                BusRange { base: 0xfee00000, len: 0x1000 },
            ]).map_err(|e| e.to_string())?;
        vm.map_memory(&ram).map_err(|e| e.to_string())?;
        let rom = Arc::new(MmapRegion::new(WINDOW).map_err(|e| e.to_string())?);
        rom.load(&reset_rom()).map_err(|e| e.to_string())?;
        vm.map_readonly(ROM, rom).map_err(|e| e.to_string())?;
        let mut cpu = vm.create_vcpu(0).map_err(|e| e.to_string())?;
        if cpu.apply_x86_smram(OPEN).is_ok() { return Err("fresh CPU accepted transition".into()); }
        cpu.set_boot_regs(&BootRegs::X86ResetVector).map_err(|e| e.to_string())?;
        vm.finalize().map_err(|e| e.to_string())?;
        // Continue only through the CPU's retained VM fd and backing owners.
        drop(ram); drop(vm); drop(hv); Ok((cpu, router))
    }

    fn native(state: MchSmramState) -> X86SmramState {
        X86SmramState { tseg: match state.tseg {
            MchTsegState::Open => X86TsegState::Open,
            MchTsegState::Closed => X86TsegState::Closed,
            MchTsegState::Locked => X86TsegState::Locked,
        }, default_smbase_locked: state.default_smbase_locked }
    }
    fn exercise(case: Case) -> Result<()> {
        let (mut cpu, mut router) = cpu(case)?;
        for step in normal_code(case).2 {
            let got = cpu.run().map_err(|e| e.to_string())?;
            let expected = match &step {
                Step::Route { exit, .. } | Step::Observe(exit) => exit.clone(),
                Step::Inject => pio_out(0x710, 1, 0x13),
            };
            if got != expected { return Err(format!("unexpected exit {got:?}; expected {expected:?}")); }
            match step {
                Step::Observe(_) => {}
                Step::Inject => cpu.inject_smi().map_err(|e| e.to_string())?,
                Step::Route { exit, read, state } => {
                    if let Some(value) = read {
                        let mut data = [0u8; 4];
                        let len = match exit {
                            VcpuExit::PioIn { port, len } => {
                                router.pio_read(port, &mut data[..len]).map_err(|e| e.to_string())?;
                                if data[..len] != value.to_le_bytes()[..len] { return Err("wrong PIO register readback".into()); }
                                cpu.complete_pio_in(&data[..len]).map_err(|e| e.to_string())?; len
                            }
                            VcpuExit::MmioRead { addr, len } => {
                                if !router.mmio_read(addr, &mut data[..len]).map_err(|e| e.to_string())? { return Err("ECAM read unmapped".into()); }
                                if data[..len] != value.to_le_bytes()[..len] { return Err("wrong ECAM register readback".into()); }
                                cpu.complete_mmio_read(&data[..len]).map_err(|e| e.to_string())?; len
                            }
                            _ => return Err("invalid read step".into()),
                        };
                        if !matches!(len, 1 | 2 | 4) { return Err("invalid completion length".into()); }
                    } else {
                        let mut applications = 0;
                        let mut apply = |requested| {
                            applications += 1;
                            let target = native(requested);
                            if Some(target) != state { return Err(PciConfigError::SmramBackend); }
                            let actual = cpu.apply_x86_smram(target).map_err(|_| PciConfigError::SmramBackend)?;
                            if actual != target || cpu.apply_x86_smram(target).is_ok() { return Err(PciConfigError::SmramBackend); }
                            Ok(requested)
                        };
                        match exit {
                            VcpuExit::PioOut { port, len, data } => router.pio_write_with_smram(port, &data[..len], &mut apply).map_err(|e| e.to_string())?,
                            VcpuExit::MmioWrite { addr, len, data } => {
                                if !router.mmio_write_with_smram(addr, &data[..len], &mut apply).map_err(|e| e.to_string())? { return Err("ECAM write unmapped".into()); }
                            }
                            _ => return Err("invalid write step".into()),
                        }
                        if applications != usize::from(state.is_some()) { return Err("wrong number of mapping applications".into()); }
                    }
                }
            }
        }
        if !matches!(case, Case::Positive) {
            match cpu.run() {
                Err(Error::GuestFault(message)) if message == "normal-context access to private SMRAM" => {}
                other => return Err(format!("private access not denied: {other:?}")),
            }
            if cpu.run().is_ok() || cpu.complete_mmio_read(&[0]).is_ok() || cpu.inject_smi().is_ok()
                || cpu.apply_x86_smram(OPEN).is_ok() { return Err("private denial did not latch".into()); }
        }
        Ok(())
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    pub fn run() -> Result<()> { exercise(Case::Positive)?; exercise(Case::TsegRead)?; exercise(Case::DefaultRead) }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn scalar_codecs_preserve_operand_size_and_exact_read_completion() {
            for (len, mov, io) in [(1, vec![0xb0, 0x34], vec![0xee]),
                (2, vec![0x66, 0xb8, 0x34, 0x12], vec![0x66, 0xef]),
                (4, vec![0xb8, 0x34, 0x12, 0, 0], vec![0xef])] {
                let mut code = vec![]; let mut steps = vec![];
                port_write(&mut code, &mut steps, 0xcfc, len, 0x1234, None);
                assert_eq!(code, [vec![0x66, 0xba, 0xfc, 0x0c], mov, io].concat());
                assert_eq!(steps.len(), 1);
            }
            let mut code = vec![]; let mut steps = vec![];
            config_write(&mut code, &mut steps, true, 0x9e, 1, 0x3d, Some(CLOSED));
            assert_eq!(code, [0xc6, 0x05, 0x9e, 0, 0, 0xe0, 0x3d]);
            code.clear(); steps.clear();
            config_read(&mut code, &mut steps, true, 0x50, 2, 8);
            assert_eq!(code, [0x31, 0xc0, 0x66, 0xa1, 0x50, 0, 0, 0xe0, 0x66, 0xba, 0x12, 7, 0xef]);
            assert!(matches!(steps[0], Step::Route { exit: VcpuExit::MmioRead { len: 2, .. }, read: Some(8), state: None }));
            assert!(matches!(&steps[1], Step::Observe(exit) if *exit == pio_out(0x712, 4, 8)));
        }

        #[test]
        fn register_sequence_requires_both_transports_and_actual_backend_applications() {
            for case in [Case::Positive, Case::TsegRead, Case::DefaultRead] {
                let steps = normal_code(case).2;
                let requests = steps.iter().filter_map(|step| match step {
                    Step::Route { state: Some(state), .. } => Some(*state), _ => None,
                }).collect::<Vec<_>>();
                assert_eq!(requests, [vec![OPEN; 8], vec![CLOSED, OPEN, CLOSED, OPEN, CLOSED],
                    vec![X86SmramState { tseg: X86TsegState::Locked, ..OPEN }], vec![LOCKED; 4]].concat());
                assert!(steps.iter().any(|s| matches!(s, Step::Route { exit: VcpuExit::PioIn { len: 4, .. }, .. })));
                assert!(steps.iter().any(|s| matches!(s, Step::Route { exit: VcpuExit::MmioRead { len: 1, .. }, .. })));
                for (index, step) in steps.iter().enumerate() {
                    if let Step::Route { read: Some(value), .. } = step {
                        assert!(matches!(&steps[index + 1], Step::Observe(exit) if *exit == pio_out(0x712, 4, *value)));
                    }
                }
                assert_eq!(steps.iter().filter(|s| matches!(s, Step::Inject)).count(), usize::from(matches!(case, Case::Positive)));
            }
        }
        #[test]
        fn protected_entry_targets_flat_gdt_and_owned_common_ram() {
            for case in [Case::Positive, Case::TsegRead, Case::DefaultRead] {
                let image = ram_image(case);
                let (code, protected, _) = normal_code(case);
                assert_eq!(image.len(), WINDOW); assert!(code.len() < 0x1000);
                assert_eq!(u32::from_le_bytes(code[protected - 6..protected - 2].try_into().unwrap()), RAM as u32 + protected as u32);
                assert_eq!(&code[protected - 2..protected], &8u16.to_le_bytes());
                assert_eq!(&code[protected..protected + 4], &[0x66, 0xb8, 0x10, 0]);
                assert_eq!(&image[0x1000..0x1006], &[23, 0, 0x10, 0x10, 1, 0]);
                assert_eq!(&image[0x1018..0x1020], &0x00cf9a000000ffffu64.to_le_bytes());
                assert_eq!(&image[0x1020..0x1028], &0x00cf92000000ffffu64.to_le_bytes());
                assert_eq!(&code[code.len()-2..], &[0xeb, 0xfe]);
            }
        }
        #[test]
        fn handler_uses_upper_default_range_and_tseg_without_touching_save_area() {
            let handler = smm_handler();
            assert!(0x8000 + handler.len() < 0xfe00);
            assert_eq!(&handler[handler.len()-2..], &[0x0f, 0xaa]);
            assert!(handler.windows(6).any(|b| b == [0x67, 0xa0, 0, 0x10, 4, 0]));
            assert!(handler.windows(6).any(|b| b == [0x67, 0xa0, 0, 0x10, 0x80, 0x1f]));
            assert_eq!(&reset_rom()[0xfff0..0xfff5], &[0xea, 0, 0, 0, 0x10]);
            assert!(0x20000 + 0x800000 + 2 * WINDOW < 9 * 1024 * 1024);
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn main() {
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::Duration;
    if std::env::args_os().len() != 1 {
        eprintln!("mch_smram accepts no arguments"); std::process::exit(2);
    }
    let (done, deadline) = mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if matches!(deadline.recv_timeout(Duration::from_secs(15)), Err(RecvTimeoutError::Timeout)) {
            eprintln!("native MCH SMRAM timed out after 15 seconds"); std::process::exit(124);
        }
    });
    let result = fixture::run();
    let _ = done.send(());
    if watchdog.join().is_err() { eprintln!("watchdog failed"); std::process::exit(2); }
    match result {
        Ok(()) => println!("{}", serde_json::json!({"schema":1,"test":"native_mch_smram","status":"pass",
            "vm_count":3,"max_live_vcpus":1,"cpu_initialization":true,"tseg_close_reopen":true,
            "cf8_ecam_binding":true,"masked_unlocks_ignored":true,"both_locked":true,"smi_rsm":true,"normal_private_access_denied":true,
            "dma_ranges_absent":true,"full_512mib_board":false,"os_or_firmware_boot":false})),
        Err(error) => { eprintln!("native MCH SMRAM failed: {error}"); std::process::exit(2); }
    }
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() {
    eprintln!("native MCH SMRAM requires Linux x86_64 KVM; no guest was run");
    std::process::exit(2);
}

//! Fixed-Q35 native SMRAM acceptance; no OS, firmware, disk or network.
//! Three sequential VMs, one CPU at a time, 15 seconds for the invocation.

#[cfg(any(test, all(target_os = "linux", target_arch = "x86_64")))]
#[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64")), allow(dead_code))]
mod fixture {
    use std::sync::Arc;
    use vroom_hv::{BootRegs, Error, IrqChipConfig, Vcpu, VcpuExit, VmParams,
        X86IoExit, X86SmramState, X86TsegState};
    use vroom_memory::{GuestAddress, GuestMemory, MmapRegion};

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

    fn normal_code(case: Case) -> (Vec<u8>, usize) {
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
        // CPU itself initializes the owned SMRAM through its temporary AS0
        // view; no host pointer or GuestMemory write reaches private backing.
        code.push(0xbe); code.extend(HANDLER.to_le_bytes()); // MOV ESI,source
        code.push(0xbf); code.extend((SMBASE + 0x8000).to_le_bytes()); // MOV EDI,dest
        code.push(0xb9); code.extend((smm_handler().len() as u32).to_le_bytes());
        code.extend([0xf3, 0xa4]); // REP MOVSB (memory copy, no string PIO)
        write32(&mut code, DEFAULT_SENTINEL, 0x5a, true);
        write32(&mut code, TSEG_SENTINEL, 0x3c, true);
        out(&mut code, 0x710, Some(0x10), true); // host closes TSEG
        out(&mut code, 0x710, Some(0x11), true); // host reopens TSEG
        read32(&mut code, TSEG_SENTINEL, true);
        out(&mut code, 0x711, None, true); // same bytes survive close/reopen
        write32(&mut code, TSEG_SENTINEL, 0xa6, true);
        out(&mut code, 0x710, Some(0x12), true); // host locks BOTH regions
        out(&mut code, 0x710, Some(0x13), true); // fresh write exit: SMI or denial
        match case {
            Case::Positive => {
                out(&mut code, 0x710, Some(0x14), true); // only AFTER real RSM
                read32(&mut code, MAILBOX, true);
                out(&mut code, 0x713, None, true);
            }
            Case::TsegRead => read32(&mut code, TSEG_SENTINEL, true),
            Case::DefaultRead => read32(&mut code, DEFAULT_SENTINEL, true),
        }
        code.extend([0xeb, 0xfe]); // cannot fabricate a terminal success
        (code, protected)
    }

    fn ram_image(case: Case) -> Vec<u8> {
        let mut image = vec![0; WINDOW];
        let (code, _) = normal_code(case);
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

    fn cpu(case: Case) -> Result<Box<dyn Vcpu>> {
        let hv = vroom_hv::default_hypervisor().map_err(|e| e.to_string())?;
        if hv.name() != "kvm" || hv.arch() != vroom_hv::Arch::X86_64 { return Err("requires native x86 KVM".into()); }
        let vm = hv.create_vm(&VmParams { vcpus: 1, irqchip: IrqChipConfig::Ioapic,
            nested: false, small_pages: false, helper_page: 0 }).map_err(|e| e.to_string())?;
        let caps = vm.configure_x86_q35_smram().map_err(|e| e.to_string())?;
        if !caps.vm_smm || caps.system_address_spaces < 2 { return Err("missing SMM capabilities".into()); }
        println!("{}", serde_json::json!({"test":"q35_smram_capabilities","vm_smm":true,
            "system_address_spaces":caps.system_address_spaces}));
        let ram = GuestMemory::new(&[(GuestAddress(RAM), WINDOW as u64)]).map_err(|e| e.to_string())?;
        ram.write(GuestAddress(RAM), &ram_image(case)).map_err(|e| e.to_string())?;
        for (base, size) in [(SMBASE as u64, 0x20000), (TSEG as u64, 0x800000)] {
            if ram.check_range(GuestAddress(base), size) || ram.host_ranges(GuestAddress(base), size).is_some()
                || ram.write(GuestAddress(base), &[0]).is_ok() { return Err("private memory leaked into DMA".into()); }
        }
        vm.map_memory(&ram).map_err(|e| e.to_string())?;
        let rom = Arc::new(MmapRegion::new(WINDOW).map_err(|e| e.to_string())?);
        rom.load(&reset_rom()).map_err(|e| e.to_string())?;
        vm.map_readonly(ROM, rom).map_err(|e| e.to_string())?;
        let mut cpu = vm.create_vcpu(0).map_err(|e| e.to_string())?;
        if cpu.apply_x86_smram(OPEN).is_ok() { return Err("fresh CPU accepted transition".into()); }
        cpu.set_boot_regs(&BootRegs::X86ResetVector).map_err(|e| e.to_string())?;
        vm.finalize().map_err(|e| e.to_string())?;
        // Continue only through the CPU's retained VM fd and backing owners.
        drop(ram); drop(vm); drop(hv); Ok(cpu)
    }

    fn expect(cpu: &mut dyn Vcpu, port: u16, value: u8, smm: bool) -> Result<()> {
        let data = [value, 0, 0, 0];
        let wanted = if smm { VcpuExit::X86SmmIo(X86IoExit::PioOut { port, len: 1, data }) }
            else { VcpuExit::PioOut { port, len: 1, data } };
        let got = cpu.run().map_err(|e| e.to_string())?;
        if got != wanted { return Err(format!("unexpected exit {got:?}; expected {wanted:?}")); }
        Ok(())
    }
    fn apply(cpu: &mut dyn Vcpu, state: X86SmramState) -> Result<()> {
        let actual = cpu.apply_x86_smram(state).map_err(|e| e.to_string())?;
        if actual != state { return Err("mapping readback differs from applied state".into()); }
        if cpu.apply_x86_smram(state).is_ok() { return Err("write-exit grant reused".into()); }
        Ok(())
    }
    fn initialize_and_lock(cpu: &mut dyn Vcpu) -> Result<()> {
        expect(cpu, 0x710, 0x10, false)?; apply(cpu, CLOSED)?;
        expect(cpu, 0x710, 0x11, false)?; apply(cpu, OPEN)?;
        expect(cpu, 0x711, 0x3c, false)?;
        expect(cpu, 0x710, 0x12, false)?; apply(cpu, LOCKED)?;
        expect(cpu, 0x710, 0x13, false)?; Ok(())
    }
    fn positive() -> Result<()> {
        let mut cpu = cpu(Case::Positive)?;
        initialize_and_lock(&mut *cpu)?;
        cpu.inject_smi().map_err(|e| e.to_string())?;
        expect(&mut *cpu, 0x720, 0x5a, true)?;
        expect(&mut *cpu, 0x721, 0xa6, true)?;
        expect(&mut *cpu, 0x722, 0x69, true)?;
        expect(&mut *cpu, 0x710, 0x14, false)?;
        apply(&mut *cpu, LOCKED)?; // successful idempotent state on a new exit
        expect(&mut *cpu, 0x713, 0x69, false)?;
        Ok(())
    }
    fn denial(case: Case) -> Result<()> {
        let mut cpu = cpu(case)?;
        initialize_and_lock(&mut *cpu)?;
        match cpu.run() {
            Err(Error::GuestFault(message)) if message == "normal-context access to private SMRAM" => {}
            other => return Err(format!("private access not denied: {other:?}")),
        }
        if cpu.run().is_ok() || cpu.complete_mmio_read(&[0]).is_ok() || cpu.inject_smi().is_ok()
            || cpu.apply_x86_smram(OPEN).is_ok() { return Err("private denial did not latch".into()); }
        Ok(())
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    pub fn run() -> Result<()> { positive()?; denial(Case::TsegRead)?; denial(Case::DefaultRead) }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn protected_entry_targets_flat_gdt_and_owned_common_ram() {
            for case in [Case::Positive, Case::TsegRead, Case::DefaultRead] {
                let image = ram_image(case);
                let (code, protected) = normal_code(case);
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
        eprintln!("q35_smram accepts no arguments"); std::process::exit(2);
    }
    let (done, deadline) = mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if matches!(deadline.recv_timeout(Duration::from_secs(15)), Err(RecvTimeoutError::Timeout)) {
            eprintln!("native Q35 SMRAM timed out after 15 seconds"); std::process::exit(124);
        }
    });
    let result = fixture::run();
    let _ = done.send(());
    if watchdog.join().is_err() { eprintln!("watchdog failed"); std::process::exit(2); }
    match result {
        Ok(()) => println!("{}", serde_json::json!({"schema":1,"test":"native_q35_smram","status":"pass",
            "vm_count":3,"max_live_vcpus":1,"cpu_initialization":true,"tseg_close_reopen":true,
            "both_locked":true,"smi_rsm":true,"normal_private_access_denied":true,
            "dma_ranges_absent":true,"os_or_firmware_boot":false})),
        Err(error) => { eprintln!("native Q35 SMRAM failed: {error}"); std::process::exit(2); }
    }
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() {
    eprintln!("native Q35 SMRAM requires Linux x86_64 KVM; no guest was run");
    std::process::exit(2);
}

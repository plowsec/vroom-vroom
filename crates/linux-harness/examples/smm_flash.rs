//! Restricted native KVM SMM/flash acceptance. No OS, firmware, disk or network.
//! One invocation, four sequential VMs, one CPU at a time, 15 seconds overall.

#[cfg(any(test, all(target_os = "linux", target_arch = "x86_64")))]
#[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64")), allow(dead_code))]
mod fixture {
    use std::sync::Arc;
    use vroom_devices::pc_flash::{PcFlash, PcFlashAccess, PcFlashError, SECTOR_SIZE};
    use vroom_hv::{BootRegs, Error, IrqChipConfig, VmParams, Vcpu, VcpuExit, X86IoExit};
    use vroom_memory::{GuestAddress, GuestMemory, MmapRegion};

    const RAM: u64 = 0x10000;
    const SMRAM: u64 = 0x30000;
    const ROM: u64 = 0xffff0000;
    const FLASH: u64 = 0x100000;
    const WINDOW: usize = 0x10000;
    type Result<T> = std::result::Result<T, String>;

    #[derive(Clone, Copy)]
    enum Case { Positive, PrivateRead, PrivateWrite, FlashWrite }

    // All snippets execute in 16-bit real/SMM mode. Except the explicit SMM
    // entry ROM probe below, DS-relative offsets stay below64KiB. No assembler
    // or downloaded binary is needed; these bytes are part of the source.
    fn ds(code: &mut Vec<u8>, segment: u16) {
        code.push(0xb8); code.extend(segment.to_le_bytes()); // MOV AX,segment
        code.extend([0x8e, 0xd8]); // MOV DS,AX
    }
    fn read(code: &mut Vec<u8>, offset: u16) {
        code.push(0xa0); code.extend(offset.to_le_bytes()); // MOV AL,[moffs16]
    }
    fn write(code: &mut Vec<u8>, offset: u16, value: u8) {
        code.extend([0xc6, 0x06]); code.extend(offset.to_le_bytes()); code.push(value);
    }
    fn port(code: &mut Vec<u8>, port: u16) {
        code.push(0xba); code.extend(port.to_le_bytes()); // MOV DX,port
    }
    fn out(code: &mut Vec<u8>, number: u16, byte: Option<u8>) {
        port(code, number);
        if let Some(byte) = byte { code.extend([0xb0, byte]); }
        code.push(0xee);
    }

    fn normal_code(case: Case) -> Vec<u8> {
        let mut code = vec![0xfa]; // CLI
        match case {
            Case::Positive => {
                ds(&mut code, 0x1000);
                write(&mut code, 0x2000, 0x3c);
                port(&mut code, 0x708); code.push(0xec); // normal IN completion gate
                out(&mut code, 0x709, None);
                out(&mut code, 0x700, Some(0x11)); // host injects SMI here
                out(&mut code, 0x700, Some(0x22)); // observed only AFTER RSM
                read(&mut code, 0x2001); // SMM's write to common RAM
                out(&mut code, 0x707, None);
                ds(&mut code, 0xffff);
                read(&mut code, 0x10); // 0xffff0+0x10=FLASH
                out(&mut code, 0x706, None);
            }
            Case::PrivateRead => { ds(&mut code, 0x3000); read(&mut code, 0x1000); }
            Case::PrivateWrite => { ds(&mut code, 0x3000); write(&mut code, 0x1000, 0x55); }
            Case::FlashWrite => { ds(&mut code, 0xffff); write(&mut code, 0x10, 0x10); }
        }
        code.extend([0xeb, 0xfe]); // cannot manufacture a successful terminal exit
        code
    }

    fn private_image() -> Vec<u8> {
        let mut image = vec![0; WINDOW];
        image[0x1000] = 0x5a;
        // KVM enter_smm sets DS.base=0, limit=0xffffffff. Probe common ROM
        // using address-size32 BEFORE loading DS; no normal-mode unreal trick.
        let mut code = vec![0x67, 0xa0, 0xe0, 0xff, 0xff, 0xff];
        out(&mut code, 0x70a, None);
        ds(&mut code, 0x3000);
        read(&mut code, 0x1000);
        out(&mut code, 0x701, None); // private sentinel
        ds(&mut code, 0x1000);
        read(&mut code, 0x2000);
        out(&mut code, 0x702, None); // normal-mode common RAM value
        write(&mut code, 0x2001, 0x69);
        ds(&mut code, 0xffff);
        write(&mut code, 0x10, 0x10); // byte-program command
        write(&mut code, 0x10, 0xa6); // programmed byte
        write(&mut code, 0x10, 0xff); // array mode
        read(&mut code, 0x10);
        out(&mut code, 0x703, None); // proves completed SMM MMIO read
        port(&mut code, 0x704); code.push(0xec);
        code.extend([0x34, 0xff]); // XOR AL,0xff: independently check completion
        out(&mut code, 0x705, None);
        code.extend([0x0f, 0xaa]); // RSM; KVM restores the real interrupted state
        assert!(0x8000 + code.len() < 0xfe00); // never overwrite KVM's save area
        image[0x8000..0x8000 + code.len()].copy_from_slice(&code);
        image
    }

    fn reset_rom() -> Vec<u8> {
        let mut rom = vec![0xf4; WINDOW];
        rom[0xffe0] = 0x6c; // observed through the AS1 common-ROM mirror
        // Reset CS.base=ffff0000, IP=fff0; far JMP1000:0000 to common RAM.
        rom[0xfff0..0xfff5].copy_from_slice(&[0xea, 0, 0, 0, 0x10]);
        rom
    }

    fn bad(message: &str) -> String { message.into() }
    fn expect(cpu: &mut dyn Vcpu, wanted: VcpuExit) -> Result<()> {
        let got = cpu.run().map_err(|e| e.to_string())?;
        if got != wanted { return Err(format!("unexpected exit {got:?}; expected {wanted:?}")); }
        Ok(())
    }
    fn pio(number: u16, value: u8, smm: bool) -> VcpuExit {
        let data = [value, 0, 0, 0];
        if smm { VcpuExit::X86SmmIo(X86IoExit::PioOut { port: number, len: 1, data }) }
        else { VcpuExit::PioOut { port: number, len: 1, data } }
    }

    /// A fresh VM for each case. Returning only the CPU drops both public VM
    /// and GuestMemory handles before execution, exercising backing lifetime.
    fn cpu(case: Case) -> Result<Box<dyn Vcpu>> {
        let hv = vroom_hv::default_hypervisor().map_err(|e| e.to_string())?;
        if hv.name() != "kvm" || hv.arch() != vroom_hv::Arch::X86_64 { return Err(bad("requires native x86 KVM")); }
        let vm = hv.create_vm(&VmParams { vcpus: 1, irqchip: IrqChipConfig::Ioapic,
            nested: false, small_pages: false, helper_page: 0 }).map_err(|e| e.to_string())?;
        let capabilities = vm.configure_x86_smm(&private_image()).map_err(|e| e.to_string())?;
        if !capabilities.vm_smm || capabilities.system_address_spaces < 2 { return Err(bad("missing SMM capabilities")); }
        println!("{}", serde_json::json!({"test":"native_smm_capabilities","vm_smm":true,
            "system_address_spaces":capabilities.system_address_spaces}));
        let memory = GuestMemory::new(&[(GuestAddress(RAM), WINDOW as u64)]).map_err(|e| e.to_string())?;
        memory.write(GuestAddress(RAM), &normal_code(case)).map_err(|e| e.to_string())?;
        if memory.check_range(GuestAddress(SMRAM), WINDOW as u64)
            || memory.host_ranges(GuestAddress(SMRAM), WINDOW as u64).is_some()
            || memory.write(GuestAddress(SMRAM), &[0]).is_ok() { return Err(bad("SMRAM leaked into device DMA memory")); }
        vm.map_memory(&memory).map_err(|e| e.to_string())?;
        let rom = Arc::new(MmapRegion::new(WINDOW).map_err(|e| e.to_string())?);
        rom.load(&reset_rom()).map_err(|e| e.to_string())?;
        vm.map_readonly(ROM, rom).map_err(|e| e.to_string())?;
        let mut cpu = vm.create_vcpu(0).map_err(|e| e.to_string())?;
        if cpu.inject_smi().is_ok() { return Err(bad("uninitialized SMI accepted")); }
        cpu.set_boot_regs(&BootRegs::X86ResetVector).map_err(|e| e.to_string())?;
        if vm.map_memory(&memory).is_ok() || vm.configure_x86_smm(&private_image()).is_ok() {
            return Err(bad("SMM topology was not frozen"));
        }
        vm.finalize().map_err(|e| e.to_string())?;
        drop(memory); drop(vm); drop(hv);
        Ok(cpu)
    }

    fn complete(cpu: &mut dyn Vcpu, mmio: bool, value: u8) -> Result<()> {
        if cpu.run().is_ok() || cpu.inject_smi().is_ok() || cpu.set_boot_regs(&BootRegs::X86ResetVector).is_ok() {
            return Err(bad("pending read allowed RUN, SMI or boot mutation"));
        }
        let wrong = if mmio { cpu.complete_mmio_read(&[value; 2]) } else { cpu.complete_pio_in(&[value; 2]) };
        let wrong_kind = if mmio { cpu.complete_pio_in(&[value]) } else { cpu.complete_mmio_read(&[value]) };
        if wrong.is_ok() || wrong_kind.is_ok() { return Err(bad("inexact read completion accepted")); }
        if mmio { cpu.complete_mmio_read(&[value]) } else { cpu.complete_pio_in(&[value]) }.map_err(|e| e.to_string())?;
        let repeat = if mmio { cpu.complete_mmio_read(&[value]) } else { cpu.complete_pio_in(&[value]) };
        if repeat.is_ok() { return Err(bad("duplicate read completion accepted")); }
        Ok(())
    }

    fn positive() -> Result<()> {
        let mut cpu = cpu(Case::Positive)?;
        let mut flash = PcFlash::new(vec![0x5a; SECTOR_SIZE]).map_err(|e| e.to_string())?;
        expect(&mut *cpu, VcpuExit::PioIn { port: 0x708, len: 1 })?;
        complete(&mut *cpu, false, 0x4d)?;
        expect(&mut *cpu, pio(0x709, 0x4d, false))?;
        expect(&mut *cpu, pio(0x700, 0x11, false))?;
        cpu.inject_smi().map_err(|e| e.to_string())?;
        expect(&mut *cpu, pio(0x70a, 0x6c, true))?;
        expect(&mut *cpu, pio(0x701, 0x5a, true))?;
        expect(&mut *cpu, pio(0x702, 0x3c, true))?;
        for value in [0x10, 0xa6, 0xff] {
            expect(&mut *cpu, VcpuExit::X86SmmIo(X86IoExit::MmioWrite { addr: FLASH,
                len: 1, data: [value, 0, 0, 0, 0, 0, 0, 0] }))?;
            // Authorization context comes only from the validated exit above.
            flash.write(PcFlashAccess::SystemManagement, 0, &[value]).map_err(|e| e.to_string())?;
        }
        expect(&mut *cpu, VcpuExit::X86SmmIo(X86IoExit::MmioRead { addr: FLASH, len: 1 }))?;
        let mut value = [0];
        flash.read(PcFlashAccess::SystemManagement, 0, &mut value).map_err(|e| e.to_string())?;
        if value != [0xa6] { return Err(bad("flash programming did not persist")); }
        complete(&mut *cpu, true, value[0])?;
        expect(&mut *cpu, pio(0x703, 0xa6, true))?;
        expect(&mut *cpu, VcpuExit::X86SmmIo(X86IoExit::PioIn { port: 0x704, len: 1 }))?;
        complete(&mut *cpu, false, 0x7b)?;
        expect(&mut *cpu, pio(0x705, 0x84, true))?;
        expect(&mut *cpu, pio(0x700, 0x22, false))?; // RSM restored normal context
        expect(&mut *cpu, pio(0x707, 0x69, false))?; // shared AS1 write visible in AS0
        expect(&mut *cpu, VcpuExit::MmioRead { addr: FLASH, len: 1 })?;
        flash.read(PcFlashAccess::Normal, 0, &mut value).map_err(|e| e.to_string())?;
        complete(&mut *cpu, true, value[0])?;
        expect(&mut *cpu, pio(0x706, 0xa6, false))?;
        Ok(()) // checked sequence complete; close CPU, then its backing owners
    }

    fn private_denial(case: Case) -> Result<()> {
        let mut cpu = cpu(case)?;
        match cpu.run() {
            Err(Error::GuestFault(message)) if message == "normal-context access to private SMRAM" => {}
            other => return Err(format!("private access was not denied: {other:?}")),
        }
        if cpu.run().is_ok() || cpu.complete_mmio_read(&[0]).is_ok() || cpu.inject_smi().is_ok() {
            return Err(bad("private-access failure did not latch"));
        }
        Ok(())
    }

    fn flash_denial() -> Result<()> {
        let mut cpu = cpu(Case::FlashWrite)?;
        let mut flash = PcFlash::new(vec![0x5a; SECTOR_SIZE]).map_err(|e| e.to_string())?;
        expect(&mut *cpu, VcpuExit::MmioWrite { addr: FLASH, len: 1, data: [0x10, 0, 0, 0, 0, 0, 0, 0] })?;
        if flash.write(PcFlashAccess::Normal, 0, &[0x10]) != Err(PcFlashError::WriteProtected) {
            return Err(bad("normal-context flash write was not denied"));
        }
        let mut bytes = [0; 8];
        flash.read(PcFlashAccess::Normal, 0, &mut bytes).map_err(|e| e.to_string())?;
        if bytes != [0x5a; 8] { return Err(bad("denied flash write changed contents")); }
        Ok(()) // do not rerun a guest after denying its write
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    pub fn run() -> Result<()> {
        positive()?;
        private_denial(Case::PrivateRead)?;
        private_denial(Case::PrivateWrite)?;
        flash_denial()?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn snippets_fit_private_window_without_touching_save_area() {
            let image = private_image();
            assert_eq!(image.len(), WINDOW);
            assert_eq!(image[0x1000], 0x5a);
            assert!(image[0xfe00..].iter().all(|&b| b == 0));
            assert!(image[0x8000..0xfe00].windows(2).any(|b| b == [0x0f, 0xaa]));
            for case in [Case::Positive, Case::PrivateRead, Case::PrivateWrite, Case::FlashWrite] {
                let code = normal_code(case);
                assert!(code.len() < 0x1000);
                assert_eq!(&code[code.len()-2..], &[0xeb, 0xfe]);
            }
            assert_eq!(&reset_rom()[0xfff0..0xfff5], &[0xea, 0, 0, 0, 0x10]);
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn main() {
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::Duration;
    if std::env::args_os().len() != 1 {
        eprintln!("smm_flash accepts no arguments"); std::process::exit(2);
    }
    let (done, deadline) = mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if matches!(deadline.recv_timeout(Duration::from_secs(15)), Err(RecvTimeoutError::Timeout)) {
            eprintln!("native SMM flash timed out after 15 seconds"); std::process::exit(124);
        }
    });
    let result = fixture::run();
    let _ = done.send(());
    if watchdog.join().is_err() { eprintln!("watchdog failed"); std::process::exit(2); }
    match result {
        Ok(()) => println!("{}", serde_json::json!({"schema":1,"test":"native_smm_flash","status":"pass",
            "vm_count":4,"max_live_vcpus":1,"smi_rsm":true,"private_memory_exclusion":true,
            "same_run_context":true,"flash_normal_write_denied":true,"exact_read_completion":true,
            "os_or_firmware_boot":false})),
        Err(error) => { eprintln!("native SMM flash failed: {error}"); std::process::exit(2); }
    }
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() {
    eprintln!("native SMM flash requires Linux x86_64 KVM; no guest was run");
    std::process::exit(2);
}

//! Which exit do we get for a given MRS/MSR? Debug aid for guest OS bring-up.
//! Usage: sysreg_probe <instruction word hex>

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn main() {
    use vroom_hv::*;
    use vroom_memory::{GuestAddress, GuestMemory};
    let insn = u32::from_str_radix(std::env::args().nth(1).unwrap_or("d53b9c08".into()).trim_start_matches("0x"), 16)
        .expect("hex instruction");
    let hv = default_hypervisor().expect("hv");
    let vm = hv
        .create_vm(&VmParams {
            vcpus: 1,
            irqchip: IrqChipConfig::GicV3 { dist_base: 0x0800_0000, redist_base: 0x080a_0000, msi: None },
            nested: false,
        })
        .expect("vm");
    let ram = GuestAddress(0x4000_0000);
    let mem = GuestMemory::new(&[(ram, 0x10_0000)]).expect("mem");
    // insn; hvc #0
    mem.write_obj(ram, &insn).expect("w");
    mem.write_obj(GuestAddress(ram.0 + 4), &0xd400_0002u32).expect("w");
    // Vector table at ram+0x800: any exception -> hvc #1
    for off in (0..0x800).step_by(0x80) {
        mem.write_obj(GuestAddress(ram.0 + 0x800 + off), &0xd400_0022u32).expect("w");
    }
    vm.map_memory(&mem).expect("map");
    let mut cpu = vm.create_vcpu(0).expect("vcpu");
    cpu.set_boot_regs(&BootRegs::Aarch64 { pc: ram.0, x: [0; 4] }).expect("regs");
    let mut st = cpu.save_state().expect("save");
    for r in st.regs.iter_mut() {
        if r.0 == 0x1_0000 | 0xc600 {
            r.1 = ram.0 + 0x800; // VBAR_EL1
        }
    }
    cpu.restore_state(&st).expect("restore");
    let e = cpu.run().expect("run");
    let st = cpu.save_state().expect("save");
    let reg = |id: u32| st.regs.iter().find(|r| r.0 == id).map(|r| r.1).unwrap_or(0);
    println!("exit {e:x?}; x8={:#x} ESR_EL1={:#x}", reg(8), reg(0x1_0000 | 0xc290));
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn main() {}

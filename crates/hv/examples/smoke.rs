//! Smoke test for the host backend: run a few instructions, check exits.
//! macOS: needs the hypervisor entitlement (scripts/sign.sh).

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn main() {
    use bulwark_hv::*;
    use bulwark_memory::{GuestAddress, GuestMemory};

    let hv = default_hypervisor().expect("hypervisor");
    let info = hv.irqchip_info().expect("irqchip info");
    println!("irqchip: {info:x?}");
    let vm = hv
        .create_vm(&VmParams {
            vcpus: 1,
            irqchip: IrqChipConfig::GicV3 { dist_base: 0x0800_0000, redist_base: 0x080a_0000, msi: None },
        })
        .expect("vm");
    let ram = GuestAddress(0x4000_0000);
    let mem = GuestMemory::new(&[(ram, 0x10_0000)]).expect("mem");
    let code: [u32; 7] =
        [0xd2800540, 0xd2a12001, 0xb9000020, 0xb9400422, 0xd2b08000, 0xf2800100, 0xd4000002];
    for (i, w) in code.iter().enumerate() {
        mem.write_obj(GuestAddress(ram.0 + 4 * i as u64), w).expect("write code");
    }
    vm.map_memory(&mem).expect("map");
    let mut cpu = vm.create_vcpu(0).expect("vcpu");
    cpu.set_boot_regs(&BootRegs::Aarch64 { pc: ram.0, x: [0; 4] }).expect("regs");

    let e = cpu.run().expect("run1");
    assert_eq!(e, VcpuExit::MmioWrite { addr: 0x0900_0000, len: 4, data: [42, 0, 0, 0, 0, 0, 0, 0] });
    let e = cpu.run().expect("run2");
    assert_eq!(e, VcpuExit::MmioRead { addr: 0x0900_0004, len: 4 });
    cpu.complete_mmio_read(&[7, 0, 0, 0]).expect("complete");
    let e = cpu.run().expect("run3");
    assert_eq!(e, VcpuExit::Hypercall { func: 0x8400_0008, args: [0x0900_0000, 7, 0] });
    let st = cpu.save_state().expect("save");
    println!("saved {} regs, clock {}", st.regs.len(), vm.clock());
    let gic = vm.save_irqchip().expect("gic save");
    println!("gic state {} bytes", gic.blob.len());
    vm.restore_irqchip(&gic).expect("gic restore");
    cpu.restore_state(&st).expect("restore");
    println!("smoke OK");
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn main() {}

//! Smoke test for the host backend: run a few instructions, check exits.
//! macOS: needs the hypervisor entitlement (scripts/sign.sh).

#[cfg(all(any(target_os = "macos", target_os = "linux"), target_arch = "aarch64"))]
fn main() {
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::Duration;
    use vroom_hv::*;
    use vroom_memory::{GuestAddress, GuestMemory};

    // A backend regression must not leave a paid host waiting in vCPU.run().
    let (done, deadline) = mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if matches!(deadline.recv_timeout(Duration::from_secs(10)), Err(RecvTimeoutError::Timeout)) {
            eprintln!("smoke: timed out after 10 seconds");
            std::process::exit(2);
        }
    });
    let hv = default_hypervisor().expect("hypervisor");
    let info = hv.irqchip_info().expect("irqchip info");
    println!("irqchip: {info:x?}");
    let vm = hv
        .create_vm(&VmParams {
            vcpus: 1,
            irqchip: IrqChipConfig::GicV3 { dist_base: 0x0800_0000, redist_base: 0x080a_0000, msi: None },
            nested: false,
            small_pages: false,
            helper_page: 0x0f00_0000,
        })
        .expect("vm");
    let ram = GuestAddress(0x4000_0000);
    let mem = GuestMemory::new(&[(ram, 0x10_0000)]).expect("mem");
    // Write 42, read a supplied value, write it back, then PSCI SYSTEM_OFF.
    let code: [u32; 8] =
        [0xd2800540, 0xd2a12001, 0xb9000020, 0xb9400422, 0xb9000822, 0xd2b08000, 0xf2800100, 0xd4000002];
    for (i, w) in code.iter().enumerate() {
        mem.write_obj(GuestAddress(ram.0 + 4 * i as u64), w).expect("write code");
    }
    vm.map_memory(&mem).expect("map");
    let mut cpu = vm.create_vcpu(0).expect("vcpu");
    cpu.set_boot_regs(&BootRegs::Aarch64 { pc: ram.0, x: [0; 4] }).expect("regs");
    vm.finalize().expect("finalize");

    // Exercise initial state APIs before any MMIO completion or PSCI shutdown.
    let st = cpu.save_state().expect("save");
    println!("saved {} regs, clock {}", st.regs.len() + st.raw.len(), vm.clock());
    let gic = vm.save_irqchip().expect("gic save");
    println!("gic state {} bytes", gic.blob.len());
    vm.restore_irqchip(&gic).expect("gic restore");
    // Without CPU restoration this skips `mov x0, #42`, so the first write
    // would contain 99. This checks restoration is used, not just accepted.
    // The GIC round-trip above only checks its API, not interrupt delivery.
    cpu.set_boot_regs(&BootRegs::Aarch64 { pc: ram.0 + 4, x: [99, 0, 0, 0] }).expect("perturb regs");
    cpu.restore_state(&st).expect("restore");

    let e = cpu.run().expect("run1");
    assert_eq!(e, VcpuExit::MmioWrite { addr: 0x0900_0000, len: 4, data: [42, 0, 0, 0, 0, 0, 0, 0] });
    let e = cpu.run().expect("run2");
    assert_eq!(e, VcpuExit::MmioRead { addr: 0x0900_0004, len: 4 });
    cpu.complete_mmio_read(&[7, 0, 0, 0]).expect("complete");
    let e = cpu.run().expect("run3");
    assert_eq!(e, VcpuExit::MmioWrite { addr: 0x0900_0008, len: 4, data: [7, 0, 0, 0, 0, 0, 0, 0] });
    let e = cpu.run().expect("run4");
    #[cfg(target_os = "linux")]
    assert_eq!(e, VcpuExit::SystemOff); // KVM handles PSCI in the kernel.
    #[cfg(target_os = "macos")]
    assert_eq!(e, VcpuExit::Hypercall { func: 0x8400_0008, args: [0x0900_0000, 7, 0] });
    done.send(()).expect("watchdog");
    watchdog.join().expect("watchdog join");
    println!("smoke OK");
}

#[cfg(not(all(any(target_os = "macos", target_os = "linux"), target_arch = "aarch64")))]
fn main() {
    eprintln!(
        "smoke: this example only tests macOS or Linux on aarch64; no test ran on {}/{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
    );
    std::process::exit(2);
}

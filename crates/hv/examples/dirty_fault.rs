//! What a guest's first write to a page costs while RAM writes are logged
//! (a stage-2 fault: an exit, the page made writable): the guest writes a
//! word in each of N pages (8192, or the first argument), with the log
//! just taken (every write faults) and again (none does). Then what
//! protecting them again at the take costs, in one call for all or one
//! call each (every other page written: N/2 writable islands). That one
//! grows with the islands: ~3 µs a call for 2048 pages, 25-36 µs for 32768.
//! macOS: needs the hypervisor entitlement (scripts/sign.sh).

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn main() {
    use vroom_hv::*;
    use vroom_memory::{GuestAddress, GuestMemory};

    const RAM: u64 = 0x4000_0000;
    const DATA: u64 = RAM + 0x10_0000;
    const STRIDE: u64 = 0x4000;

    let pages: u64 = std::env::args().nth(1).map_or(8192, |n| n.parse().expect("page count"));
    let hv = default_hypervisor().expect("hypervisor");
    let vm = hv
        .create_vm(&VmParams {
            vcpus: 1,
            irqchip: IrqChipConfig::GicV3 { dist_base: 0x0800_0000, redist_base: 0x080a_0000, msi: None },
            nested: false,
            small_pages: false,
            helper_page: 0x0f00_0000,
        })
        .expect("vm");
    let mem = GuestMemory::new(&[(GuestAddress(RAM), 0x10_0000 + pages * STRIDE)]).expect("mem");
    // loop: str x2, [x0]; add x0, x0, x3; subs x1, x1, #1; b.ne loop; hvc #0
    for (i, w) in [0xf9000002u32, 0x8b030000, 0xf1000421, 0x54ffffa1, 0xd4000002].iter().enumerate() {
        mem.write_obj(GuestAddress(RAM + 4 * i as u64), w).expect("code");
    }
    vm.map_memory(&mem).expect("map");
    let mut cpu = vm.create_vcpu(0).expect("vcpu");
    let pass = |cpu: &mut Box<dyn Vcpu>, pages: u64, stride: u64| {
        cpu.set_boot_regs(&BootRegs::Aarch64 { pc: RAM, x: [DATA, pages, 7, stride] }).expect("regs");
        let t = std::time::Instant::now();
        match cpu.run().expect("run") {
            VcpuExit::Hypercall { .. } => t.elapsed(),
            e => panic!("unexpected exit {e:?}"),
        }
    };
    for round in 0..4 {
        // Afresh each round: pages written again soon would stay writable.
        let t = std::time::Instant::now();
        vm.start_dirty_log(&mem).expect("dirty log");
        let all = t.elapsed();
        let faulting = pass(&mut cpu, pages, STRIDE);
        let writable = pass(&mut cpu, pages, STRIDE);
        let t = std::time::Instant::now();
        let written = vm.take_dirty_log().expect("take");
        let take = t.elapsed();
        assert_eq!(written.iter().map(|(_, len)| len / STRIDE).sum::<u64>(), pages);
        println!(
            "round {round}: {:.2} us a fault ({:.1} ms for {pages} pages, {:.2} ms without faults); take {:.2} ms; all RAM protected in {:.2} ms",
            (faulting - writable).as_secs_f64() * 1e6 / pages as f64,
            faulting.as_secs_f64() * 1e3,
            writable.as_secs_f64() * 1e3,
            take.as_secs_f64() * 1e3,
            all.as_secs_f64() * 1e3,
        );
        vm.start_dirty_log(&mem).expect("dirty log");
        pass(&mut cpu, pages / 2, 2 * STRIDE);
        let t = std::time::Instant::now();
        let written = vm.take_dirty_log().expect("take");
        let apart = t.elapsed();
        assert_eq!(written.len() as u64, pages / 2);
        println!(
            "         protect again: {:.2} us a call for {} pages apart, one call for all {pages} in the take above",
            apart.as_secs_f64() * 1e6 / (pages / 2) as f64,
            pages / 2,
        );
    }
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn main() {}

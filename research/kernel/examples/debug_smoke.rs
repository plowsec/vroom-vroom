//! Real HVF debug validation, independent of any guest disk.
//! Build, scripts/sign.sh target/debug/examples/debug_smoke, then run.

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn main() {
    use vroom_hv::{debug::*, *};
    use vroom_memory::{GuestAddress, GuestMemory};
    let hv = default_hypervisor().expect("hypervisor");
    let vm = hv
        .create_vm(&VmParams {
            vcpus: 1,
            irqchip: IrqChipConfig::GicV3 {
                dist_base: 0x0800_0000,
                redist_base: 0x080a_0000,
                msi: None,
            },
            nested: false,
            small_pages: false,
        })
        .expect("vm");
    let base = 0x4000_0000;
    let mem = GuestMemory::new(&[(GuestAddress(base), 0x10_0000)]).expect("memory");
    // mov x0,#42; msr DBGBCR0_EL1,xzr; mrs x3,DBGBCR0_EL1;
    // add x0,x0,#1; brk #0x123; b .
    for (i, word) in [
        0xd2800540_u32,
        0xd51000bf,
        0xd53000a3,
        0x91000400,
        0xd4202460,
        0x14000000,
    ]
    .iter()
    .enumerate()
    {
        mem.write_obj(GuestAddress(base + i as u64 * 4), word)
            .expect("code");
    }
    vm.map_memory(&mem).expect("map");
    let mut cpu = vm.create_vcpu(0).expect("cpu");
    cpu.set_boot_regs(&BootRegs::Aarch64 {
        pc: base,
        x: [0; 4],
    })
    .expect("boot regs");
    let pristine = cpu.save_state().expect("pristine guest registers");
    let mut config = DebugConfig {
        enabled: true,
        trap_software_breakpoints: true,
        breakpoints: vec![base + 12],
        ..Default::default()
    };
    cpu.configure_debug(&config)
        .expect("enable hardware breakpoint");
    cpu.restore_state(&pristine)
        .expect("CPU reset preserves host breakpoints");
    // Prevent a backend regression from hanging a test indefinitely.
    let kick = cpu.kicker();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .is_err()
        {
            kick.kick();
        }
    });
    let first = cpu.run().expect("hardware breakpoint run");
    assert!(
        matches!(first, VcpuExit::Debug(DebugStop { pc, reason: DebugReason::Breakpoint, .. }) if pc == base + 12),
        "{first:?}"
    );
    assert_eq!(
        cpu.save_state()
            .expect("registers")
            .regs
            .iter()
            .find(|r| r.0 == 0)
            .map(|r| r.1),
        Some(42)
    );
    println!("hardware breakpoint: stopped before ADD, x0=42");
    assert_eq!(
        cpu.save_state()
            .expect("guest shadow")
            .regs
            .iter()
            .find(|r| r.0 == 3)
            .map(|r| r.1),
        Some(0)
    );
    println!("guest debug-register writes/readback and CPU reset preserve host breakpoint");
    config.breakpoints.clear();
    config.single_step = true;
    cpu.configure_debug(&config).expect("arm single-step");
    let second = cpu.run().expect("step run");
    assert!(
        matches!(second, VcpuExit::Debug(DebugStop { pc, reason: DebugReason::SingleStep, .. }) if pc == base + 16),
        "{second:?}"
    );
    assert_eq!(
        cpu.save_state()
            .expect("registers")
            .regs
            .iter()
            .find(|r| r.0 == 0)
            .map(|r| r.1),
        Some(43)
    );
    println!("single-step: exactly one ADD, x0=43");
    config.single_step = false;
    cpu.configure_debug(&config).expect("disarm single-step");
    let third = cpu.run().expect("BRK run");
    assert!(
        matches!(third, VcpuExit::Debug(DebugStop { pc, reason: DebugReason::SoftwareBreakpoint, .. }) if pc == base + 16),
        "{third:?}"
    );
    println!("BRK: preserved faulting PC and immediate");
    cpu.restore_state(&pristine).expect("restart step test");
    config.single_step = true;
    for i in 1..=3 {
        cpu.configure_debug(&config).expect("arm instruction step");
        let event = cpu
            .run()
            .expect("step through guest debug-register accesses");
        assert!(
            matches!(event, VcpuExit::Debug(DebugStop { pc, reason: DebugReason::SingleStep, .. }) if pc == base + i*4),
            "{event:?}"
        );
    }
    println!("single-step: emulated MSR/MRS each retire exactly one instruction");
    // A guest-owned BRK must reach the guest's normal EL1 exception vector.
    let mut guest = pristine.clone();
    for (id, value) in &mut guest.regs {
        if *id == 31 {
            *value = base + 16;
        }
        if *id == 0x1c600 {
            *value = base + 0x8000;
        }
    }
    for (i, word) in [0xd5385204_u32, 0xd5384025, 0xd5384006, 0xd4000002]
        .iter()
        .enumerate()
    {
        mem.write_obj(GuestAddress(base + 0x8200 + i as u64 * 4), word)
            .expect("exception handler");
    }
    cpu.restore_state(&guest).expect("install guest vectors");
    config.single_step = false;
    config.trap_software_breakpoints = false;
    cpu.configure_debug(&config).expect("forward guest BRK");
    assert!(matches!(
        cpu.run().expect("guest exception handler"),
        VcpuExit::Hypercall { .. }
    ));
    let state = cpu.save_state().expect("handler context");
    let reg = |id| state.regs.iter().find(|r| r.0 == id).expect("register").1;
    assert_eq!(reg(4) & 0xffff, 0x123, "BRK immediate survives injection");
    assert_eq!(reg(5), base + 16, "ELR points at faulting instruction");
    assert_eq!(
        reg(6),
        pristine
            .regs
            .iter()
            .find(|r| r.0 == 34)
            .expect("initial PSTATE")
            .1
    );
    println!("guest BRK: dispatched to EL1 with original ESR, ELR and SPSR");
    // str x0,[x1]; ldr x2,[x1]; hvc #0. Verify both access directions.
    for (i, word) in [0xf9000020_u32, 0xf9400022, 0xd4000002].iter().enumerate() {
        mem.write_obj(GuestAddress(base + 0x100 + i as u64 * 4), word)
            .expect("watch code");
    }
    let mut watch_state = pristine.clone();
    for (id, value) in &mut watch_state.regs {
        if *id == 31 {
            *value = base + 0x100;
        }
        if *id == 0 {
            *value = 0x1234;
        }
        if *id == 1 {
            *value = base + 0x2000;
        }
    }
    cpu.restore_state(&watch_state).expect("watch registers");
    config.watchpoints = vec![Watchpoint {
        address: base + 0x2000,
        size: 8,
        read: false,
        write: true,
    }];
    cpu.configure_debug(&config).expect("write watchpoint");
    let event = cpu.run().expect("watched store");
    assert!(
        matches!(event,VcpuExit::Debug(DebugStop {reason:DebugReason::Watchpoint,pc,..}) if pc==base+0x100),
        "{event:?}"
    );
    config.watchpoints.clear();
    config.single_step = true;
    cpu.configure_debug(&config)
        .expect("step past watched store");
    assert!(matches!(
        cpu.run().expect("watchpoint step"),
        VcpuExit::Debug(DebugStop {
            reason: DebugReason::SingleStep,
            ..
        })
    ));
    config.single_step = false;
    config.watchpoints = vec![Watchpoint {
        address: base + 0x2000,
        size: 8,
        read: true,
        write: false,
    }];
    cpu.configure_debug(&config).expect("read watchpoint");
    let event = cpu.run().expect("watched load");
    assert!(
        matches!(event,VcpuExit::Debug(DebugStop {reason:DebugReason::Watchpoint,pc,..}) if pc==base+0x104),
        "{event:?}"
    );
    println!("watchpoints: store/load trap at exact PC; step-over retires store");
    cpu.configure_debug(&DebugConfig::default())
        .expect("detach");
    let _ = done_tx.send(());
    watchdog.join().expect("watchdog");
    println!("debug smoke OK");
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn main() {
    eprintln!("debug_smoke requires macOS ARM64 with Hypervisor.framework");
}

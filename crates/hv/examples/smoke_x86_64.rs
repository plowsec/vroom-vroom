//! Native vroom-hv backend/control smoke with initial CPU state capture.
//! No OS, firmware or snapshot restore.
//! Linux x86_64 with /dev/kvm only; deliberately no emulation fallback.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn main() {
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::{Duration, Instant};
    use vroom_hv::*;
    use vroom_memory::{GuestAddress, GuestMemory};

    let (done, deadline) = mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if matches!(deadline.recv_timeout(Duration::from_secs(10)), Err(RecvTimeoutError::Timeout)) {
            eprintln!("native x86_64 smoke timed out after 10 seconds");
            std::process::exit(2);
        }
    });
    let hv = default_hypervisor().expect("KVM backend");
    assert_eq!(hv.arch(), Arch::X86_64);
    let vm = hv.create_vm(&VmParams {
        vcpus: 1, irqchip: IrqChipConfig::Ioapic, nested: false,
        small_pages: false, helper_page: 0,
    }).expect("VM");
    let memory = GuestMemory::new(&[(GuestAddress(0), 2 << 20)]).expect("RAM");
    // Identity-map the first GiB with 2 MiB pages. Only the first 2 MiB is
    // backed by RAM; 0x200000 is our harmless MMIO fixture, not a device.
    memory.write_obj(GuestAddress(0x1000), &0x2003u64).expect("PML4");
    memory.write_obj(GuestAddress(0x2000), &0x3003u64).expect("PDPT");
    for page in 0..512u64 {
        memory.write_obj(GuestAddress(0x3000 + page * 8), &((page << 21) | 0x83)).expect("PDE");
    }
    for (i, value) in [0u64, 0, 0x00af_9b00_0000_ffff, 0x00cf_9300_0000_ffff].iter().enumerate() {
        memory.write_obj(GuestAddress(0x5000 + i as u64 * 8), value).expect("GDT");
    }
    const MAILBOX: u64 = 0x8000;
    const MMIO: u64 = 0x20_0000;
    let mut code = vec![
        0x66, 0xba, 0xe9, 0, 0xb0, 42, 0xee, // OUT byte 42 to 0xe9
        0x66, 0xba, 0xea, 0, 0xec,             // IN byte from 0xea
        0x66, 0xba, 0xeb, 0, 0xee,             // OUT that byte to 0xeb
        0x48, 0xb9,                            // MOVABS RCX, MMIO
    ];
    code.extend_from_slice(&MMIO.to_le_bytes());
    code.extend_from_slice(&[
        0xb8, 42, 0, 0, 0, 0x48, 0x89, 0x01, // MOV [RCX], RAX (42)
        0x48, 0x8b, 0x41, 8,                  // MOV RAX, [RCX+8]
        0x48, 0x89, 0x41, 16,                 // MOV [RCX+16], RAX
        0x48, 0xbf,                            // MOVABS RDI, MAILBOX
    ]);
    code.extend_from_slice(&MAILBOX.to_le_bytes());
    code.extend_from_slice(&[
        0xc6, 0x47, 1, 1,                     // signal that the guest loop started
        0x80, 0x3f, 0, 0x74, 0xfb,           // wait for host's resume byte
        0x66, 0xba, 0xed, 0, 0xb0, 99, 0xee, // resumed: OUT byte 99 to 0xed
        0xf4,                                 // never used as a pass condition
    ]);
    memory.write(GuestAddress(0x10000), &code).expect("code");
    vm.map_memory(&memory).expect("map RAM");
    let mut cpu = vm.create_vcpu(0).expect("vCPU");
    cpu.save_state().expect("PC CPU state is snapshot-capable");
    cpu.set_boot_regs(&BootRegs::X86_64 { rip: 0x10000, rsi: 0, cr3: 0x1000, gdt: 0x5000 }).expect("Linux64 entry");
    vm.finalize().expect("finalize");

    assert_eq!(cpu.run().expect("PIO write"), VcpuExit::PioOut { port: 0xe9, len: 1, data: [42, 0, 0, 0] });
    assert_eq!(cpu.run().expect("PIO read"), VcpuExit::PioIn { port: 0xea, len: 1 });
    assert!(cpu.run().is_err(), "uncompleted PIO must not run");
    assert!(cpu.complete_pio_in(&[7, 8]).is_err(), "wrong completion length must fail");
    assert!(cpu.complete_mmio_read(&[7]).is_err(), "wrong completion kind must fail");
    cpu.complete_pio_in(&[7]).expect("complete PIO");
    assert_eq!(cpu.run().expect("PIO read-back"), VcpuExit::PioOut { port: 0xeb, len: 1, data: [7, 0, 0, 0] });
    assert_eq!(cpu.run().expect("MMIO write"), VcpuExit::MmioWrite { addr: MMIO, len: 8, data: 42u64.to_le_bytes() });
    assert_eq!(cpu.run().expect("MMIO read"), VcpuExit::MmioRead { addr: MMIO + 8, len: 8 });
    assert!(cpu.run().is_err(), "uncompleted MMIO must not run");
    assert!(cpu.complete_mmio_read(&[7]).is_err(), "short MMIO completion must fail");
    let value = 0x1122_3344_5566_7788u64.to_le_bytes();
    cpu.complete_mmio_read(&value).expect("complete MMIO");
    assert_eq!(cpu.run().expect("MMIO read-back"), VcpuExit::MmioWrite { addr: MMIO + 16, len: 8, data: value });

    let kicker = cpu.kicker();
    let mailbox = memory.clone();
    let controller = std::thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            if mailbox.read_obj::<u8>(GuestAddress(MAILBOX + 1)).expect("guest started") == 1 {
                kicker.kick();
                return true;
            }
            if Instant::now() >= until {
                kicker.kick();
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    assert_eq!(cpu.run().expect("controller kick"), VcpuExit::Kicked);
    assert!(controller.join().expect("controller"), "guest never entered its running loop");
    memory.write_obj(GuestAddress(MAILBOX), &1u8).expect("resume mailbox");
    assert_eq!(cpu.run().expect("resume"), VcpuExit::PioOut { port: 0xed, len: 1, data: [99, 0, 0, 0] });
    let retained = cpu.kicker();
    drop(cpu);
    retained.kick(); // a signal after drop must not touch an unmapped kvm_run
    done.send(()).expect("watchdog");
    watchdog.join().expect("watchdog join");
    println!("native x86_64 smoke OK");
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() {
    eprintln!("native x86_64 smoke requires Linux x86_64; no guest was run");
    std::process::exit(2);
}

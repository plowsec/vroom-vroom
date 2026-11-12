#![cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::sync::Arc;
use vroom_hv::{BootRegs, Hypervisor, IrqChipConfig, VcpuExit, VmParams};
use vroom_memory::{GuestAddress, GuestMemory, MmapRegion};

#[test]
#[ignore = "requires /dev/kvm; run cargo test -p vroom-hv --test x86_64 -- --ignored"]
fn reset_vector_ports_and_snapshot() {
    let hv = vroom_hv::kvm::Kvm::new().unwrap();
    let vm = hv
        .create_vm(&VmParams {
            vcpus: 1,
            irqchip: IrqChipConfig::Ioapic,
            nested: false,
            small_pages: false,
            helper_page: 0,
        })
        .unwrap();
    let ram = GuestMemory::new(&[(GuestAddress(0), 2 << 20)]).unwrap();
    vm.map_memory(&ram).unwrap();
    let rom = Arc::new(MmapRegion::new(4096).unwrap());
    let mut bytes = vec![0xff; 4096];
    // mov dx,3f8; mov al,'X'; out dx,al; in al,dx; mov dx,1234; out dx,al; hlt
    let code = [
        0xba, 0xf8, 3, 0xb0, b'X', 0xee, 0xec, 0xba, 0x34, 0x12, 0xee, 0xf4,
    ];
    bytes[4080..4080 + code.len()].copy_from_slice(&code);
    rom.load(&bytes).unwrap();
    vm.map_readonly(0xffff_f000, rom).unwrap();
    let mut cpu = vm.create_vcpu(0).unwrap();
    vm.finalize().unwrap();
    let pristine = cpu.save_state().unwrap();
    cpu.restore_state(&pristine).unwrap();
    cpu.set_boot_regs(&BootRegs::X86Reset).unwrap();
    assert_eq!(
        cpu.run().unwrap(),
        VcpuExit::PioOut {
            port: 0x3f8,
            len: 1,
            data: [b'X', 0, 0, 0]
        }
    );
    assert_eq!(
        cpu.run().unwrap(),
        VcpuExit::PioIn {
            port: 0x3f8,
            len: 1
        }
    );
    cpu.complete_pio_in(&[0x42]).unwrap();
    assert_eq!(
        cpu.run().unwrap(),
        VcpuExit::PioOut {
            port: 0x1234,
            len: 1,
            data: [0x42, 0, 0, 0]
        }
    );
    let chip = vm.save_irqchip().unwrap();
    vm.restore_irqchip(&chip).unwrap();
    cpu.restore_state(&pristine).unwrap();
    assert_eq!(
        cpu.run().unwrap(),
        VcpuExit::PioOut {
            port: 0x3f8,
            len: 1,
            data: [b'X', 0, 0, 0]
        }
    );
}

#[test]
#[ignore = "requires /dev/kvm"]
fn hardware_breakpoint_step_register_edit_and_watchpoint() {
    use vroom_hv::debug::{DebugConfig, DebugReason, Register, Watchpoint, X86Register};
    let hv = vroom_hv::kvm::Kvm::new().unwrap();
    let vm = hv
        .create_vm(&VmParams {
            vcpus: 1,
            irqchip: IrqChipConfig::Ioapic,
            nested: false,
            small_pages: false,
            helper_page: 0,
        })
        .unwrap();
    let ram = GuestMemory::new(&[(GuestAddress(0), 2 << 20)]).unwrap();
    vm.map_memory(&ram).unwrap();
    let rom = Arc::new(MmapRegion::new(4096).unwrap());
    let mut image = vec![0; 4096];
    // nop; nop; mov ax,1234; mov [0500],ax twice; mov dx,1234; out dx,al
    let code = [
        0x90, 0x90, 0xb8, 0x34, 0x12, 0xa3, 0, 5, 0xa3, 0, 5, 0xba, 0x34, 0x12, 0xee,
    ];
    image[4080..4080 + code.len()].copy_from_slice(&code);
    rom.load(&image).unwrap();
    vm.map_readonly(0xffff_f000, rom).unwrap();
    let mut cpu = vm.create_vcpu(0).unwrap();
    vm.finalize().unwrap();
    cpu.configure_debug(&DebugConfig {
        enabled: true,
        breakpoints: vec![0xffff_fff1],
        ..Default::default()
    })
    .unwrap();
    let VcpuExit::Debug(stop) = cpu.run().unwrap() else {
        panic!("expected breakpoint")
    };
    assert_eq!(
        (stop.reason, stop.pc),
        (DebugReason::Breakpoint, 0xffff_fff1)
    );
    // RF must allow the stopped execution breakpoint's instruction to run
    // while the breakpoint remains armed. Do not emulate this by removing it.
    cpu.debug_write_register(Register::X86(X86Register::Rflags), 2 | (1 << 16))
        .unwrap();
    cpu.configure_debug(&DebugConfig {
        enabled: true,
        single_step: true,
        breakpoints: vec![0xffff_fff1],
        ..Default::default()
    })
    .unwrap();
    let VcpuExit::Debug(stop) = cpu.run().unwrap() else {
        panic!("expected step")
    };
    assert_eq!(
        (stop.reason, stop.pc),
        (DebugReason::SingleStep, 0xffff_fff2)
    );
    cpu.debug_write_register(Register::X86(X86Register::Rax), 0x42)
        .unwrap();
    let state = cpu.save_state().unwrap();
    assert_eq!(
        state
            .regs
            .iter()
            .find(|r| r.0 == X86Register::Rax as u32)
            .unwrap()
            .1,
        0x42
    );
    cpu.configure_debug(&DebugConfig {
        enabled: true,
        breakpoints: vec![0xffff_fff2],
        watchpoints: vec![Watchpoint {
            address: 0x500,
            size: 2,
            read: false,
            write: true,
        }],
        ..Default::default()
    })
    .unwrap();
    let VcpuExit::Debug(stop) = cpu.run().unwrap() else {
        panic!("expected execution tracepoint")
    };
    assert_eq!(
        (stop.reason, stop.pc),
        (DebugReason::Breakpoint, 0xffff_fff2)
    );
    // Automatic hardware tracing needs RF only, not debugger TF/single-step.
    // Retain both traps: the next exit must be the data watch, not a step.
    cpu.debug_write_register(Register::X86(X86Register::Rflags), 2 | (1 << 16))
        .unwrap();
    let VcpuExit::Debug(stop) = cpu.run().unwrap() else {
        panic!("expected data watch")
    };
    assert_eq!(stop.reason, DebugReason::Watchpoint);
    let mut data = [0; 2];
    ram.read(GuestAddress(0x500), &mut data).unwrap();
    assert_eq!(data, [0x34, 0x12]);
    // Data breakpoints trap after the access: resuming without RF or another
    // single-step must reach the next write, rather than repeat the first.
    let first_write_pc = stop.pc;
    let VcpuExit::Debug(stop) = cpu.run().unwrap() else {
        panic!("expected second data watch")
    };
    assert_eq!(stop.reason, DebugReason::Watchpoint);
    assert_eq!(stop.pc, first_write_pc + 3);
    cpu.configure_debug(&DebugConfig::default()).unwrap();
    assert!(matches!(
        cpu.run().unwrap(),
        VcpuExit::PioOut { port: 0x1234, .. }
    ));
}

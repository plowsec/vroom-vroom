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
        VcpuExit::PioOutBatch {
            port: 0x3f8,
            size: 1,
            data: vec![b'X']
        }
    );
    assert_eq!(
        cpu.run().unwrap(),
        VcpuExit::PioInBatch {
            port: 0x3f8,
            size: 1,
            count: 1
        }
    );
    cpu.complete_pio_in(&[0x42]).unwrap();
    assert_eq!(
        cpu.run().unwrap(),
        VcpuExit::PioOutBatch {
            port: 0x1234,
            size: 1,
            data: vec![0x42]
        }
    );
    let chip = vm.save_irqchip().unwrap();
    vm.restore_irqchip(&chip).unwrap();
    cpu.restore_state(&pristine).unwrap();
    assert_eq!(
        cpu.run().unwrap(),
        VcpuExit::PioOutBatch {
            port: 0x3f8,
            size: 1,
            data: vec![b'X']
        }
    );
}

#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use vroom_hv::{BootRegs, IrqChipConfig, VcpuExit, VmParams};
use vroom_memory::{GuestAddress, GuestMemory};

// Run with scripts/sign.sh on the test binary first.
#[test]
#[ignore = "requires the hypervisor entitlement"]
fn released_ram_reads_zero_and_stays_one_page_for_both() {
    const BASE: u64 = 0x4000_0000;
    const REGION: u64 = BASE + 0x10_0000;
    const SIZE: u64 = 64 << 20;
    // Each 4 KiB page of [x1, x1+x3): add its first word to x5, write x0
    // there; the sum goes just before x1.
    let code: [u32; 11] = [
        0xaa0103e2, // mov x2, x1
        0xaa0303e7, // mov x7, x3
        0xd2800005, // mov x5, #0
        0xf9400048, // 1: ldr x8, [x2]
        0x8b0800a5, // add x5, x5, x8
        0xf9000040, // str x0, [x2]
        0x91400442, // add x2, x2, #4096
        0xf14004e7, // subs x7, x7, #4096
        0x54ffff61, // b.ne 1b
        0xf81f8025, // stur x5, [x1, #-8]
        0xd4000003, // smc #0: back to the host
    ];
    let hv = vroom_hv::default_hypervisor().unwrap();
    let vm = hv
        .create_vm(&VmParams {
            vcpus: 1,
            irqchip: IrqChipConfig::GicV3 { dist_base: 0x0800_0000, redist_base: 0x080a_0000, msi: None },
            nested: false,
            small_pages: false,
            helper_page: 0x0f00_0000,
        })
        .unwrap();
    let mem = GuestMemory::new(&[(GuestAddress(BASE), 0x10_0000 + SIZE)]).unwrap();
    let bytes: Vec<u8> = code.iter().flat_map(|w| w.to_le_bytes()).collect();
    mem.write(GuestAddress(BASE), &bytes).unwrap();
    vm.map_memory(&mem).unwrap();
    let mut cpu = vm.create_vcpu(0).unwrap();
    let pristine = cpu.save_state().unwrap();
    let mut run = |value: u64| -> u64 {
        cpu.restore_state(&pristine).unwrap();
        cpu.set_boot_regs(&BootRegs::Aarch64 { pc: BASE, x: [value, REGION, 0, SIZE] }).unwrap();
        let exit = cpu.run().unwrap();
        assert!(matches!(exit, VcpuExit::Hypercall { .. }), "{exit:?}");
        mem.read_obj::<u64>(GuestAddress(REGION - 8)).unwrap()
    };
    let pages = SIZE / 4096;
    run(7);
    assert_eq!(run(7), 7 * pages, "the guest wrote every page");
    // The second half goes back to the host.
    assert!(vm.release_memory(&mem, REGION + SIZE / 2, SIZE / 2).unwrap());
    assert_eq!(mem.read_obj::<u64>(GuestAddress(REGION + SIZE / 2)).unwrap(), 0, "the host reads zeros");
    assert_eq!(mem.read_obj::<u64>(GuestAddress(REGION)).unwrap(), 7, "the rest stays");
    // The guest reads zeros there too, and what it writes, the host reads.
    assert_eq!(run(9), 7 * pages / 2);
    assert_eq!(mem.read_obj::<u64>(GuestAddress(REGION + SIZE - 4096)).unwrap(), 9);
}

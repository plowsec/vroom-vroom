#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use vroom_hv::{BootRegs, IrqChipConfig, VcpuExit, VmParams};
use vroom_memory::{GuestAddress, GuestMemory};

// Run with scripts/sign.sh on the test binary first.
#[test]
#[ignore = "requires an M3+ Mac, macOS 15+, and the hypervisor entitlement"]
fn guest_sees_nested_extensions_at_el2_and_el1() {
    const BASE: u64 = 0x4000_0000;
    let code: [u32; 17] = [
        0xd5380400, // mrs x0, ID_AA64PFR0_EL1
        0xf9000020, // str x0, [x1]
        0xd5384240, // mrs x0, CurrentEL
        0xf9000420, // str x0, [x1, #8]
        0xd2b00000, // mov x0, #0x80000000
        0xd51c1100, // msr HCR_EL2, x0
        0xd28078a0, // mov x0, #0x3c5 (EL1h, interrupts masked)
        0xd51c4000, // msr SPSR_EL2, x0
        0x10000080, // adr x0, el1
        0xd51c4020, // msr ELR_EL2, x0
        0xd5033fdf, // isb
        0xd69f03e0, // eret
        0xd5380400, // el1: mrs x0, ID_AA64PFR0_EL1
        0xf9000820, // str x0, [x1, #16]
        0xd5384240, // mrs x0, CurrentEL
        0xf9000c20, // str x0, [x1, #24]
        0xd4000003, // smc #0: return to the host
    ];
    let hv = vroom_hv::default_hypervisor().unwrap();
    for nested in [false, true] {
        let vm = hv.create_vm(&VmParams {
            vcpus: 1,
            irqchip: IrqChipConfig::GicV3 { dist_base: 0x0800_0000, redist_base: 0x080a_0000, msi: None },
            nested,
            small_pages: false,
        }).unwrap();
        let mem = GuestMemory::new(&[(GuestAddress(BASE), 0x10000)]).unwrap();
        let bytes: Vec<u8> = code.iter().flat_map(|word| word.to_le_bytes()).collect();
        mem.write(GuestAddress(BASE), &bytes).unwrap();
        vm.map_memory(&mem).unwrap();
        let mut cpu = vm.create_vcpu(0).unwrap();
        let pristine = cpu.save_state().unwrap();
        cpu.set_boot_regs(&BootRegs::Aarch64 {
            pc: BASE + if nested { 0 } else { 48 },
            x: [0, BASE + 0x1000, 0, 0],
        }).unwrap();
        let kicker = cpu.kicker();
        let (done, wait) = std::sync::mpsc::channel();
        let watchdog = std::thread::spawn(move || {
            if wait.recv_timeout(std::time::Duration::from_secs(3)).is_err() { kicker.kick(); }
        });
        let exit = cpu.run();
        let _ = done.send(());
        watchdog.join().unwrap();
        assert!(matches!(exit.unwrap(), VcpuExit::Hypercall { .. }));
        let read = |offset| u64::from_le_bytes(mem.read_obj::<[u8; 8]>(GuestAddress(BASE + 0x1000 + offset)).unwrap());
        let pfr0 = read(16);
        assert_eq!(read(24), 4, "probe must reach EL1");
        assert_eq!((pfr0 >> 8) & 0xf, u64::from(nested), "EL2 feature must match the VM config");
        assert_eq!((pfr0 >> 24) & 0xf, 1, "native GICv3 must be advertised");
        if nested {
            assert_eq!(read(8), 8, "firmware must start at EL2");
            assert_eq!(read(0), pfr0, "EL1 and EL2 must see the same features");
            // A reset restores the pristine state: the probe's EL2 setup
            // (HCR, SPSR, ELR) must not survive it.
            let el2 = |s: &vroom_hv::VcpuState| s.raw.clone();
            assert_ne!(el2(&cpu.save_state().unwrap()), el2(&pristine));
            cpu.restore_state(&pristine).unwrap();
            assert_eq!(el2(&cpu.save_state().unwrap()), el2(&pristine), "reset must restore EL2 registers");
        }
    }
}

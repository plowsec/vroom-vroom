//! Real HVF execute-patch validation, independent of any guest disk.
//! Build, scripts/sign.sh target/debug/examples/execute_smoke, then run.

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn main() {
    use vroom_hv::{debug::*, *};
    use vroom_memory::{GuestAddress, GuestMemory};
    const BRK: u16 = 0x7674;
    let hv = default_hypervisor().expect("hypervisor");
    let vm = hv
        .create_vm(&VmParams {
            vcpus: 1,
            irqchip: IrqChipConfig::GicV3 { dist_base: 0x0800_0000, redist_base: 0x080a_0000, msi: None },
            nested: false,
            small_pages: false,
        })
        .expect("vm");
    let g = vm.map_granule();
    let base = 0x4000_0000;
    let page = base + g;
    let mem = GuestMemory::new(&[(GuestAddress(base), 0x10_0000)]).expect("memory");
    let code = |at: u64, words: &[u32]| {
        for (i, word) in words.iter().enumerate() {
            mem.write_obj(GuestAddress(at + i as u64 * 4), word).expect("code");
        }
    };
    // Another page: ldp x8, x9, [x2] (a pair: maps the data view); b page.
    code(base, &[0xa9402448, 0x14000000 | ((page - base - 4) / 4) as u32]);
    let add = 0x91000463u32;
    // ldr x1, [x2]; add x3, x3, #1 (patched); ldr w6, [x7]; ldp x4, x5, [x2];
    // str x1, [x2, #8]; stp x1, x1, [x2, #16]; ldp w10, w11, [x7]; hvc #0
    code(page, &[0xf9400041, add, 0xb94000e6, 0xa9401444, 0xf9000441, 0xa9010441, 0x29402cea, 0xd4000002]);
    let data = page + 0x100;
    code(data, &[0x3333_4444, 0x1111_2222, 0x7777_8888, 0x5555_6666]);
    let nop = 0xd503201fu32;
    code(page + 0x200, &[nop]);
    vm.map_memory(&mem).expect("map");
    let mut cpu = vm.create_vcpu(0).expect("cpu");
    cpu.set_boot_regs(&BootRegs::Aarch64 { pc: base, x: [0; 4] }).expect("boot regs");
    cpu.debug_write_register(Register::X(2), data).expect("x2");
    cpu.debug_write_register(Register::X(7), page + 4).expect("x7");
    cpu.configure_debug(&DebugConfig { enabled: true, trap_brk_immediate: Some(BRK), ..Default::default() }).expect("debug");
    let brk = (0xd420_0000u32 | (u32::from(BRK) << 5)).to_le_bytes();
    vm.set_execute_patch(page + 4, Some(brk)).expect("patch");
    vm.set_execute_patch(page + 0x200, Some(brk)).expect("second patch");
    let kick = cpu.kicker();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(5));
        kick.kick();
    });
    let reg = |cpu: &mut Box<dyn Vcpu>, id: u32| cpu.save_state().expect("state").regs.iter().find(|r| r.0 == id).expect("register").1;
    let first = cpu.run().expect("run to the patch");
    assert!(
        matches!(first, VcpuExit::Debug(DebugStop { pc, reason: DebugReason::SoftwareBreakpoint, .. }) if pc == page + 4),
        "{first:?}"
    );
    assert_eq!((reg(&mut cpu, 8), reg(&mut cpu, 9)), (0x1111_2222_3333_4444, 0x5555_6666_7777_8888), "pair read of the page from another page");
    assert_eq!(reg(&mut cpu, 1), 0x1111_2222_3333_4444, "emulated read on the page");
    assert_eq!(mem.read_obj::<u32>(GuestAddress(page + 4)).expect("ram"), add, "RAM keeps the instruction");
    println!("patch: executes BRK, RAM keeps ADD; reads see RAM (emulated, data view)");
    vm.set_execute_patch(page + 4, None).expect("unpatch");
    let second = cpu.run().expect("run to the end");
    assert!(matches!(second, VcpuExit::Hypercall { .. }), "{second:?}");
    assert_eq!(reg(&mut cpu, 3), 1, "ADD ran once unpatched");
    assert_eq!(reg(&mut cpu, 6), add as u64, "emulated read of a patched page's word");
    assert_eq!((reg(&mut cpu, 4), reg(&mut cpu, 5)), (0x1111_2222_3333_4444, 0x5555_6666_7777_8888), "native step: pair read");
    let x1 = 0x1111_2222_3333_4444u64;
    for (offset, want) in [(8, x1), (16, x1), (24, x1)] {
        assert_eq!(mem.read_obj::<u64>(GuestAddress(data + offset)).expect("ram"), want, "write at +{offset}");
    }
    assert_eq!((reg(&mut cpu, 10), reg(&mut cpu, 11)), (add as u64, 0xb94000e6), "native step reads RAM under the patch");
    println!("emulated write; native steps for pairs on the page itself");
    assert_eq!(vm.refresh_execute_patches().expect("refresh"), Vec::<u64>::new());
    mem.write_obj(GuestAddress(page + 0x200), &0u32).expect("dma");
    assert_eq!(vm.refresh_execute_patches().expect("refresh"), vec![page + 0x200], "RAM changed under the patch");
    assert_eq!(vm.refresh_execute_patches().expect("refresh"), Vec::<u64>::new());
    println!("refresh: a patch whose RAM word changed is dropped and reported once");
    // The page is unpatched now: plain RAM again, every permission.
    code(page, &[0xd4000002]);
    cpu.debug_write_register(Register::Pc, page).expect("pc");
    assert!(matches!(cpu.run().expect("unpatched page"), VcpuExit::Hypercall { .. }));
    println!("unpatched page runs from RAM");
    // A debugger stepping through a patched page: one instruction each.
    code(page, &[0xf9400041, add, 0xb94000e6, 0xa9401444, 0xf9000441, 0xa9010441, 0x29402cea, 0xd4000002]);
    code(page + 0x200, &[nop]);
    vm.set_execute_patch(page + 0x200, Some(brk)).expect("patch again");
    cpu.debug_write_register(Register::Pc, page).expect("pc");
    for i in 1..=7 {
        cpu.configure_debug(&DebugConfig { enabled: true, single_step: true, trap_brk_immediate: Some(BRK), ..Default::default() }).expect("step");
        let event = cpu.run().expect("step");
        assert!(
            matches!(event, VcpuExit::Debug(DebugStop { pc, reason: DebugReason::SingleStep, .. }) if pc == page + i * 4),
            "step {i}: {event:?}"
        );
    }
    assert_eq!(reg(&mut cpu, 3), 2, "ADD ran once more");
    println!("single-step: emulated, native and plain instructions on a patched page each retire once");
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn main() {}

//! A register restore must not leave the guest on translations it cached
//! from page tables the host has since rewritten (an in-place snapshot
//! restore). The guest maps one page and reads it; the host points the
//! mapping elsewhere in RAM and restores the saved registers; the guest
//! must read the new page. Without the restore it reads the old one: the
//! check is live. macOS: needs the hypervisor entitlement (scripts/sign.sh).

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn main() {
    use vroom_hv::*;
    use vroom_memory::{GuestAddress, GuestMemory};

    const RAM: u64 = 0x4000_0000;
    const L3: u64 = RAM + 0x3000;
    const PAGES: [u64; 2] = [RAM + 0x10000, RAM + 0x20000];

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
    let mem = GuestMemory::new(&[(GuestAddress(RAM), 0x10_0000)]).expect("mem");
    // mov x0, #0x8000_0000; ldr x1, [x0]; hvc #0
    for (i, w) in [0xd2b00000u32, 0xf9400001, 0xd4000002].iter().enumerate() {
        mem.write_obj(GuestAddress(RAM + 4 * i as u64), w).expect("code");
    }
    // 4 KiB granule, 39-bit VA: 1 GiB identity block over RAM, VA -> L2 -> L3.
    mem.write_obj(GuestAddress(RAM + 0x1000 + 8), &(RAM | 0x701)).expect("l1");
    mem.write_obj(GuestAddress(RAM + 0x1000 + 16), &((RAM + 0x2000) | 3)).expect("l1");
    mem.write_obj(GuestAddress(RAM + 0x2000), &(L3 | 3)).expect("l2");
    for (i, &page) in PAGES.iter().enumerate() {
        mem.write_obj(GuestAddress(page), &(0xa0 + i as u64)).expect("data");
    }
    let map = |page: u64| mem.write_obj(GuestAddress(L3), &(page | 0x703)).expect("pte");
    vm.map_memory(&mem).expect("map");

    let mut cpu = vm.create_vcpu(0).expect("vcpu");
    let mut state = cpu.save_state().expect("save");
    let tcr = 25 | (1 << 8) | (1 << 10) | (3 << 12) | (1 << 23) | (2 << 32);
    let set = [(31, RAM), (34, 0x3c5), (0x1_c510, 0xff), (0x1_c102, tcr), (0x1_c100, RAM + 0x1000), (0x1_c080, 0x30d0_1805)];
    for (id, v) in set {
        state.regs.iter_mut().find(|(r, _)| *r == id).expect("register in state").1 = v;
    }
    let read = |cpu: &mut Box<dyn Vcpu>| match cpu.run().expect("run") {
        VcpuExit::Hypercall { args, .. } => args[0],
        e => panic!("unexpected exit {e:?}"),
    };

    map(PAGES[0]);
    cpu.restore_state(&state).expect("restore");
    assert_eq!(read(&mut cpu), 0xa0);
    map(PAGES[1]);
    cpu.set_boot_regs(&BootRegs::Aarch64 { pc: RAM, x: [0; 4] }).expect("regs");
    let stale = read(&mut cpu) == 0xa0;
    let mut wrong = 0;
    for i in 0..1000u64 {
        map(PAGES[(i % 2) as usize]);
        cpu.restore_state(&state).expect("restore");
        if read(&mut cpu) != 0xa0 + i % 2 {
            wrong += 1;
        }
    }
    println!("without a restore: {}", if stale { "old translation (TLB kept it)" } else { "new translation" });
    println!("after a restore: {wrong}/1000 reads through an old translation");
    assert_eq!(wrong, 0);
    println!("tlb_restore OK");
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn main() {}

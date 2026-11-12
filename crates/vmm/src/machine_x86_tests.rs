//! Real VMM lifecycle/PIO with a scripted hypervisor; no guest instructions run.
use super::*;
use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use std::sync::Condvar;
use vroom_config::{Boot, Disk, GuestOs, Serial};
use vroom_hv::{Arch, IrqChipConfig, IrqChipInfo, Vcpu, VcpuExit, VcpuKicker, VcpuState, VmParams};
use vroom_memory::GuestAddress;

#[derive(Debug, Default)]
struct Probe {
    creates: AtomicU64, runs: AtomicU64, state_calls: AtomicU64, drops: AtomicU64,
    boot: Mutex<Option<BootRegs>>, memory: Mutex<Option<GuestMemory>>,
    exits: Mutex<VecDeque<VcpuExit>>, completions: Mutex<Vec<Vec<u8>>>,
    irqs: Mutex<Vec<(u32, bool)>>,
    fail_irq_low: AtomicBool, fail_boot: AtomicBool, fail_finalize: AtomicBool,
}
#[derive(Debug, Default)]
struct Kick(Mutex<bool>, Condvar);
impl VcpuKicker for Kick {
    fn kick(&self) { *self.0.lock().unwrap() = true; self.1.notify_all(); }
    fn wake(&self) { self.kick(); }
}
#[derive(Debug)]
struct Cpu(Arc<Probe>, Arc<Kick>);
impl Drop for Cpu { fn drop(&mut self) { self.0.drops.fetch_add(1, SeqCst); } }
impl Vcpu for Cpu {
    fn index(&self) -> u32 { 0 }
    fn arch_id(&self) -> u64 { 0 }
    fn kicker(&self) -> Arc<dyn VcpuKicker> { self.1.clone() }
    fn run(&mut self) -> vroom_hv::Result<VcpuExit> {
        self.0.runs.fetch_add(1, SeqCst);
        if let Some(exit) = self.0.exits.lock().unwrap().pop_front() { return Ok(exit); }
        let signalled = self.1.0.lock().unwrap();
        let (mut signalled, _) = self.1.1.wait_timeout_while(signalled, Duration::from_secs(1), |v| !*v).unwrap();
        *signalled = false;
        Ok(VcpuExit::Kicked)
    }
    fn set_boot_regs(&mut self, regs: &BootRegs) -> vroom_hv::Result<()> {
        *self.0.boot.lock().unwrap() = Some(regs.clone());
        if self.0.fail_boot.load(SeqCst) { return Err(vroom_hv::Error::Invalid("injected initialization failure".into())); }
        Ok(())
    }
    fn save_state(&mut self) -> vroom_hv::Result<VcpuState> {
        self.0.state_calls.fetch_add(1, SeqCst); Err(vroom_hv::Error::Unsupported("test save"))
    }
    fn restore_state(&mut self, _: &VcpuState) -> vroom_hv::Result<()> {
        self.0.state_calls.fetch_add(1, SeqCst); Err(vroom_hv::Error::Unsupported("test restore"))
    }
    fn complete_pio_in(&mut self, bytes: &[u8]) -> vroom_hv::Result<()> {
        self.0.completions.lock().unwrap().push(bytes.to_vec()); Ok(())
    }
    fn complete_mmio_read(&mut self, _: &[u8]) -> vroom_hv::Result<()> { panic!("unexpected MMIO completion") }
    fn complete_hypercall(&mut self, _: [u64; 4]) -> vroom_hv::Result<()> { panic!("unexpected hypercall") }
}
#[derive(Debug)]
struct TestVm(Arc<Probe>);
impl Vm for TestVm {
    fn map_memory(&self, memory: &GuestMemory) -> vroom_hv::Result<()> { *self.0.memory.lock().unwrap() = Some(memory.clone()); Ok(()) }
    fn map_readonly(&self, _: u64, _: Arc<vroom_memory::MmapRegion>) -> vroom_hv::Result<()> { panic!("no firmware") }
    fn create_vcpu(&self, index: u32) -> vroom_hv::Result<Box<dyn Vcpu>> {
        assert_eq!(index, 0); Ok(Box::new(Cpu(self.0.clone(), Arc::new(Kick::default()))))
    }
    fn finalize(&self) -> vroom_hv::Result<()> {
        if self.0.fail_finalize.load(SeqCst) { return Err(vroom_hv::Error::Invalid("injected finalize failure".into())); } Ok(())
    }
    fn set_irq_line(&self, irq: u32, level: bool) -> vroom_hv::Result<()> {
        self.0.irqs.lock().unwrap().push((irq, level));
        if !level && self.0.fail_irq_low.load(SeqCst) { return Err(vroom_hv::Error::Invalid("injected IRQ failure".into())); } Ok(())
    }
    fn signal_msi(&self, _: u64, _: u32) -> vroom_hv::Result<()> { panic!("no MSI") }
    fn clock(&self) -> u64 { 0 }
    fn set_clock(&self, _: u64) {}
    fn save_irqchip(&self) -> vroom_hv::Result<IrqChipState> { self.0.state_calls.fetch_add(1, SeqCst); panic!("cold-only irqchip save") }
    fn restore_irqchip(&self, _: &IrqChipState) -> vroom_hv::Result<()> { self.0.state_calls.fetch_add(1, SeqCst); panic!("cold-only irqchip restore") }
}
#[derive(Debug)]
struct TestHv(Arc<Probe>);
impl Hypervisor for TestHv {
    fn name(&self) -> &'static str { "kvm" } // test implementation, never native acceptance
    fn arch(&self) -> Arch { Arch::X86_64 }
    fn irqchip_info(&self) -> vroom_hv::Result<IrqChipInfo> { panic!("x64 must not request a GIC") }
    fn create_vm(&self, p: &VmParams) -> vroom_hv::Result<Arc<dyn Vm>> {
        assert_eq!(p.vcpus, 1); assert!(matches!(p.irqchip, IrqChipConfig::Ioapic));
        assert!(!p.nested && !p.small_pages); assert_eq!(p.helper_page, 0);
        self.0.creates.fetch_add(1, SeqCst); Ok(Arc::new(TestVm(self.0.clone())))
    }
}
fn image() -> Vec<u8> {
    let mut b = vec![0; 5 * 512 + 0x4000]; b[0x1f1] = 4;
    for (o, v) in [(0x1f4, 0x4000u32 / 16), (0x22c, 0x7fff_ffff), (0x230, 2 << 20), (0x238, 2047), (0x248, 0x300), (0x24c, 0x1000), (0x260, 2 << 20)] { b[o..o + 4].copy_from_slice(&v.to_le_bytes()); }
    b[0x1fe..0x200].copy_from_slice(&0xaa55u16.to_le_bytes()); b[0x200] = 0xeb; b[0x201] = 0x66;
    b[0x202..0x206].copy_from_slice(b"HdrS"); b[0x206..0x208].copy_from_slice(&0x020cu16.to_le_bytes());
    b[0x211] = 1; b[0x234] = 1; b[0x235] = 21; b[0x236] = 1;
    b[0x258..0x260].copy_from_slice(&(16u64 << 20).to_le_bytes()); b
}
fn fixture(root: &Path) -> (VmConfig, VmmOptions, Arc<TestHv>, Arc<Probe>) {
    let kernel = root.join("kernel"); let initrd = root.join("initrd");
    std::fs::write(&kernel, image()).unwrap(); std::fs::write(&initrd, b"test fixture, not an executable initramfs").unwrap();
    let mut c = VmConfig::from_toml("name='x64-reference'\ncpus=1\nmemory_mib=512\nballoon=false\n[clipboard]\nhost_to_guest=false\nguest_to_host=false\n[boot]\ntype='kernel'\nkernel='unused'\n").unwrap();
    c.boot = Boot::Kernel { kernel, initrd: Some(initrd), cmdline: machine_x86::CMDLINE.into() };
    let o = VmmOptions { serial_out: Box::new(io::sink()), start: StartMode::BootPaused, clipboard: None, ui: None, state_dir: root.join("state"), sandbox: true };
    let p = Arc::new(Probe::default()); (c, o, Arc::new(TestHv(p.clone())), p)
}
#[test]
fn strict_profile_rejections_precede_state_and_vm_creation() {
    let temp = tempfile::tempdir().unwrap();
    let variants: Vec<Box<dyn Fn(&mut VmConfig)>> = vec![
        Box::new(|c| c.cpus = 2), Box::new(|c| c.memory_mib = 1024), Box::new(|c| c.nested = true),
        Box::new(|c| c.balloon = true), Box::new(|c| c.tpm = true), Box::new(|c| c.camera = true),
        Box::new(|c| c.nets.push(serde_json::from_value(serde_json::json!({"mode": "nat"})).unwrap())),
        Box::new(|c| c.audio.output = true), Box::new(|c| c.clipboard.host_to_guest = true),
        Box::new(|c| c.display.gpu = vroom_config::Gpu::Virtio2d), Box::new(|c| c.identity.product = "custom".into()),
        Box::new(|c| c.virtio_transport = vroom_config::Transport::Pci), Box::new(|c| c.os = Some(GuestOs::Windows)),
        Box::new(|c| c.serial = Serial::File { path: "must-not-create".into() }),
        Box::new(|c| c.disks.push(Disk { path: "must-not-open".into(), readonly: false, format: Default::default(), cdrom: false, bus: Default::default(), sync: Default::default(), backing: None })),
        Box::new(|c| c.shares.push(vroom_config::Share { path: "must-not-open".into(), tag: "share".into(), readonly: true })),
        Box::new(|c| c.usb.push(vroom_config::Usb { id: "1234:5678".into(), serial: None, name: None, mode: Default::default(), readonly: true })),
        Box::new(|c| c.boot = Boot::Uefi { firmware: None, vars: "must-not-create".into(), secure_boot: true }),
        Box::new(|c| if let Boot::Kernel { cmdline, .. } = &mut c.boot { cmdline.push_str(" extra"); }),
        Box::new(|c| if let Boot::Kernel { initrd, .. } = &mut c.boot { *initrd = None; }),
    ];
    for mutate in variants {
        let (mut c, o, hv, p) = fixture(temp.path()); mutate(&mut c);
        assert!(Vmm::start(c, hv, o).is_err()); assert!(!temp.path().join("state").exists()); assert_eq!(p.creates.load(SeqCst), 0);
    }
    for start in [StartMode::Restore("absent".into()), StartMode::RestorePaused("absent".into())] {
        let (c, mut o, hv, p) = fixture(temp.path()); o.start = start;
        assert!(matches!(Vmm::start(c, hv, o), Err(Error::UnsupportedLifecycle("restore"))));
        assert!(!temp.path().join("state").exists()); assert_eq!(p.creates.load(SeqCst), 0);
    }
    let (c, mut o, hv, p) = fixture(temp.path());
    let (ui, _peer) = std::os::unix::net::UnixStream::pair().unwrap(); o.ui = Some(ui);
    assert!(Vmm::start(c, hv, o).is_err()); assert!(!temp.path().join("state").exists()); assert_eq!(p.creates.load(SeqCst), 0);
}
#[test]
fn invalid_or_unbounded_boot_input_fails_before_state_creation() {
    let temp = tempfile::tempdir().unwrap();
    for mode in 0..4 {
        let (c, o, hv, p) = fixture(temp.path()); let k = temp.path().join("kernel");
        match mode {
            0 => std::fs::write(&k, b"not a bzImage").unwrap(),
            1 => std::fs::File::create(&k).unwrap().set_len((128 << 20) + 1).unwrap(),
            2 => { std::fs::remove_file(&k).unwrap(); std::os::unix::fs::symlink(temp.path().join("initrd"), &k).unwrap(); }
            _ => { std::fs::remove_file(&k).unwrap(); std::fs::create_dir(&k).unwrap(); }
        }
        assert!(Vmm::start(c, hv, o).is_err()); assert!(!temp.path().join("state").exists()); assert_eq!(p.creates.load(SeqCst), 0);
        if mode == 3 { std::fs::remove_dir(&k).unwrap(); } else { std::fs::remove_file(&k).unwrap(); }
    }
}
#[test]
fn public_vmm_boot_paused_uses_real_worker_counter_and_cold_only_controls() {
    let temp = tempfile::tempdir().unwrap(); let (c, o, hv, p) = fixture(temp.path());
    let mut vm = Vmm::start(c, hv, o).unwrap(); assert_eq!(vm.state(), RunState::Paused);
    assert_eq!(vm.vcpu_run_calls(), 0); assert_eq!(p.runs.load(SeqCst), 0);
    assert!(matches!(*p.boot.lock().unwrap(), Some(BootRegs::X86_64 { rip: 0x1000200, rsi: 0x15000, cr3: 0x10000, gdt: 0x14000 })));
    let memory = p.memory.lock().unwrap(); let memory = memory.as_ref().unwrap();
    let mut regions = memory.regions(); let ram = regions.next().unwrap();
    assert_eq!(ram.start(), GuestAddress(0)); assert_eq!(ram.len(), 512 << 20); assert!(regions.next().is_none());
    assert_eq!(memory.read_obj::<u32>(GuestAddress(0x15202)).unwrap(), u32::from_le_bytes(*b"HdrS"));
    assert!(vm.serial_input(b"ignored").is_err()); assert_eq!(vm.agent().features(), 0); assert!(!vm.agent().status().connected);
    vm.resume().unwrap(); let deadline = Instant::now() + Duration::from_secs(1);
    while p.runs.load(SeqCst) == 0 { assert!(Instant::now() < deadline); std::thread::yield_now(); }
    vm.pause().unwrap(); let paused = vm.vcpu_run_calls(); assert_eq!(paused, p.runs.load(SeqCst));
    std::thread::sleep(Duration::from_millis(30)); assert_eq!(vm.vcpu_run_calls(), paused);
    assert!(matches!(vm.reset(), Err(Error::UnsupportedLifecycle("reset"))));
    assert!(vm.snapshot(&temp.path().join("snapshot"), "unsupported").is_err()); assert!(vm.suspend(&temp.path().join("suspend")).is_err());
    assert!(!temp.path().join("snapshot").exists()); assert!(!temp.path().join("suspend").exists()); assert_eq!(p.state_calls.load(SeqCst), 0);
    vm.shutdown_checked().unwrap(); assert_eq!(vm.state(), RunState::Stopped); assert_eq!(p.drops.load(SeqCst), 1);
}
#[test]
fn initialization_and_finalization_failures_stop_without_run_or_saved_state() {
    for finalize in [false, true] {
        let temp = tempfile::tempdir().unwrap(); let (c, o, hv, p) = fixture(temp.path());
        if finalize { p.fail_finalize.store(true, SeqCst); } else { p.fail_boot.store(true, SeqCst); }
        assert!(Vmm::start(c, hv, o).is_err()); assert_eq!(p.runs.load(SeqCst), 0);
        assert_eq!(p.state_calls.load(SeqCst), 0); assert_eq!(p.drops.load(SeqCst), 1);
    }
}
fn out(port: u16, byte: u8) -> VcpuExit { VcpuExit::PioOut { port, len: 1, data: [byte, 0, 0, 0] } }
#[test]
fn irq_failure_during_pio_read_stops_before_completion_or_another_run() {
    let temp = tempfile::tempdir().unwrap(); let (c, o, hv, p) = fixture(temp.path());
    p.fail_irq_low.store(true, SeqCst);
    p.exits.lock().unwrap().extend([out(0x3fc, 8), out(0x3f9, 2), VcpuExit::PioIn { port: 0x3fa, len: 1 }]);
    let mut vm = Vmm::start(c, hv, o).unwrap(); vm.resume().unwrap();
    assert!(matches!(vm.next_event(Duration::from_secs(1)), Some(VmEvent::Crashed(why)) if why.contains("IRQ4 failed")));
    vm.pause().unwrap(); assert_eq!(vm.vcpu_run_calls(), 3); assert_eq!(p.runs.load(SeqCst), 3);
    assert!(p.completions.lock().unwrap().is_empty()); assert_eq!(*p.irqs.lock().unwrap(), vec![(4, true), (4, false)]); vm.shutdown();
}
#[test]
fn serial_output_failure_is_not_silently_discarded() {
    struct Broken;
    impl io::Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> { Err(io::Error::other("injected output failure")) }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }
    let temp = tempfile::tempdir().unwrap(); let (c, mut o, hv, p) = fixture(temp.path()); o.serial_out = Box::new(Broken);
    p.exits.lock().unwrap().push_back(out(0x3f8, b'x'));
    let mut vm = Vmm::start(c, hv, o).unwrap(); vm.resume().unwrap();
    assert!(matches!(vm.next_event(Duration::from_secs(1)), Some(VmEvent::Crashed(why)) if why.contains("COM1 output failed")));
    vm.pause().unwrap(); assert_eq!(vm.vcpu_run_calls(), 1); vm.shutdown();
}


#[test]
fn unmapped_pc_mmio_fails_without_completion_or_another_run() {
    for exit in [VcpuExit::MmioRead { addr: 0xf000_0000, len: 4 },
        VcpuExit::MmioWrite { addr: 0xf000_0000, len: 4, data: [0; 8] }] {
        let temp = tempfile::tempdir().unwrap();
        let (config, opts, hv, counts) = fixture(temp.path());
        counts.exits.lock().unwrap().push_back(exit);
        let mut vm = Vmm::start(config, hv, opts).unwrap();
        vm.resume().unwrap();
        assert!(matches!(vm.next_event(Duration::from_secs(1)), Some(VmEvent::Crashed(why)) if why.contains("unmapped PC MMIO")));
        vm.pause().unwrap();
        assert_eq!(vm.vcpu_run_calls(), 1);
        vm.shutdown_checked().unwrap();
    }
}

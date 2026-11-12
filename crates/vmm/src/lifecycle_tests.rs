//! Exercises the real worker and VMM lifecycle with a counting fake hypervisor.
use super::*;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
    Condvar,
};
use vroom_hv::{Arch, IrqChipInfo, Vcpu, VcpuExit, VcpuKicker, VcpuState, VmParams};

#[derive(Debug, Default)]
struct Counts {
    runs: AtomicUsize,
    boots: AtomicUsize,
    saves: AtomicUsize,
    restores: AtomicUsize,
    irq_saves: AtomicUsize,
    irq_restores: AtomicUsize,
    drops: AtomicUsize,
    zeroes: AtomicUsize,
    fail_boot: AtomicBool,
    panic_boot_cpu: AtomicBool,
    fail_create: AtomicBool,
    panic_create: AtomicBool,
    fail_save: AtomicBool,
    panic_cpu_drop: AtomicBool,
    vm_drops: AtomicUsize,
    startup_drop_gate: Mutex<Option<StartupDropGate>>,
    drop_wait_timed_out: AtomicBool,
    boot_regs: Mutex<Vec<BootRegs>>,
    timeline: Mutex<Vec<String>>,
}

#[derive(Debug)]
struct StartupDropGate {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

impl Counts {
    fn wait_at_startup_drop(&self) {
        let gate = self.startup_drop_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            let _ = gate.entered.send(());
            if gate.release.recv_timeout(Duration::from_secs(2)).is_err() {
                // Do not panic from a destructor while another panic unwinds.
                self.drop_wait_timed_out.store(true, SeqCst);
            }
        }
    }
}

#[derive(Debug, Default)]
struct Kick(Mutex<bool>, Condvar);
impl VcpuKicker for Kick {
    fn kick(&self) {
        *self.0.lock().unwrap() = true;
        self.1.notify_all();
    }
    fn wake(&self) {
        self.kick();
    }
}

#[derive(Debug)]
struct FakeCpu {
    index: u32,
    counts: Arc<Counts>,
    kicker: Arc<Kick>,
    kernel_psci: bool,
}
impl Drop for FakeCpu {
    fn drop(&mut self) {
        self.counts.wait_at_startup_drop();
        self.counts.drops.fetch_add(1, SeqCst);
        if self.counts.panic_cpu_drop.load(SeqCst) {
            panic!("injected startup CPU drop panic");
        }
    }
}
impl Vcpu for FakeCpu {
    fn index(&self) -> u32 {
        self.index
    }
    fn arch_id(&self) -> u64 {
        self.index as u64
    }
    fn kicker(&self) -> Arc<dyn VcpuKicker> {
        self.kicker.clone()
    }
    fn in_kernel_psci(&self) -> bool {
        self.counts
            .timeline
            .lock()
            .unwrap()
            .push(format!("secondary-init {}", self.index));
        self.kernel_psci
    }
    fn run(&mut self) -> vroom_hv::Result<VcpuExit> {
        if self.index == 0 && self.counts.panic_boot_cpu.load(SeqCst) {
            panic!("injected worker panic");
        }
        self.counts
            .timeline
            .lock()
            .unwrap()
            .push(format!("run {}", self.index));
        self.counts.runs.fetch_add(1, SeqCst);
        let signalled = self.kicker.0.lock().unwrap();
        let (mut signalled, _) = self
            .kicker
            .1
            .wait_timeout_while(signalled, Duration::from_secs(2), |v| !*v)
            .unwrap();
        *signalled = false;
        Ok(VcpuExit::Kicked)
    }
    fn set_boot_regs(&mut self, regs: &BootRegs) -> vroom_hv::Result<()> {
        self.counts.boots.fetch_add(1, SeqCst);
        self.counts.boot_regs.lock().unwrap().push(regs.clone());
        if self.counts.fail_boot.load(SeqCst) {
            return Err(vroom_hv::Error::Invalid("injected boot failure".into()));
        }
        self.counts
            .timeline
            .lock()
            .unwrap()
            .push(format!("boot-init {}", self.index));
        Ok(())
    }
    fn save_state(&mut self) -> vroom_hv::Result<VcpuState> {
        self.counts.saves.fetch_add(1, SeqCst);
        if self.counts.fail_save.load(SeqCst) {
            return Err(vroom_hv::Error::Invalid("injected pristine save failure".into()));
        }
        Ok(VcpuState {
            index: self.index,
            ..Default::default()
        })
    }
    fn restore_state(&mut self, _: &VcpuState) -> vroom_hv::Result<()> {
        self.counts.restores.fetch_add(1, SeqCst);
        Ok(())
    }
    fn complete_mmio_read(&mut self, _: &[u8]) -> vroom_hv::Result<()> {
        Ok(())
    }
    fn complete_pio_in(&mut self, _: &[u8]) -> vroom_hv::Result<()> {
        Ok(())
    }
    fn complete_hypercall(&mut self, _: [u64; 4]) -> vroom_hv::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct FakeVm {
    counts: Arc<Counts>,
    memory: Mutex<Option<GuestMemory>>,
    kernel_psci: bool,
}
impl Drop for FakeVm {
    fn drop(&mut self) {
        self.counts.wait_at_startup_drop();
        self.counts.vm_drops.fetch_add(1, SeqCst);
    }
}
impl Vm for FakeVm {
    fn map_memory(&self, memory: &GuestMemory) -> vroom_hv::Result<()> {
        *self.memory.lock().unwrap() = Some(memory.clone());
        Ok(())
    }
    fn map_readonly(&self, _: u64, _: Arc<vroom_memory::MmapRegion>) -> vroom_hv::Result<()> {
        Ok(())
    }
    fn create_vcpu(&self, index: u32) -> vroom_hv::Result<Box<dyn Vcpu>> {
        if self.counts.panic_create.load(SeqCst) {
            panic!("injected startup creation panic");
        }
        if self.counts.fail_create.load(SeqCst) {
            return Err(vroom_hv::Error::Invalid("injected CPU creation failure".into()));
        }
        Ok(Box::new(FakeCpu {
            index,
            counts: self.counts.clone(),
            kicker: Arc::new(Kick::default()),
            kernel_psci: self.kernel_psci,
        }))
    }
    fn zero_memory(&self, memory: &GuestMemory) -> vroom_hv::Result<()> {
        self.counts.zeroes.fetch_add(1, SeqCst);
        memory
            .discard_all()
            .map_err(|e| vroom_hv::Error::Invalid(e.to_string()))
    }
    fn set_irq_line(&self, _: u32, _: bool) -> vroom_hv::Result<()> {
        Ok(())
    }
    fn signal_msi(&self, _: u64, _: u32) -> vroom_hv::Result<()> {
        Ok(())
    }
    fn clock(&self) -> u64 {
        0
    }
    fn set_clock(&self, _: u64) {}
    fn save_irqchip(&self) -> vroom_hv::Result<IrqChipState> {
        self.counts.irq_saves.fetch_add(1, SeqCst);
        Ok(IrqChipState::default())
    }
    fn restore_irqchip(&self, _: &IrqChipState) -> vroom_hv::Result<()> {
        self.counts.irq_restores.fetch_add(1, SeqCst);
        Ok(())
    }
}

#[derive(Debug)]
struct FakeHv {
    vm: Arc<FakeVm>,
    arch: Arch,
}
impl Hypervisor for FakeHv {
    fn name(&self) -> &'static str {
        "lifecycle-test"
    }
    fn arch(&self) -> Arch {
        self.arch
    }
    fn irqchip_info(&self) -> vroom_hv::Result<IrqChipInfo> {
        Ok(IrqChipInfo {
            dist_size: 0x10000,
            redist_size_per_cpu: 0x20000,
            msi_size: 0x10000,
            spi_base: 32,
            spi_count: 960,
            msi_frame_in_kernel: false,
        })
    }
    fn create_vm(&self, _: &VmParams) -> vroom_hv::Result<Arc<dyn Vm>> {
        Ok(self.vm.clone())
    }
}

fn fixture(
    root: &Path,
    cpus: u32,
    start: StartMode,
) -> (VmConfig, VmmOptions, Arc<FakeHv>, Arc<Counts>) {
    let path = root.join("Image");
    let mut image = vec![0; 4096];
    image[16..24].copy_from_slice(&(2u64 << 20).to_le_bytes());
    image[56..60].copy_from_slice(&0x644d_5241u32.to_le_bytes());
    std::fs::write(&path, image).unwrap();
    let mut config = VmConfig::from_toml("name='lifecycle-test'\ncpus=1\nmemory_mib=64\nballoon=false\n[boot]\ntype='kernel'\nkernel='unused'\n").unwrap();
    config.cpus = cpus;
    config.boot = vroom_config::Boot::Kernel {
        kernel: path,
        initrd: None,
        cmdline: String::new(),
    };
    let opts = VmmOptions {
        serial_out: Box::new(std::io::sink()),
        start,
        clipboard: None,
        ui: None,
        state_dir: root.join("state"),
        sandbox: false,
    };
    let counts = Arc::new(Counts::default());
    let hv = Arc::new(FakeHv {
        vm: Arc::new(FakeVm {
            counts: counts.clone(),
            memory: Mutex::new(None),
            kernel_psci: true,
        }),
        arch: Arch::Aarch64,
    });
    (config, opts, hv, counts)
}

fn wait_for_runs(counts: &Counts, after: usize) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while counts.runs.load(SeqCst) <= after {
        assert!(
            Instant::now() < deadline,
            "worker did not execute after resume"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn no_state_calls(counts: &Counts) {
    assert_eq!(counts.saves.load(SeqCst), 0);
    assert_eq!(counts.restores.load(SeqCst), 0);
    assert_eq!(counts.irq_saves.load(SeqCst), 0);
    assert_eq!(counts.irq_restores.load(SeqCst), 0);
}

#[test]
fn cold_boot_paused_acknowledges_once_without_execution_or_saved_state() {
    let temp = tempfile::tempdir().unwrap();
    let (config, opts, hv, counts) = fixture(temp.path(), 2, StartMode::BootPaused);
    let mut vm =
        Vmm::start_with_lifecycle(config, hv, opts, LifecyclePolicy::ColdBootOnly).unwrap();
    assert_eq!(vm.state(), RunState::Paused);
    assert!(vm.pristine_devices.is_empty());
    assert_eq!(counts.boots.load(SeqCst), 1);
    // A command round trip proves the worker has processed initialization,
    // without relying on a scheduling delay to establish quiescence.
    let (tx, rx) = mpsc::channel();
    vm.vcpus[0]
        .peer
        .send(VcpuCmd::Initialize(
            Some(BootRegs::Aarch64 { pc: 0, x: [0; 4] }),
            tx,
        ))
        .unwrap();
    assert!(matches!(
        rx.recv_timeout(PAUSE_TIMEOUT).unwrap(),
        Err(Error::Setup(_))
    ));
    assert_eq!(counts.runs.load(SeqCst), 0);
    assert_eq!(counts.boots.load(SeqCst), 1);
    no_state_calls(&counts);
    vm.resume().unwrap();
    wait_for_runs(&counts, 0);
    vm.pause().unwrap();
    let timeline = counts.timeline.lock().unwrap();
    let first_run = timeline.iter().position(|s| s.starts_with("run ")).unwrap();
    assert!(timeline[..first_run].contains(&"boot-init 0".into()));
    assert!(timeline[..first_run].contains(&"secondary-init 1".into()));
    drop(timeline);
    vm.shutdown();
    assert_eq!(counts.drops.load(SeqCst), 2);
    no_state_calls(&counts);
}

#[test]
fn failed_initialization_is_returned_and_no_cpu_runs() {
    for lifecycle in [LifecyclePolicy::ColdBootOnly, LifecyclePolicy::Resettable] {
        let temp = tempfile::tempdir().unwrap();
        let (config, opts, hv, counts) = fixture(temp.path(), 2, StartMode::Boot);
        counts.fail_boot.store(true, SeqCst);
        let error = Vmm::start_with_lifecycle(config, hv, opts, lifecycle).unwrap_err();
        assert!(error.to_string().contains("injected boot failure"));
        assert_eq!(counts.runs.load(SeqCst), 0);
        assert_eq!(counts.drops.load(SeqCst), 2);
        if lifecycle == LifecyclePolicy::ColdBootOnly {
            no_state_calls(&counts);
        }
    }
}

#[test]
fn unsupported_save_reset_and_suspend_preserve_running_vm_and_files() {
    let temp = tempfile::tempdir().unwrap();
    let (config, opts, hv, counts) = fixture(temp.path(), 1, StartMode::Boot);
    let mut vm =
        Vmm::start_with_lifecycle(config, hv, opts, LifecyclePolicy::ColdBootOnly).unwrap();
    wait_for_runs(&counts, 0);
    let device_calls = Arc::new(AtomicUsize::new(0));
    vm.devices
        .push(Arc::new(Mutex::new(ProbeDevice(device_calls.clone()))));
    let target = temp.path().join("must-not-exist");
    assert!(matches!(
        vm.reset(),
        Err(Error::UnsupportedLifecycle("reset"))
    ));
    assert!(matches!(
        vm.snapshot(&target, "test"),
        Err(Error::UnsupportedLifecycle("snapshot"))
    ));
    assert!(matches!(
        vm.suspend(&target),
        Err(Error::UnsupportedLifecycle("suspend"))
    ));
    assert!(matches!(
        vm.suspend_in_place(&target),
        Err(Error::UnsupportedLifecycle("suspend"))
    ));
    assert!(matches!(
        snapshot::restore(&mut vm, &target),
        Err(Error::UnsupportedLifecycle("restore"))
    ));
    assert!(!target.exists());
    assert_eq!(vm.state(), RunState::Running);
    assert_eq!(device_calls.load(SeqCst), 0);
    assert_eq!(counts.zeroes.load(SeqCst), 0);
    assert_eq!(counts.drops.load(SeqCst), 0);
    assert_eq!(counts.boots.load(SeqCst), 1);
    no_state_calls(&counts);
    vm.pause().unwrap();
    let before = counts.runs.load(SeqCst);
    vm.resume().unwrap();
    wait_for_runs(&counts, before);
    vm.shutdown();
    assert_eq!(counts.drops.load(SeqCst), 1);
}

#[test]
fn restore_policy_is_checked_before_snapshot_read_or_state_directory_creation() {
    for start in [
        StartMode::Restore(PathBuf::from("missing")),
        StartMode::RestorePaused(PathBuf::from("missing")),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let (config, opts, hv, counts) = fixture(temp.path(), 1, start);
        let state_dir = opts.state_dir.clone();
        assert!(matches!(
            Vmm::start_with_lifecycle(config, hv, opts, LifecyclePolicy::ColdBootOnly),
            Err(Error::UnsupportedLifecycle("restore"))
        ));
        assert!(!state_dir.exists());
        assert_eq!(counts.boots.load(SeqCst), 0);
        no_state_calls(&counts);
    }
}

#[test]
fn default_policy_retains_pristine_state_and_reset() {
    let temp = tempfile::tempdir().unwrap();
    let (config, opts, hv, counts) = fixture(temp.path(), 1, StartMode::BootPaused);
    let mut vm = Vmm::start(config, hv, opts).unwrap();
    assert_eq!(vm.lifecycle, LifecyclePolicy::Resettable);
    assert_eq!(counts.saves.load(SeqCst), 1);
    assert_eq!(counts.restores.load(SeqCst), 1);
    assert_eq!(counts.irq_saves.load(SeqCst), 1);
    vm.reset().unwrap();
    assert_eq!(counts.restores.load(SeqCst), 2);
    assert_eq!(counts.boots.load(SeqCst), 2);
    assert_eq!(vm.state(), RunState::Paused);
    assert_eq!(counts.runs.load(SeqCst), 0);
}

#[test]
fn unsupported_x64_backend_is_rejected_before_state_creation() {
    let temp = tempfile::tempdir().unwrap();
    let (config, opts, hv, counts) = fixture(temp.path(), 1, StartMode::BootPaused);
    let hv = Arc::new(FakeHv {
        vm: hv.vm.clone(),
        arch: Arch::X86_64,
    });
    assert!(Vmm::start(config, hv, opts)
        .unwrap_err()
        .to_string()
        .contains("requires the x86_64 KVM backend"));
    assert!(!temp.path().join("state").exists());
    assert_eq!(counts.runs.load(SeqCst), 0);
}

#[derive(Debug)]
struct ProbeDevice(Arc<AtomicUsize>);
impl Snapshottable for ProbeDevice {
    fn snapshot_id(&self) -> String {
        "lifecycle-probe".into()
    }
    fn save(&mut self) -> vroom_snapshot::Result<Vec<u8>> {
        self.0.fetch_add(1, SeqCst);
        Ok(Vec::new())
    }
    fn restore(&mut self, _: &[u8]) -> vroom_snapshot::Result<()> {
        self.0.fetch_add(1, SeqCst);
        Ok(())
    }
    fn pause(&mut self) {
        self.0.fetch_add(1, SeqCst);
    }
    fn resume(&mut self) {
        self.0.fetch_add(1, SeqCst);
    }
}

struct FailingTpmCapture(Arc<AtomicBool>);
impl vroom_devices::tpm::TpmBackend for FailingTpmCapture {
    fn execute(&mut self, command: &[u8]) -> Vec<u8> { command.to_vec() }
    fn reset(&mut self) {}
    fn save(&mut self) -> vroom_snapshot::Result<Vec<u8>> {
        if self.0.load(SeqCst) {
            Err(vroom_snapshot::Error::Invalid("injected TPM capture failure".into()))
        } else {
            Ok(b"test complete TPM state".to_vec())
        }
    }
}

fn add_failing_tpm(vm: &mut Vmm) -> Arc<AtomicBool> {
    let fail = Arc::new(AtomicBool::new(true));
    vm.devices.push(Arc::new(Mutex::new(vroom_devices::tpm::Tpm::new(
        0x0c00_0000, Box::new(FailingTpmCapture(fail.clone())),
    ))));
    fail
}

#[test]
fn failed_tpm_snapshot_does_not_commit_and_preserves_running_or_paused_state() {
    for mode in [StartMode::Boot, StartMode::BootPaused] {
        let temp = tempfile::tempdir().unwrap();
        let (config, opts, hv, counts) = fixture(temp.path(), 1, mode);
        let mut vm = Vmm::start(config, hv, opts).unwrap();
        let state = vm.state();
        let fail = add_failing_tpm(&mut vm);
        let rejected = temp.path().join("rejected");
        let before = counts.runs.load(SeqCst);
        let error = vm.snapshot(&rejected, "TPM capture failure").unwrap_err();
        assert!(error.to_string().contains("injected TPM capture failure"));
        assert!(vroom_snapshot::SnapshotDir::open(&rejected).is_err());
        assert_eq!(vm.state(), state);
        assert_eq!(counts.drops.load(SeqCst), 0);
        if state == RunState::Running {
            wait_for_runs(&counts, before);
        } else {
            assert_eq!(counts.runs.load(SeqCst), 0);
        }
        // A fresh target can be saved after the transient backend fault clears.
        fail.store(false, SeqCst);
        let complete = temp.path().join("complete");
        vm.snapshot(&complete, "retry").unwrap();
        assert!(vroom_snapshot::SnapshotDir::open(&complete).is_ok());
        assert_eq!(vm.state(), state);
        vm.shutdown_checked().unwrap();
    }
}

#[test]
fn failed_tpm_suspend_keeps_workers_paused_and_can_resume() {
    let temp = tempfile::tempdir().unwrap();
    let (config, opts, hv, counts) = fixture(temp.path(), 1, StartMode::Boot);
    let mut vm = Vmm::start(config, hv, opts).unwrap();
    add_failing_tpm(&mut vm);
    let target = temp.path().join("rejected-suspend");
    assert!(vm.suspend(&target).unwrap_err().to_string().contains("injected TPM capture failure"));
    assert!(vroom_snapshot::SnapshotDir::open(&target).is_err());
    assert_eq!(vm.state(), RunState::Paused);
    assert_eq!(counts.drops.load(SeqCst), 0);
    let before = counts.runs.load(SeqCst);
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(counts.runs.load(SeqCst), before);
    vm.resume().unwrap();
    wait_for_runs(&counts, before);
    vm.shutdown_checked().unwrap();
}

#[test]
fn reset_vector_initialization_is_acknowledged_parked_and_failure_never_runs() {
    for fail in [false, true] {
        let counts = Arc::new(Counts::default());
        counts.fail_boot.store(fail, SeqCst);
        let (events, _events_rx) = mpsc::channel();
        let shared = Arc::new(VcpuShared {
            vm: Arc::new(FakeVm { counts: counts.clone(), memory: Mutex::new(None), kernel_psci: false }),
            mmio: vroom_devices::BusBuilder::new().build(), pio: None,
            cpus: Mutex::new(psci::CpuTable::new(vec![0])), events, peers: Default::default(),
            run_calls: Default::default(), device_exits: Default::default(),
            halt_on_stop: Default::default(), halting: Default::default(),
            parked: Default::default(), halted_at: Default::default(),
        });
        let mut worker = vcpu::spawn(0, shared.clone(), LifecyclePolicy::ColdBootOnly).unwrap();
        let (tx, rx) = mpsc::channel();
        worker.peer.send(VcpuCmd::Initialize(Some(BootRegs::X86ResetVector), tx)).unwrap();
        assert_eq!(rx.recv_timeout(PAUSE_TIMEOUT).unwrap().is_err(), fail);
        assert_eq!(*counts.boot_regs.lock().unwrap(), [BootRegs::X86ResetVector]);
        // Give an incorrectly running worker a chance to enter the counting
        // fake backend before the command barrier. No guest instructions run.
        std::thread::sleep(Duration::from_millis(20));
        let (tx, rx) = mpsc::channel();
        worker.peer.send(VcpuCmd::Pause(tx)).unwrap();
        rx.recv_timeout(PAUSE_TIMEOUT).unwrap();
        assert_eq!(shared.run_calls.load(SeqCst), 0);
        assert_eq!(counts.runs.load(SeqCst), 0);
        let (tx, rx) = mpsc::channel();
        worker.peer.send(VcpuCmd::Initialize(Some(BootRegs::X86ResetVector), tx)).unwrap();
        assert!(rx.recv_timeout(PAUSE_TIMEOUT).unwrap().is_err());
        worker.peer.send(VcpuCmd::Resume).unwrap();
        if !fail { wait_for_runs(&counts, 0); }
        else { std::thread::sleep(Duration::from_millis(20)); }
        let (tx, rx) = mpsc::channel();
        worker.peer.send(VcpuCmd::Pause(tx)).unwrap();
        rx.recv_timeout(PAUSE_TIMEOUT).unwrap();
        if fail {
            assert_eq!(shared.run_calls.load(SeqCst), 0);
            assert_eq!(counts.runs.load(SeqCst), 0);
        }
        assert_eq!(counts.boots.load(SeqCst), 1);
        no_state_calls(&counts);
        worker.peer.send(VcpuCmd::Stop).unwrap();
        worker.join_checked().unwrap();
        assert_eq!(counts.drops.load(SeqCst), 1);
    }
}

#[test]
fn failed_worker_cold_start_cannot_retry_or_accept_state_commands() {
    let counts = Arc::new(Counts::default());
    counts.fail_boot.store(true, SeqCst);
    let vm = Arc::new(FakeVm {
        counts: counts.clone(),
        memory: Mutex::new(None),
        kernel_psci: false,
    });
    let (events, _rx) = mpsc::channel();
    let shared = Arc::new(VcpuShared {
        vm,
        mmio: vroom_devices::BusBuilder::new().build(),
        pio: None,
        cpus: Mutex::new(psci::CpuTable::new(vec![0])),
        events,
        peers: Default::default(),
        run_calls: Default::default(),
        device_exits: Default::default(),
        halt_on_stop: Default::default(),
        halting: Default::default(),
        parked: Default::default(),
        halted_at: Default::default(),
    });
    let mut worker = vcpu::spawn(0, shared, LifecyclePolicy::ColdBootOnly).unwrap();
    let regs = BootRegs::Aarch64 { pc: 0, x: [0; 4] };
    let (tx, rx) = mpsc::channel();
    worker
        .peer
        .send(VcpuCmd::Initialize(Some(regs.clone()), tx))
        .unwrap();
    assert!(rx
        .recv_timeout(PAUSE_TIMEOUT)
        .unwrap()
        .unwrap_err()
        .to_string()
        .contains("injected boot failure"));
    counts.fail_boot.store(false, SeqCst);
    let (tx, rx) = mpsc::channel();
    worker
        .peer
        .send(VcpuCmd::Initialize(Some(regs.clone()), tx))
        .unwrap();
    assert!(rx
        .recv_timeout(PAUSE_TIMEOUT)
        .unwrap()
        .unwrap_err()
        .to_string()
        .contains("already attempted"));
    let (tx, rx) = mpsc::channel();
    worker.peer.send(VcpuCmd::Save(tx)).unwrap();
    assert!(matches!(
        rx.recv_timeout(PAUSE_TIMEOUT).unwrap(),
        Err(Error::UnsupportedLifecycle("snapshot"))
    ));
    let (tx, rx) = mpsc::channel();
    worker
        .peer
        .send(VcpuCmd::Restore(Box::default(), true, tx))
        .unwrap();
    assert!(matches!(
        rx.recv_timeout(PAUSE_TIMEOUT).unwrap(),
        Err(Error::UnsupportedLifecycle("restore"))
    ));
    let (tx, rx) = mpsc::channel();
    worker.peer.send(VcpuCmd::Reset(Some(regs), tx)).unwrap();
    assert!(matches!(
        rx.recv_timeout(PAUSE_TIMEOUT).unwrap(),
        Err(Error::UnsupportedLifecycle("reset"))
    ));
    worker.peer.send(VcpuCmd::Resume).unwrap();
    let (tx, rx) = mpsc::channel();
    worker.peer.send(VcpuCmd::Pause(tx)).unwrap();
    rx.recv_timeout(PAUSE_TIMEOUT).unwrap();
    assert_eq!(counts.runs.load(SeqCst), 0);
    assert_eq!(counts.boots.load(SeqCst), 1);
    no_state_calls(&counts);
    worker.peer.send(VcpuCmd::Stop).unwrap();
    worker.join();
    assert_eq!(counts.drops.load(SeqCst), 1);
}

#[test]
fn resettable_runtime_cpu_on_remains_distinct_from_once_only_initialization() {
    let temp = tempfile::tempdir().unwrap();
    let (config, opts, original, counts) = fixture(temp.path(), 2, StartMode::BootPaused);
    let hv = Arc::new(FakeHv {
        vm: Arc::new(FakeVm {
            counts: counts.clone(),
            memory: Mutex::new(None),
            kernel_psci: false,
        }),
        arch: original.arch,
    });
    let vm = Vmm::start(config, hv, opts).unwrap();
    assert_eq!(counts.saves.load(SeqCst), 2);
    for expected in [2, 3] {
        vm.vcpus[1]
            .peer
            .send(VcpuCmd::PowerOn(BootRegs::Aarch64 {
                pc: 0x4000_1000,
                x: [1, 0, 0, 0],
            }))
            .unwrap();
        let (tx, rx) = mpsc::channel();
        vm.vcpus[1].peer.send(VcpuCmd::Pause(tx)).unwrap();
        rx.recv_timeout(PAUSE_TIMEOUT).unwrap();
        assert_eq!(counts.boots.load(SeqCst), expected);
        assert_eq!(counts.restores.load(SeqCst), expected);
    }
    assert_eq!(counts.runs.load(SeqCst), 0);
}


#[test]
fn checked_shutdown_reports_panic_and_still_joins_every_worker() {
    let temp = tempfile::tempdir().unwrap();
    let (config, opts, hv, counts) = fixture(temp.path(), 2, StartMode::BootPaused);
    let mut vm = Vmm::start(config, hv, opts).unwrap();
    counts.panic_boot_cpu.store(true, SeqCst);
    vm.resume().unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while counts.drops.load(SeqCst) == 0 {
        assert!(Instant::now() < deadline, "injected panic did not occur");
        std::thread::yield_now();
    }
    assert!(vm.shutdown_checked().unwrap_err().to_string().contains("worker panicked"));
    assert_eq!(vm.state(), RunState::Stopped);
    assert_eq!(counts.drops.load(SeqCst), 2, "the other worker must also be joined");
}

fn startup_shared(counts: &Arc<Counts>) -> Arc<VcpuShared> {
    let (events, _) = mpsc::channel();
    Arc::new(VcpuShared {
        vm: Arc::new(FakeVm { counts: counts.clone(), memory: Mutex::new(None), kernel_psci: false }),
        mmio: vroom_devices::BusBuilder::new().build(), pio: None,
        cpus: Mutex::new(psci::CpuTable::new(vec![0])), events, peers: Default::default(),
        run_calls: Default::default(), device_exits: Default::default(),
        halt_on_stop: Default::default(), halting: Default::default(),
        parked: Default::default(), halted_at: Default::default(),
    })
}

fn startup_drop_gate(counts: &Counts) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entered, receive) = mpsc::channel(); let (release, wait) = mpsc::channel();
    *counts.startup_drop_gate.lock().unwrap() = Some(StartupDropGate { entered, release: wait });
    (receive, release)
}

// Counts are sampled immediately when startup returns, never after a later
// wait that could hide a detached worker eventually destroying its resources.
type StartupReturn = (Error, usize, usize, usize);
fn startup_return(error: Error, counts: &Counts) -> StartupReturn {
    (error, counts.drops.load(SeqCst), counts.vm_drops.load(SeqCst), counts.runs.load(SeqCst))
}

fn joined_startup_error(counts: &Counts, entered: mpsc::Receiver<()>, release: mpsc::Sender<()>,
    returned: mpsc::Receiver<StartupReturn>, caller: std::thread::JoinHandle<()>, cpus: usize) -> Error {
    let dropping = entered.recv_timeout(Duration::from_secs(2));
    let early = returned.recv_timeout(Duration::from_millis(40));
    let returned_before_cleanup = early.is_ok();
    // Always release and join before asserting, even if the regression returns
    // early. The 40ms wait is a test checkpoint, not a runtime timeout policy.
    let _ = release.send(());
    let result = early.or_else(|_| returned.recv_timeout(Duration::from_secs(2)));
    caller.join().unwrap();
    assert!(dropping.is_ok(), "worker did not reach the controlled destructor");
    assert!(!returned_before_cleanup, "startup detached the worker during destruction");
    let (error, cpu_drops, vm_drops, runs) = result.unwrap();
    assert_eq!((cpu_drops, vm_drops, runs), (cpus, 1, 0));
    assert_eq!(counts.boots.load(SeqCst), 0);
    assert!(!counts.drop_wait_timed_out.load(SeqCst));
    error
}

#[test]
fn startup_errors_and_panics_join_owned_resources_before_return_without_run() {
    for failure in 0..4 {
        let counts = Arc::new(Counts::default());
        counts.fail_create.store(failure == 0, SeqCst);
        counts.panic_create.store(failure == 1, SeqCst);
        counts.fail_save.store(failure >= 2, SeqCst);
        counts.panic_cpu_drop.store(failure == 3, SeqCst);
        let shared = startup_shared(&counts); let weak = Arc::downgrade(&shared);
        let (entered, release) = startup_drop_gate(&counts);
        let (returned_tx, returned) = mpsc::channel(); let c = counts.clone();
        let caller = std::thread::spawn(move || {
            let error = vcpu::spawn(0, shared, LifecyclePolicy::Resettable).unwrap_err();
            let _ = returned_tx.send(startup_return(error, &c));
        });
        let error = joined_startup_error(&counts, entered, release, returned, caller, usize::from(failure >= 2));
        assert!(weak.upgrade().is_none(), "shared VM owner survived failed startup");
        match failure {
            0 => assert!(matches!(error, Error::Hv(vroom_hv::Error::Invalid(ref s)) if s == "injected CPU creation failure")),
            2 => assert!(matches!(error, Error::Hv(vroom_hv::Error::Invalid(ref s)) if s == "injected pristine save failure")),
            _ => assert!(matches!(error, Error::Setup(ref s) if s == "vCPU worker panicked")),
        }
        assert_eq!(counts.saves.load(SeqCst), usize::from(failure >= 2));
    }
}

#[test]
fn startup_disconnected_readiness_closes_commands_and_joins_before_error() {
    let counts = Arc::new(Counts::default()); let shared = startup_shared(&counts);
    let weak = Arc::downgrade(&shared); let (entered, release) = startup_drop_gate(&counts);
    let (tx, rx) = mpsc::channel(); let (ready_tx, ready_rx) = mpsc::channel();
    // A clean disconnected readiness channel is not normally produced by the
    // backend. Inject it at the same private completion helper used by spawn.
    let worker = std::thread::spawn(move || {
        let cpu = shared.vm.create_vcpu(0).unwrap();
        drop(ready_tx);
        assert!(matches!(rx.recv_timeout(Duration::from_secs(2)), Err(mpsc::RecvTimeoutError::Disconnected)),
            "startup must close commands before joining");
        drop(cpu); drop(shared);
    });
    let (returned_tx, returned) = mpsc::channel(); let c = counts.clone();
    let caller = std::thread::spawn(move || {
        let error = vcpu::finish_startup_for_test(tx, ready_rx, worker).unwrap_err();
        let _ = returned_tx.send(startup_return(error, &c));
    });
    assert!(matches!(joined_startup_error(&counts, entered, release, returned, caller, 1), Error::VcpuGone));
    assert!(weak.upgrade().is_none());
    assert_eq!(counts.saves.load(SeqCst), 0);
}

#[test]
fn startup_abandoned_ready_receiver_never_enters_the_command_loop() {
    for lifecycle in [LifecyclePolicy::ColdBootOnly, LifecyclePolicy::Resettable] {
        let counts = Arc::new(Counts::default()); let shared = startup_shared(&counts);
        let weak = Arc::downgrade(&shared);
        let (tx, rx) = mpsc::channel(); let (ready_tx, ready_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        tx.send(VcpuCmd::Initialize(Some(BootRegs::X86ResetVector), reply_tx)).unwrap();
        tx.send(VcpuCmd::Resume).unwrap(); tx.send(VcpuCmd::Stop).unwrap();
        drop(ready_rx);
        let worker = std::thread::spawn(move || vcpu::start_worker_for_test(0, shared, lifecycle, rx, ready_tx));
        worker.join().unwrap();
        // Ignoring the send failure would consume Initialize before Stop,
        // even though this queued Stop prevents an accidentally endless loop.
        assert!(matches!(reply_rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)));
        assert_eq!(counts.boots.load(SeqCst), 0);
        assert_eq!(counts.runs.load(SeqCst), 0);
        assert_eq!(counts.drops.load(SeqCst), 1);
        assert_eq!(counts.vm_drops.load(SeqCst), 1);
        assert_eq!(counts.saves.load(SeqCst), usize::from(lifecycle == LifecyclePolicy::Resettable));
        assert!(weak.upgrade().is_none());
    }
}

/// The frontend's sandbox goes on before any guest instruction, at boot
/// and at resume (`vroom run` locks down in `confine`).
#[test]
fn confine_runs_before_the_guest_and_its_failure_never_runs_it() {
    let temp = tempfile::tempdir().unwrap();
    let (config, opts, hv, counts) = fixture(temp.path(), 2, StartMode::Boot);
    let mut vm = Vmm::start_confined(config.clone(), hv.clone(), opts, |vm| {
        // Room for a worker that was let go to call run.
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(vm.state(), RunState::Paused);
        assert_eq!(vm.vcpu_run_calls(), 0);
        assert_eq!(counts.runs.load(SeqCst), 0);
        Ok::<_, Error>(())
    })
    .unwrap();
    assert_eq!(vm.state(), RunState::Running);
    wait_for_runs(&counts, 0);
    let snap = temp.path().join("snap");
    vm.snapshot(&snap, "t").unwrap();
    drop(vm);

    let opts = VmmOptions {
        serial_out: Box::new(std::io::sink()),
        start: StartMode::Restore(snap.clone()),
        clipboard: None,
        ui: None,
        state_dir: temp.path().join("state"),
        sandbox: false,
    };
    let vm = Vmm::start_confined(config.clone(), hv.clone(), opts, |vm| {
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(vm.vcpu_run_calls(), 0);
        Ok::<_, Error>(())
    })
    .unwrap();
    let after = counts.runs.load(SeqCst);
    wait_for_runs(&counts, after);
    drop(vm);

    let runs = counts.runs.load(SeqCst);
    let drops = counts.drops.load(SeqCst);
    let opts = VmmOptions {
        serial_out: Box::new(std::io::sink()),
        start: StartMode::Restore(snap),
        clipboard: None,
        ui: None,
        state_dir: temp.path().join("state"),
        sandbox: false,
    };
    let e = Vmm::start_confined(config, hv, opts, |_| Err(Error::Setup("no sandbox".into()))).err().unwrap();
    assert!(e.to_string().contains("no sandbox"));
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(counts.runs.load(SeqCst), runs, "a failed lock-down ran the guest");
    assert_eq!(counts.drops.load(SeqCst), drops + 2, "workers joined");
}

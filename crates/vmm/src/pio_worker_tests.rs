//! Scripted backend exits through the actual vCPU thread, without guest execution.
use super::*;
use crate::pio::{PcPioBus, PioRegion};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use vroom_devices::BusDevice;
use vroom_hv::IrqChipState;
use vroom_memory::GuestMemory;

#[derive(Debug)]
struct Script {
    exits: Mutex<VecDeque<VcpuExit>>,
    completions: Mutex<Vec<Vec<u8>>>,
    completion_attempts: AtomicUsize,
    runs: AtomicUsize,
    drops: AtomicUsize,
    fail_completion: bool,
}

#[derive(Debug)]
struct NoKick;
impl VcpuKicker for NoKick {
    fn kick(&self) {}
    fn wake(&self) {}
}

struct ScriptCpu {
    script: Arc<Script>,
    pending_read: Option<usize>,
}
impl Drop for ScriptCpu {
    fn drop(&mut self) {
        self.script.drops.fetch_add(1, SeqCst);
    }
}
impl Vcpu for ScriptCpu {
    fn index(&self) -> u32 {
        0
    }
    fn arch_id(&self) -> u64 {
        0
    }
    fn kicker(&self) -> Arc<dyn VcpuKicker> {
        Arc::new(NoKick)
    }
    fn run(&mut self) -> vroom_hv::Result<VcpuExit> {
        self.script.runs.fetch_add(1, SeqCst);
        if self.pending_read.is_some() {
            return Err(vroom_hv::Error::Invalid(
                "run before completing PIO read".into(),
            ));
        }
        let exit = self
            .script
            .exits
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| vroom_hv::Error::Invalid("run after script ended".into()))?;
        if let VcpuExit::PioIn { len, .. } = exit {
            self.pending_read = Some(len);
        }
        Ok(exit)
    }
    fn complete_pio_in(&mut self, bytes: &[u8]) -> vroom_hv::Result<()> {
        self.script.completion_attempts.fetch_add(1, SeqCst);
        if self.pending_read != Some(bytes.len()) {
            return Err(vroom_hv::Error::Invalid(
                "wrong PIO completion length".into(),
            ));
        }
        if self.script.fail_completion {
            return Err(vroom_hv::Error::Invalid(
                "injected PIO completion failure".into(),
            ));
        }
        self.script.completions.lock().unwrap().push(bytes.to_vec());
        self.pending_read = None;
        Ok(())
    }
    fn set_boot_regs(&mut self, _: &BootRegs) -> vroom_hv::Result<()> {
        Ok(())
    }
    fn complete_mmio_read(&mut self, _: &[u8]) -> vroom_hv::Result<()> {
        unreachable!("PIO-only fixture")
    }
    fn complete_hypercall(&mut self, _: [u64; 4]) -> vroom_hv::Result<()> {
        unreachable!("PIO-only fixture")
    }
    fn save_state(&mut self) -> vroom_hv::Result<VcpuState> {
        panic!("cold worker must not save state")
    }
    fn restore_state(&mut self, _: &VcpuState) -> vroom_hv::Result<()> {
        panic!("cold worker must not restore state")
    }
}

struct ScriptVm(Arc<Script>);
impl Vm for ScriptVm {
    fn map_memory(&self, _: &GuestMemory) -> vroom_hv::Result<()> {
        Ok(())
    }
    fn map_readonly(&self, _: u64, _: Arc<vroom_memory::MmapRegion>) -> vroom_hv::Result<()> {
        Ok(())
    }
    fn create_vcpu(&self, _: u32) -> vroom_hv::Result<Box<dyn Vcpu>> {
        Ok(Box::new(ScriptCpu {
            script: self.0.clone(),
            pending_read: None,
        }))
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
        unreachable!("worker has no irqchip state operations")
    }
    fn restore_irqchip(&self, _: &IrqChipState) -> vroom_hv::Result<()> {
        unreachable!("worker has no irqchip state operations")
    }
}

#[derive(Debug, Default)]
struct Device {
    reads: usize,
    writes: Vec<(u64, Vec<u8>)>,
}
impl BusDevice for Device {
    fn read(&mut self, offset: u64, bytes: &mut [u8]) {
        self.reads += 1;
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = 0x60 + offset as u8 + index as u8;
        }
    }
    fn write(&mut self, offset: u64, bytes: &[u8]) {
        self.writes.push((offset, bytes.to_vec()));
    }
}

fn bus(device: Arc<Mutex<Device>>) -> PcPioBus {
    PcPioBus::new(vec![
        PioRegion::new(0x500, 16, &[1, 2, 4], device.clone()).unwrap(),
        PioRegion::new(0x600, 4, &[1], device).unwrap(),
    ])
    .unwrap()
}

fn run(
    exits: Vec<VcpuExit>,
    pio: Option<PcPioBus>,
    fail_completion: bool,
) -> (VmEvent, Arc<Script>, u64) {
    let script = Arc::new(Script {
        exits: Mutex::new(exits.into()),
        completions: Mutex::new(Vec::new()),
        completion_attempts: AtomicUsize::new(0),
        runs: AtomicUsize::new(0),
        drops: AtomicUsize::new(0),
        fail_completion,
    });
    let (events, receiver) = mpsc::channel();
    let shared = Arc::new(VcpuShared {
        vm: Arc::new(ScriptVm(script.clone())),
        mmio: Bus::default(),
        pio,
        cpus: Mutex::new(CpuTable::new(vec![0])),
        events,
        peers: Default::default(),
        run_calls: Default::default(),
        device_exits: Default::default(),
        halt_on_stop: Default::default(),
        halting: Default::default(),
        parked: Default::default(),
        halted_at: Default::default(),
    });
    let mut handle = spawn(0, shared.clone(), LifecyclePolicy::ColdBootOnly).unwrap();
    let (tx, rx) = mpsc::channel();
    handle
        .peer
        .send(VcpuCmd::Initialize(
            Some(BootRegs::X86_64 {
                rip: 0,
                rsi: 0,
                cr3: 0,
                gdt: 0,
            }),
            tx,
        ))
        .unwrap();
    rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
    assert_eq!(script.runs.load(SeqCst), 0);
    handle.peer.send(VcpuCmd::Resume).unwrap();
    let event = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
    // A terminal exit/error parks the worker. A command round trip proves it
    // did not rerun the backend or manufacture a second read completion.
    let (tx, rx) = mpsc::channel();
    handle.peer.send(VcpuCmd::Pause(tx)).unwrap();
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    handle.peer.send(VcpuCmd::Stop).unwrap();
    handle.join();
    assert_eq!(script.drops.load(SeqCst), 1);
    assert!(receiver.try_recv().is_err());
    (event, script, shared.device_exits.load(SeqCst))
}

#[test]
fn scalar_reads_complete_once_at_exact_width_before_next_run() {
    let device = Arc::new(Mutex::new(Device::default()));
    let mut exits = Vec::new();
    for (port, len) in [(0x500, 1), (0x502, 2), (0x504, 4)] {
        exits.push(VcpuExit::PioIn { port, len });
        exits.push(VcpuExit::PioOut {
            port,
            len,
            data: [1, 2, 3, 4],
        });
    }
    exits.push(VcpuExit::PioIn {
        port: 0x700,
        len: 4,
    });
    exits.push(VcpuExit::PioOut {
        port: 0x700,
        len: 4,
        data: [9; 4],
    });
    exits.push(VcpuExit::SystemOff);
    let (event, script, count) = run(exits, Some(bus(device.clone())), false);
    assert_eq!(event, VmEvent::Shutdown);
    assert_eq!(script.runs.load(SeqCst), 9);
    assert_eq!(script.completion_attempts.load(SeqCst), 4);
    assert_eq!(
        *script.completions.lock().unwrap(),
        vec![
            vec![0x60],
            vec![0x62, 0x63],
            vec![0x64, 0x65, 0x66, 0x67],
            vec![0xff; 4]
        ]
    );
    assert_eq!(
        device.lock().unwrap().writes,
        vec![(0, vec![1]), (2, vec![1, 2]), (4, vec![1, 2, 3, 4])]
    );
    assert_eq!(device.lock().unwrap().reads, 3);
    assert_eq!(count, 8);
}

#[test]
fn malformed_or_cross_boundary_exits_stop_without_device_access_or_completion() {
    let mut bad = Vec::new();
    for len in [0, 3, 8, usize::MAX] {
        bad.push(VcpuExit::PioIn { port: 0x500, len });
        bad.push(VcpuExit::PioOut {
            port: 0x500,
            len,
            data: [1; 4],
        });
    }
    for (port, len) in [(0xffff, 2), (0xfffd, 4), (0x4ff, 2), (0x50f, 2), (0x600, 2)] {
        bad.push(VcpuExit::PioIn { port, len });
        bad.push(VcpuExit::PioOut {
            port,
            len,
            data: [1; 4],
        });
    }
    for exit in bad {
        let device = Arc::new(Mutex::new(Device::default()));
        let (event, script, count) = run(
            vec![exit.clone(), VcpuExit::SystemOff],
            Some(bus(device.clone())),
            false,
        );
        assert!(matches!(event, VmEvent::Crashed(_)), "{exit:?}: {event:?}");
        assert_eq!(script.runs.load(SeqCst), 1, "{exit:?}");
        assert_eq!(script.completion_attempts.load(SeqCst), 0);
        assert!(script.completions.lock().unwrap().is_empty());
        assert_eq!(device.lock().unwrap().reads, 0);
        assert!(device.lock().unwrap().writes.is_empty());
        assert_eq!(count, 0);
    }
}

#[test]
fn absent_ports_require_an_explicit_pc_bus() {
    for exit in [
        VcpuExit::PioIn {
            port: 0xffff,
            len: 1,
        },
        VcpuExit::PioOut {
            port: 0xffff,
            len: 1,
            data: [1; 4],
        },
    ] {
        let (event, script, count) = run(vec![exit, VcpuExit::SystemOff], None, false);
        assert!(
            matches!(event, VmEvent::Crashed(ref error) if error.contains("board without ports"))
        );
        assert_eq!(script.runs.load(SeqCst), 1);
        assert_eq!(script.completion_attempts.load(SeqCst), 0);
        assert_eq!(count, 0);
    }
    let (event, script, _) = run(
        vec![
            VcpuExit::PioIn {
                port: 0xffff,
                len: 1,
            },
            VcpuExit::SystemOff,
        ],
        Some(PcPioBus::new(Vec::new()).unwrap()),
        false,
    );
    assert_eq!(event, VmEvent::Shutdown);
    assert_eq!(*script.completions.lock().unwrap(), vec![vec![0xff]]);
}

#[test]
fn completion_error_parks_worker_without_rerun_or_fabricated_success() {
    let device = Arc::new(Mutex::new(Device::default()));
    let (event, script, _) = run(
        vec![
            VcpuExit::PioIn {
                port: 0x500,
                len: 2,
            },
            VcpuExit::SystemOff,
        ],
        Some(bus(device.clone())),
        true,
    );
    assert!(
        matches!(event, VmEvent::Crashed(ref error) if error.contains("injected PIO completion failure"))
    );
    assert_eq!(script.runs.load(SeqCst), 1);
    assert_eq!(script.completion_attempts.load(SeqCst), 1);
    assert!(script.completions.lock().unwrap().is_empty());
    assert_eq!(device.lock().unwrap().reads, 1);
}

#[test]
fn smm_io_never_reaches_generic_devices_or_read_completion() {
    use vroom_hv::X86IoExit;
    for io in [
        X86IoExit::PioIn { port: 0x500, len: 1 },
        X86IoExit::PioOut { port: 0x500, len: 1, data: [1; 4] },
        X86IoExit::MmioRead { addr: 0x100000, len: 1 },
        X86IoExit::MmioWrite { addr: 0x100000, len: 1, data: [1; 8] },
    ] {
        let device = Arc::new(Mutex::new(Device::default()));
        let (event, script, count) = run(
            vec![VcpuExit::X86SmmIo(io), VcpuExit::SystemOff], Some(bus(device.clone())), false);
        assert!(matches!(event, VmEvent::Crashed(ref e) if e.contains("SMM I/O is unavailable")));
        assert_eq!(script.runs.load(SeqCst), 1);
        assert_eq!(script.completion_attempts.load(SeqCst), 0);
        assert!(script.completions.lock().unwrap().is_empty());
        assert_eq!(device.lock().unwrap().reads, 0);
        assert!(device.lock().unwrap().writes.is_empty());
        assert_eq!(count, 0);
    }
}

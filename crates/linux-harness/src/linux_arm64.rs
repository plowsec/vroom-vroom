use std::fs::File;
use std::io::Read;
use std::sync::{
    Arc, Mutex,
    mpsc::{self, Receiver, Sender},
};
use std::time::{Duration, Instant};

use serde_json::json;
use vroom_arch::aarch64::layout;
use vroom_devices::IrqLine;
use vroom_hv::{IrqChipConfig, VcpuExit, VcpuKicker, Vm, VmParams};
use vroom_linux_harness::arm64_boot::{ArmBoot, ArmDevices, RAM_SIZE};
use vroom_linux_harness::{SerialEvidence, SerialSink};
use vroom_memory::{GuestAddress, GuestMemory};

type Result<T> = std::result::Result<T, String>;
type Evidence = Arc<Mutex<SerialEvidence>>;

#[derive(Clone, Copy, Debug)]
enum Command {
    Pause,
    Resume,
    Stop,
}
enum Event {
    Ready(Arc<dyn VcpuKicker>),
    Paused(u64, usize),
    Resumed,
    Stopped,
}

struct SerialIrq {
    vm: Arc<dyn Vm>,
    irq: u32,
    error: Arc<Mutex<Option<String>>>,
}
impl IrqLine for SerialIrq {
    fn set_level(&self, level: bool) {
        if let Err(error) = self.vm.set_irq_line(self.irq, level) {
            *self.error.lock().expect("IRQ error lock") = Some(format!("ARM device IRQ: {error}"));
        }
    }
}

fn state(evidence: &Evidence) -> Result<(u64, usize)> {
    let state = evidence
        .lock()
        .map_err(|_| "serial evidence lock poisoned")?;
    if let Some(error) = &state.error {
        return Err(error.clone());
    }
    Ok((state.heartbeats, state.bytes.len()))
}

fn read_bounded(path: &str, limit: u64) -> Result<Vec<u8>> {
    let file = File::open(path).map_err(|e| format!("open input: {e}"))?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > limit {
        return Err("input is not a bounded regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > limit {
        return Err("input grew beyond size limit".into());
    }
    Ok(bytes)
}

fn worker(
    vm: Arc<dyn Vm>,
    memory: GuestMemory,
    boot: ArmBoot,
    evidence: Evidence,
    commands: Receiver<Command>,
    events: Sender<Event>,
) -> Result<()> {
    // KVM vCPU creation, execution and destruction stay on this thread.
    let mut cpu = vm.create_vcpu(0).map_err(|e| e.to_string())?;
    let regs = boot.load(&memory, cpu.arch_id())?;
    cpu.set_boot_regs(&regs).map_err(|e| e.to_string())?;
    vm.finalize().map_err(|e| e.to_string())?;
    let irq_error = Arc::new(Mutex::new(None));
    let mut devices = ArmDevices::new(
        Box::new(SerialSink(evidence.clone())),
        Arc::new(SerialIrq {
            vm: vm.clone(),
            irq: layout::UART_SPI,
            error: irq_error.clone(),
        }),
        Arc::new(SerialIrq {
            vm,
            irq: layout::RTC_SPI,
            error: irq_error.clone(),
        }),
    );
    events
        .send(Event::Ready(cpu.kicker()))
        .map_err(|e| e.to_string())?;
    let mut paused = false;
    loop {
        // Only this worker changes run state. A pause is acknowledged here,
        // outside KVM, after the preceding serial I/O has been processed.
        let command = if paused {
            Some(commands.recv().map_err(|_| "controller disconnected")?)
        } else {
            match commands.try_recv() {
                Ok(command) => Some(command),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err("controller disconnected".into());
                }
            }
        };
        if let Some(command) = command {
            match command {
                Command::Pause if !paused => {
                    paused = true;
                    let (count, bytes) = state(&evidence)?;
                    events
                        .send(Event::Paused(count, bytes))
                        .map_err(|e| e.to_string())?;
                }
                Command::Resume if paused => {
                    paused = false;
                    events.send(Event::Resumed).map_err(|e| e.to_string())?;
                }
                Command::Stop => {
                    drop(cpu); // controller stop means the vCPU is destroyed
                    events.send(Event::Stopped).map_err(|e| e.to_string())?;
                    return Ok(());
                }
                _ => return Err("invalid controller state transition".into()),
            }
            continue;
        }
        if let Some(error) = irq_error.lock().map_err(|_| "IRQ lock poisoned")?.clone() {
            return Err(error);
        }
        state(&evidence)?;
        match cpu.run().map_err(|e| e.to_string())? {
            VcpuExit::MmioWrite { addr, len, data } => {
                let bytes = data.get(..len).ok_or("invalid ARM MMIO write length")?;
                devices.write(addr, bytes)?;
            }
            VcpuExit::MmioRead { addr, len } => {
                let mut bytes = [0u8; 8];
                let bytes = bytes.get_mut(..len).ok_or("invalid ARM MMIO read length")?;
                devices.read(addr, bytes)?;
                cpu.complete_mmio_read(bytes).map_err(|e| e.to_string())?;
            }
            VcpuExit::Kicked => (),
            // With the in-kernel GIC KVM normally waits for a timer/IRQ
            // itself. This handles a returned halt without a busy spin.
            VcpuExit::Halted => std::thread::sleep(Duration::from_millis(1)),
            other => return Err(format!("unexpected guest exit: {other:?}")),
        }
    }
}

fn receive(events: &Receiver<Event>, deadline: Instant) -> Result<Event> {
    loop {
        match events.recv_timeout(Duration::from_millis(10)) {
            Ok(event) => return Ok(event),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("vCPU worker disconnected before acknowledgement".into());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => (),
        }
        if Instant::now() >= deadline {
            return Err("controller acknowledgement timed out".into());
        }
    }
}

fn await_heartbeats(
    evidence: &Evidence,
    count: u64,
    events: &Receiver<Event>,
    deadline: Instant,
) -> Result<()> {
    loop {
        if state(evidence)?.0 >= count {
            return Ok(());
        }
        match events.try_recv() {
            Ok(_) => return Err("unexpected controller event while awaiting heartbeat".into()),
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err("vCPU worker exited before heartbeat".into());
            }
            Err(mpsc::TryRecvError::Empty) => (),
        }
        if Instant::now() >= deadline {
            return Err("PID1 heartbeat timed out".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn run(args: &[String], report: &mut serde_json::Value) -> Result<()> {
    if args.len() != 4 {
        return Err("usage: vroom-linux-harness KERNEL INITRAMFS TOKEN_HEX32".into());
    }
    let evidence = Arc::new(Mutex::new(SerialEvidence::new(&args[3])?));
    let kernel = read_bounded(&args[1], 128 << 20)?;
    let initramfs = read_bounded(&args[2], 8 << 20)?;
    let boot = ArmBoot::new(kernel, initramfs)?;
    let memory = GuestMemory::new(&[(GuestAddress(layout::RAM_BASE), RAM_SIZE)])
        .map_err(|e| e.to_string())?;
    let hv = vroom_hv::default_hypervisor().map_err(|e| e.to_string())?;
    let info = hv.irqchip_info().map_err(|e| e.to_string())?;
    if info.dist_size != 0x1_0000 || info.redist_size_per_cpu != 0x2_0000 {
        return Err("unsupported ARM GIC layout".into());
    }
    let vm = hv
        .create_vm(&VmParams {
            vcpus: 1,
            irqchip: IrqChipConfig::GicV3 {
                dist_base: layout::GIC_DIST_BASE,
                redist_base: layout::GIC_REDIST_BASE,
                msi: None,
            },
            nested: false,
            small_pages: false,
            helper_page: layout::HELPER_PAGE_BASE,
        })
        .map_err(|e| e.to_string())?;
    vm.map_memory(&memory).map_err(|e| e.to_string())?;
    let (commands, incoming) = mpsc::channel();
    let (events_tx, events) = mpsc::channel();
    let (done_tx, done) = mpsc::channel();
    let worker_evidence = evidence.clone();
    let thread = std::thread::spawn(move || {
        // Retain RAM until the worker exits, including a failed join. The VM
        // also retains its mapping, but ownership is explicit at this layer.
        let result = worker(vm, memory, boot, worker_evidence, incoming, events_tx);
        let _ = done_tx.send(result);
    });
    let mut kicker = None;
    let checks = (|| -> Result<()> {
        let Event::Ready(handle) = receive(&events, Instant::now() + Duration::from_secs(5))?
        else {
            return Err("missing vCPU ready acknowledgement".into());
        };
        kicker = Some(handle);
        await_heartbeats(
            &evidence,
            5,
            &events,
            Instant::now() + Duration::from_secs(45),
        )?;
        report["pid1_heartbeat_verified"] = json!(true);
        commands.send(Command::Pause).map_err(|e| e.to_string())?;
        kicker.as_ref().expect("ready kicker").kick();
        let Event::Paused(count, bytes) =
            receive(&events, Instant::now() + Duration::from_secs(2))?
        else {
            return Err("missing pause acknowledgement".into());
        };
        report["pause_acknowledged"] = json!(true);
        report["heartbeats_before_pause"] = json!(count);
        // Longer than three PID1 periods; compare all serial bytes as well.
        std::thread::sleep(Duration::from_millis(350));
        if state(&evidence)? != (count, bytes) {
            return Err("guest made serial progress while paused".into());
        }
        report["paused_quiescence_verified"] = json!(true);
        commands.send(Command::Resume).map_err(|e| e.to_string())?;
        let Event::Resumed = receive(&events, Instant::now() + Duration::from_secs(2))? else {
            return Err("missing resume acknowledgement".into());
        };
        await_heartbeats(
            &evidence,
            count + 5,
            &events,
            Instant::now() + Duration::from_secs(5),
        )?;
        report["resume_progress_verified"] = json!(true);
        Ok(())
    })();
    // Stop on success and every ordinary failure. The process watchdog is a
    // separate final bound if a kernel ioctl or worker cannot be joined.
    let _ = commands.send(Command::Stop);
    if let Some(handle) = kicker {
        handle.kick();
    }
    let stopped = done.recv_timeout(Duration::from_secs(2));
    let cleanup: Result<()> = match stopped {
        Ok(result) => match thread.join() {
            Ok(()) => {
                if result.is_ok() {
                    report["hypervisor_initiated_guest_stop_verified"] = json!(true);
                }
                result
            }
            Err(_) => Err("vCPU worker panicked".into()),
        },
        Err(_) => Err("vCPU worker did not stop within two seconds".into()),
    };
    if let Ok(state) = evidence.lock() {
        report["heartbeats_observed"] = json!(state.heartbeats);
        report["serial_bytes"] = json!(state.bytes.len());
        if checks.is_err() || cleanup.is_err() {
            eprintln!(
                "serial tail:\n{}",
                String::from_utf8_lossy(&state.bytes[state.bytes.len().saturating_sub(16384)..])
            );
        }
    }
    cleanup?;
    checks
}

pub fn main() {
    let (cancel, watchdog_rx) = mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if watchdog_rx.recv_timeout(Duration::from_secs(65)).is_err() {
            println!(
                "{}",
                json!({"schema_version": 1, "status": "unknown", "scope": "native_linux_harness",
                "error": "65-second process watchdog expired", "guest_poweroff_verified": false})
            );
            std::process::exit(2);
        }
    });
    let mut report = json!({"schema_version": 1, "status": "unknown", "scope": "native_linux_harness",
        "engine": "vroom_hv_kvm", "architecture": "arm64", "vcpus": 1, "ram_mib": 512, "runtime_limit_seconds": 65,
        "guest_disks": 0, "guest_nics": 0, "pid1_heartbeat_verified": false,
        "pause_acknowledged": false, "paused_quiescence_verified": false, "resume_progress_verified": false,
        "hypervisor_initiated_guest_stop_verified": false, "guest_poweroff_verified": false,
        "serial_receive_verified": false, "serial_irq_delivery_verified": false,
        "vroom_cli_or_rpc_verified": false, "security_features_verified": false});
    let result = run(&std::env::args().collect::<Vec<_>>(), &mut report);
    let code = if let Err(error) = result {
        report["status"] = json!("fail");
        report["error"] = json!(error);
        1
    } else {
        report["status"] = json!("pass");
        0
    };
    println!("{report}");
    let _ = cancel.send(());
    let _ = watchdog.join();
    std::process::exit(code);
}

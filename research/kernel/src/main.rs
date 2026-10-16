//! Line-delimited JSON control endpoint; the Textual frontend owns this process.
use clap::Parser;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;
use vroom_hv::debug::DebugConfig;
use vroom_kernel::arm64::{AddressSpace, Registers};
use vroom_vmm::{RunState, StartMode, VmEvent, Vmm, VmmOptions};

#[derive(Debug, Parser)]
#[command(about = "Windows ARM64 kernel debugger backend (JSON lines on stdio)")]
struct Args {
    /// A disposable vroom VM config. Boots paused; resume explicitly.
    config: PathBuf,
    /// Restore a vroom checkpoint instead of cold booting.
    #[arg(long)]
    restore: Option<PathBuf>,
}

fn number(v: &Value) -> Result<u64, String> {
    if let Some(n) = v.as_u64() {
        return Ok(n);
    }
    let s = v.as_str().ok_or("expected an integer or 0x address")?;
    if let Some(s) = s.strip_prefix("0x") {
        u64::from_str_radix(s, 16)
    } else {
        s.parse()
    }
    .map_err(|e| format!("invalid address: {e}"))
}

fn output(v: Value) -> Result<(), String> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &v).map_err(|e| e.to_string())?;
    writeln!(out)
        .and_then(|_| out.flush())
        .map_err(|e| e.to_string())
}

struct Session {
    vm: Vmm,
    breakpoints: Vec<u64>,
    step_cpu: Option<usize>,
    kernel: Option<vroom_kernel::windows::Image>,
    symbols: Option<vroom_kernel::symbols::Symbols>,
    cache: PathBuf,
    exports: PathBuf,
    continue_cpus: std::collections::VecDeque<usize>,
    continuing: bool,
    input: std::os::unix::net::UnixStream,
}

impl Session {
    fn configure(&self) -> Result<(), String> {
        for cpu in 0..self.vm.config().cpus as usize {
            self.vm
                .debug_configure(
                    cpu,
                    DebugConfig {
                        enabled: !self.breakpoints.is_empty(),
                        single_step: false,
                        breakpoints: self.breakpoints.clone(),
                        trap_software_breakpoints: false,
                    },
                )
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn command(&mut self, request: &Value) -> Result<Value, String> {
        let cpu = request.get("cpu").map(number).transpose()?.unwrap_or(0) as usize;
        let op = request["op"].as_str().ok_or("missing op")?;
        match op {
            "status" => Ok(
                json!({"state": format!("{:?}", self.vm.state()), "cpus": self.vm.config().cpus, "breakpoints": self.breakpoints}),
            ),
            "pause" => {
                self.continue_cpus.clear();
                self.continuing = false;
                self.vm.pause().map_err(|e| e.to_string())?;
                Ok(json!("paused"))
            }
            "resume" => {
                if self.vm.state() != RunState::Paused {
                    return Err("VM must be paused".into());
                }
                self.configure()?;
                self.continue_cpus.clear();
                for cpu in 0..self.vm.config().cpus as usize {
                    let state = self.vm.debug_registers(cpu).map_err(|e| e.to_string())?;
                    let regs = Registers::from_hvf(&state).map_err(|e| e.to_string())?;
                    if state.powered_on && self.breakpoints.contains(&regs.pc) {
                        self.continue_cpus.push_back(cpu);
                    }
                }
                self.continuing = true;
                self.continue_next()?;
                Ok(json!("running"))
            }
            "registers" => {
                let regs = self.vm.debug_registers(cpu).map_err(|e| e.to_string())?;
                Ok(json!(Registers::from_hvf(&regs).map_err(|e| e.to_string())?))
            }
            "process_event" => {
                let state = self.vm.debug_registers(cpu).map_err(|e| e.to_string())?;
                let regs = Registers::from_hvf(&state).map_err(|e| e.to_string())?;
                let space = AddressSpace {
                    memory: &self.vm,
                    registers: &regs,
                };
                Ok(json!(vroom_kernel::windows::process_event(&space, &regs.x)
                    .map_err(|e| e.to_string())?))
            }
            "regions" => Ok(json!(self
                .vm
                .debug_memory_regions()
                .map_err(|e| e.to_string())?)),
            "kernel" => {
                let state = self.vm.debug_registers(cpu).map_err(|e| e.to_string())?;
                let regs = Registers::from_hvf(&state).map_err(|e| e.to_string())?;
                let space = AddressSpace {
                    memory: &self.vm,
                    registers: &regs,
                };
                let image = vroom_kernel::windows::discover_kernel(&space, &[regs.vbar, regs.pc])
                    .map_err(|e| e.to_string())?;
                let result = json!({"base":image.base,"size":image.size,"name":image.name,"pdb":image.pdb,"export_count":image.exports.len()});
                self.kernel = Some(image);
                Ok(result)
            }
            "symbols" => {
                let image = self.kernel.as_ref().ok_or("discover kernel first")?;
                let identity = image
                    .pdb
                    .as_ref()
                    .ok_or("kernel has no CodeView PDB identity")?;
                let path = vroom_kernel::symbols::fetch(&self.cache, identity)
                    .map_err(|e| e.to_string())?;
                let symbols = vroom_kernel::symbols::Symbols::load(&path, identity, image.base)
                    .map_err(|e| e.to_string())?;
                let result = json!({"path":path,"count":symbols.names.len(),"types":symbols.types});
                self.symbols = Some(symbols);
                Ok(result)
            }
            "resolve" => {
                let name = request["name"].as_str().ok_or("missing name")?;
                if let Some(symbols) = &self.symbols {
                    return Ok(json!(symbols.resolve(name).map_err(|e| e.to_string())?));
                }
                self.kernel
                    .as_ref()
                    .and_then(|k| k.exports.get(name.strip_prefix("nt!").unwrap_or(name)))
                    .map(|&a| json!(a))
                    .ok_or_else(|| "symbol not found; load matching PDB symbols".into())
            }
            "export" => {
                let base = number(&request["base"])?;
                let state = self.vm.debug_registers(cpu).map_err(|e| e.to_string())?;
                let regs = Registers::from_hvf(&state).map_err(|e| e.to_string())?;
                let space = AddressSpace {
                    memory: &self.vm,
                    registers: &regs,
                };
                let image =
                    vroom_kernel::windows::Image::read(&space, base).map_err(|e| e.to_string())?;
                let partial = request["allow_missing"].as_bool().unwrap_or(false);
                let (bytes, missing) = image.export(&space, partial).map_err(|e| e.to_string())?;
                std::fs::create_dir_all(&self.exports).map_err(|e| e.to_string())?;
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|e| e.to_string())?
                    .as_nanos();
                let path = self.exports.join(format!("image-{base:016x}-{stamp}.exe"));
                let mut file = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&path)
                    .map_err(|e| e.to_string())?;
                file.write_all(&bytes).map_err(|e| e.to_string())?;
                let manifest = json!({"path":path,"image":image,"missing_page_rvas":missing,"layout":"reconstructed PE from paused guest memory; runtime image base; no relocations"});
                let metadata = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(path.with_extension("json"))
                    .map_err(|e| e.to_string())?;
                serde_json::to_writer_pretty(metadata, &manifest).map_err(|e| e.to_string())?;
                Ok(manifest)
            }
            "modules" | "callbacks" => {
                let symbols = self
                    .symbols
                    .as_ref()
                    .ok_or("load matching kernel symbols first")?;
                let head = symbols
                    .resolve("PsLoadedModuleList")
                    .map_err(|e| e.to_string())?;
                let layout = symbols.module_layout().map_err(|e| e.to_string())?;
                let state = self.vm.debug_registers(cpu).map_err(|e| e.to_string())?;
                let regs = Registers::from_hvf(&state).map_err(|e| e.to_string())?;
                let space = AddressSpace {
                    memory: &self.vm,
                    registers: &regs,
                };
                let modules = vroom_kernel::windows::modules(&space, head, &layout)
                    .map_err(|e| e.to_string())?;
                if op == "callbacks" {
                    return Ok(json!(vroom_kernel::windows::process_callbacks(
                        &space, symbols, &modules
                    )
                    .map_err(|e| e.to_string())?));
                }
                Ok(json!(modules))
            }
            "translate" | "read_virtual" | "read_physical" => {
                let address = number(&request["address"])?;
                let length = request
                    .get("length")
                    .map(number)
                    .transpose()?
                    .unwrap_or(256);
                if length > 65536 {
                    return Err("read limit is 64 KiB".into());
                }
                let mut data = vec![0; length as usize];
                if op == "read_physical" {
                    self.vm
                        .debug_read_physical(address, &mut data)
                        .map_err(|e| e.to_string())?;
                } else {
                    let state = self.vm.debug_registers(cpu).map_err(|e| e.to_string())?;
                    let regs = Registers::from_hvf(&state).map_err(|e| e.to_string())?;
                    let space = AddressSpace {
                        memory: &self.vm,
                        registers: &regs,
                    };
                    if op == "translate" {
                        return Ok(json!(space.translate(address).map_err(|e| e.to_string())?));
                    }
                    space.read(address, &mut data).map_err(|e| e.to_string())?;
                }
                Ok(
                    json!({"address": address, "hex": data.iter().map(|b| format!("{b:02x}")).collect::<String>()}),
                )
            }
            "breakpoints" => {
                if self.vm.state() != RunState::Paused {
                    return Err("pause before changing breakpoints".into());
                }
                let addresses = request["addresses"]
                    .as_array()
                    .ok_or("expected addresses array")?;
                let addresses: Vec<u64> = addresses.iter().map(number).collect::<Result<_, _>>()?;
                if addresses.len() > 4 || addresses.iter().any(|a| a & 3 != 0) {
                    return Err("at most four 4-byte-aligned addresses".into());
                }
                let old = std::mem::replace(&mut self.breakpoints, addresses);
                if let Err(e) = self.configure() {
                    self.breakpoints = old;
                    let rollback = self.configure();
                    return Err(format!("{e}; rollback: {rollback:?}; VM remains paused"));
                }
                Ok(json!(self.breakpoints))
            }
            "step" => {
                self.configure()?;
                // Temporarily remove breakpoints on the selected CPU so the
                // stopped instruction can execute; rearm them at the stop.
                self.vm
                    .debug_step(
                        cpu,
                        DebugConfig {
                            enabled: true,
                            ..Default::default()
                        },
                    )
                    .map_err(|e| e.to_string())?;
                self.step_cpu = Some(cpu);
                Ok(json!("stepping"))
            }
            "screen" => {
                let path = request["path"].as_str().ok_or("missing screenshot path")?;
                Ok(json!(self
                    .vm
                    .screendump(std::path::Path::new(path))
                    .map_err(|e| e.to_string())?))
            }
            "key" => {
                let code = number(&request["code"])?;
                if code > 767 {
                    return Err("invalid evdev key code".into());
                }
                let down = request["down"].as_bool().ok_or("missing down boolean")?;
                self.input
                    .write_all(
                        &vroom_display::ToVmm::Key {
                            code: code as u16,
                            down,
                        }
                        .encode(),
                    )
                    .map_err(|e| e.to_string())?;
                Ok(json!(true))
            }
            "checkpoint" => {
                let path = request["path"]
                    .as_str()
                    .ok_or("missing checkpoint directory")?;
                if self.vm.state() != RunState::Paused {
                    return Err("pause before saving a checkpoint".into());
                }
                for cpu in 0..self.vm.config().cpus as usize {
                    self.vm
                        .debug_configure(cpu, DebugConfig::default())
                        .map_err(|e| e.to_string())?;
                }
                let saved = self
                    .vm
                    .snapshot(std::path::Path::new(path), "kernel debugger checkpoint")
                    .map_err(|e| e.to_string());
                self.configure()?;
                saved?;
                Ok(json!("checkpoint saved"))
            }
            "shutdown" => {
                self.vm.shutdown();
                Ok(json!("stopped"))
            }
            _ => Err(format!("unknown command {op}")),
        }
    }

    fn continue_next(&mut self) -> Result<(), String> {
        if let Some(cpu) = self.continue_cpus.pop_front() {
            self.vm
                .debug_step(
                    cpu,
                    DebugConfig {
                        enabled: true,
                        ..Default::default()
                    },
                )
                .map_err(|e| e.to_string())?;
            self.step_cpu = Some(cpu);
        } else {
            self.continuing = false;
            self.vm.resume().map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

fn run() -> Result<(), String> {
    let args = Args::parse();
    let config = vroom_config::VmConfig::load(&args.config).map_err(|e| e.to_string())?;
    if config.nested {
        return Err("disable nested virtualization for kernel debugging".into());
    }
    let state_dir = config.default_state_dir(&args.config);
    std::fs::create_dir_all(&state_dir).map_err(|e| e.to_string())?;
    // Never send guest serial bytes into the debugger protocol.
    let serial = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir.join("lab-serial.log"))
        .map_err(|e| e.to_string())?;
    let hv = vroom_hv::default_hypervisor().map_err(|e| e.to_string())?;
    let cache = state_dir.join("symbols");
    let exports = state_dir.join("exports");
    // A small headless viewer keeps the ordinary display/input path available
    // to the lab without adding a second input implementation to the VMM.
    let (viewer, vmm_viewer) = std::os::unix::net::UnixStream::pair().map_err(|e| e.to_string())?;
    let input = viewer.try_clone().map_err(|e| e.to_string())?;
    let start = args
        .restore
        .map(StartMode::RestorePaused)
        .unwrap_or(StartMode::BootPaused);
    let mut vm = Vmm::start(
        config,
        hv,
        VmmOptions {
            serial_out: Box::new(serial),
            start,
            clipboard: None,
            ui: Some(vmm_viewer),
            state_dir,
        },
    )
    .map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        let mut viewer = viewer;
        if let Ok(_framebuffer) = vroom_display::recv_fd(&viewer) {
            let _ = std::io::copy(&mut viewer, &mut std::io::sink());
        }
    });
    vm.pause().map_err(|e| e.to_string())?;
    let mut session = Session {
        vm,
        breakpoints: Vec::new(),
        step_cpu: None,
        kernel: None,
        symbols: None,
        cache,
        exports,
        continue_cpus: Default::default(),
        continuing: false,
        input,
    };
    session.configure()?;
    output(json!({"event":"ready", "state":"Paused", "cpus": session.vm.config().cpus}))?;
    let (tx, rx) = mpsc::sync_channel(8);
    std::thread::spawn(move || {
        let mut input = std::io::stdin().lock();
        loop {
            let mut line = Vec::new();
            let n = std::io::Read::take(&mut input, 65537).read_until(b'\n', &mut line);
            match n {
                Ok(0) | Err(_) => break,
                Ok(_) if line.len() > 65536 => {
                    let _ = tx.send(Err("request exceeds 64 KiB".to_owned()));
                    break;
                }
                Ok(_) => {
                    if tx
                        .send(serde_json::from_slice::<Value>(&line).map_err(|e| e.to_string()))
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    loop {
        // Consume debug events first: RAM reads only happen after all-stop.
        while let Some(event) = session.vm.next_event(Duration::ZERO) {
            match event {
                VmEvent::Debug(stop) => {
                    session.vm.pause().map_err(|e| e.to_string())?;
                    session.configure()?;
                    session.step_cpu = None;
                    if session.continuing && stop.reason == vroom_hv::debug::DebugReason::SingleStep
                    {
                        session.continue_next()?;
                        continue;
                    }
                    session.continuing = false;
                    session.continue_cpus.clear();
                    output(json!({"event":"stopped", "cpu":stop.cpu, "pc":stop.pc,
                        "reason":format!("{:?}",stop.reason), "syndrome":stop.syndrome}))?;
                }
                VmEvent::Shutdown => {
                    session.vm.shutdown();
                    output(json!({"event":"shutdown"}))?;
                    return Ok(());
                }
                VmEvent::Crashed(e) => {
                    session.vm.pause().map_err(|e| e.to_string())?;
                    output(json!({"event":"fault", "message":e}))?;
                }
                VmEvent::Reset => {
                    session.vm.pause().map_err(|e| e.to_string())?;
                    output(
                        json!({"event":"reset_requested", "message":"guest requested reset; restart the lab to reboot"}),
                    )?;
                }
                _ => {}
            }
        }
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(Ok(request)) => {
                let id = request.get("id").cloned().unwrap_or(Value::Null);
                let response = match session.command(&request) {
                    Ok(result) => json!({"id":id,"result":result}),
                    Err(error) => json!({"id":id,"error":error}),
                };
                output(response)?;
                if session.vm.state() == RunState::Stopped {
                    return Ok(());
                }
            }
            Ok(Err(error)) => output(json!({"id":null,"error":error}))?,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("vroom-lab: {e}");
        std::process::exit(1);
    }
}

//! AMD64 lab driver. Shares the RPC protocol, but advertises only implemented ops.
use crate::{agent, guest, Args};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;
use vroom_hv::debug::{DebugConfig, DebugReason, Register, Watchpoint, X86Register};
use vroom_kernel::windows::Image;
use vroom_kernel::x86_64::{canonical, AddressSpace, Registers};
use vroom_kernel::{process, symbols::Symbols};
use vroom_vmm::{RunState, StartMode, VmEvent, Vmm, VmmOptions};

#[path = "x64_analysis.rs"]
pub(super) mod analysis;

#[path = "x64_memory.rs"]
mod memory_util;

#[path = "x64_step.rs"]
mod stepping;

pub(super) const OPS: &[&str] = &[
    "status",
    "pause",
    "restart",
    "snapshot",
    "snapshots",
    "restore",
    "resume",
    "step",
    "step_over",
    "step_out",
    "shutdown",
    "screen",
    "key",
    "registers",
    "write_register",
    "regions",
    "read_physical",
    "read_virtual",
    "translate",
    "write_physical",
    "write_virtual",
    "dump",
    "search",
    "breakpoints",
    "watchpoints",
    "kernel",
    "symbols",
    "functions",
    "resolve",
    "processes",
    "process",
    "modules",
    "peb",
    "user_modules",
    "callbacks",
    "stack",
    "image",
    "export",
    "process_event",
    "cache",
    "guest_exec",
    "guest_upload",
    "guest_download",
];

fn number(v: &Value) -> Result<u64, String> {
    if let Some(n) = v.as_u64() {
        return Ok(n);
    }
    if let Some(s) = v.as_str() {
        return if let Some(s) = s.strip_prefix("0x") {
            u64::from_str_radix(s, 16)
        } else {
            s.parse()
        }
        .map_err(|e| e.to_string());
    }
    Err("expected an unsigned integer or 0x address".into())
}
fn bytes(v: &Value) -> Result<Vec<u8>, String> {
    let text: String = v
        .as_str()
        .ok_or("missing hex")?
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    if !text.is_ascii() || text.len() % 2 != 0 || text.len() > 131072 {
        return Err("hex must contain at most 65536 complete bytes".into());
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}
fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}
fn string(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn sparse_read(
    address: u64,
    length: u64,
    mut read: impl FnMut(u64, &mut [u8]) -> Result<(), String>,
) -> Result<Value, String> {
    let end = address.checked_add(length).ok_or("range overflow")?;
    if length > 65536 {
        return Err("read limit is 64 KiB".into());
    }
    let mut at = address;
    let mut runs: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut holes = Vec::new();
    while at < end {
        let count = (4096 - (at & 4095)).min(end - at) as usize;
        let mut data = vec![0; count];
        if read(at, &mut data).is_ok() {
            match runs.last_mut() {
                Some((start, run)) if *start + run.len() as u64 == at => run.extend(data),
                _ => runs.push((at, data)),
            }
        } else {
            super::merge_hole(&mut holes, at, count as u64);
        }
        at += count as u64;
    }
    Ok(json!({"address":address,"length":length,
        "runs":runs.iter().map(|(at,data)|json!({"address":at,"hex":hex(data)})).collect::<Vec<_>>(),
        "holes":super::holes_json(&holes)}))
}

/// Start (or restore) the VM with the lab's headless display/input pair.
fn start_vm(
    config: vroom_config::VmConfig,
    state: &Path,
    start: StartMode,
) -> Result<(Vmm, UnixStream), String> {
    let serial = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state.join("lab-serial.log"))
        .map_err(string)?;
    let (viewer, vmm_viewer) = UnixStream::pair().map_err(string)?;
    let input = viewer.try_clone().map_err(string)?;
    let vm = Vmm::start(
        config,
        vroom_hv::default_hypervisor().map_err(string)?,
        VmmOptions {
            serial_out: Box::new(serial),
            start,
            clipboard: None,
            ui: Some(vmm_viewer),
            state_dir: state.to_path_buf(),
            sandbox: false,
        },
    )
    .map_err(string)?;
    std::thread::spawn(move || {
        let mut viewer = viewer;
        if let Ok(_fb) = vroom_display::recv_fd(&viewer) {
            let _ = std::io::copy(&mut viewer, &mut std::io::sink());
        }
    });
    Ok((vm, input))
}

struct Session {
    vm: Vmm,
    input: UnixStream,
    state: PathBuf,
    breakpoints: Vec<u64>,
    watches: Vec<Watchpoint>,
    stopped: BTreeMap<usize, DebugReason>,
    run_to: Option<stepping::RunTo>,
    kernel: Option<Image>,
    kernel_regs: Option<Registers>,
    symbols: BTreeMap<u64, Symbols>,
    selected: Option<process::Process>,
}
impl Session {
    fn registers(&self, cpu: usize, context: bool) -> Result<Registers, String> {
        let mut r = Registers::from_state(&self.vm.debug_registers(cpu).map_err(string)?)
            .map_err(string)?;
        if context {
            if let Some(p) = &self.selected {
                r.sregs.cr3 = p.directory_table;
            }
        }
        Ok(r)
    }
    fn debug(&self, points: &[u64], watches: &[Watchpoint]) -> Result<(), String> {
        let mut points = points.to_vec();
        if let Some(run) = &self.run_to {
            if !points.contains(&run.address) {
                points.push(run.address);
            }
        }
        let config = DebugConfig {
            enabled: !points.is_empty() || !watches.is_empty(),
            breakpoints: points.to_vec(),
            watchpoints: watches.to_vec(),
            ..Default::default()
        };
        for cpu in 0..self.vm.config().cpus as usize {
            if let Err(e) = self.vm.debug_configure(cpu, config.clone()) {
                let mut errors = Vec::new();
                for done in 0..cpu {
                    let (breakpoints, watchpoints) = (self.breakpoints.clone(), self.watches.clone());
                    if let Err(e) = self.vm.debug_configure(
                        done,
                        DebugConfig {
                            enabled: !breakpoints.is_empty() || !watchpoints.is_empty(),
                            breakpoints,
                            watchpoints,
                            ..Default::default()
                        },
                    ) {
                        errors.push(e.to_string());
                    }
                }
                return Err(format!(
                    "{e}; configuration rollback errors: {errors:?}; VM remains paused"
                ));
            }
        }
        Ok(())
    }
    fn snapshot_dir(&self, q: &Value) -> Result<PathBuf, String> {
        match (q["path"].as_str(), q["name"].as_str()) {
            (Some(p), _) => Ok(PathBuf::from(p)),
            (None, Some(n)) if !n.is_empty() && !n.contains('/') && !n.starts_with('.') => {
                Ok(self.state.join("snapshots").join(n))
            }
            (None, Some(n)) => Err(format!("bad snapshot name {n:?}")),
            (None, None) => {
                let t = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                Ok(self.state.join("snapshots").join(format!("snap-{t}")))
            }
        }
    }

    fn snapshot(&mut self, q: &Value) -> Result<Value, String> {
        let dir = self.snapshot_dir(q)?;
        if dir.exists() {
            return Err(format!("{} exists already", dir.display()));
        }
        let running = self.vm.state() == RunState::Running;
        if running {
            self.vm.pause().map_err(string)?;
        }
        if self.pending_step_events()? {
            return Err("pending VM stops were reported; inspect them and retry snapshot (VM remains paused)".into());
        }
        // Saved without the debugger's traps.
        for cpu in 0..self.vm.config().cpus as usize {
            self.vm
                .debug_configure(cpu, DebugConfig::default())
                .map_err(string)?;
        }
        let saved = self
            .vm
            .snapshot(&dir, q["description"].as_str().unwrap_or("vroom-lab"))
            .map_err(string);
        let rearmed = self.debug(&self.breakpoints, &self.watches);
        rearmed?;
        if running {
            self.vm.resume().map_err(string)?;
        }
        saved?;
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(json!({"name":name,"path":dir}))
    }

    fn snapshots(&self) -> Result<Value, String> {
        let list = vroom_state::list(&self.state).map_err(string)?;
        Ok(Value::Array(
            list.into_iter()
                .map(|s| {
                    json!({"name":s.name,"path":s.path,"created_unix":s.created_unix,
                           "description":s.description,"bytes":s.bytes})
                })
                .collect(),
        ))
    }

    fn restore_plan(&self, q: &Value) -> Result<(PathBuf, bool), String> {
        let given = q["name"]
            .as_str()
            .or(q["path"].as_str())
            .ok_or("missing name (or path)")?;
        let resume = match q.get("resume").filter(|v| !v.is_null()) {
            Some(v) => v.as_bool().ok_or("resume: expected a boolean")?,
            None => false,
        };
        let dir = super::restore_dir(Path::new(given), &self.state)?;
        vroom_vmm::check_snapshot(&dir).map_err(string)?;
        Ok((dir, resume))
    }

    /// Replace this VM with a fresh one restored from `dir`. On success the
    /// old VMM is dropped, releasing its state lock, disks and network ports.
    fn restore(
        self,
        config: vroom_config::VmConfig,
        dir: PathBuf,
        resume: bool,
    ) -> Result<(Session, Value), String> {
        let state = self.state.clone();
        let mut old = self;
        old.vm.shutdown();
        drop(old);
        let (vm, input) = start_vm(config, &state, StartMode::RestorePaused(dir.clone()))?;
        let mut session = Session {
            vm,
            input,
            state,
            breakpoints: vec![],
            watches: vec![],
            stopped: BTreeMap::new(),
            run_to: None,
            kernel: None,
            kernel_regs: None,
            symbols: BTreeMap::new(),
            selected: None,
        };
        session.vm.pause().map_err(string)?;
        let mut reply = json!({"restored": dir, "state": "Paused",
            "note": "paused: vm.resume runs the guest (or restore with resume: true)"});
        if resume {
            session.command(&json!({"op":"resume"}))?;
            reply["state"] = json!("Running");
            reply.as_object_mut().expect("object").remove("note");
        }
        Ok((session, reply))
    }

    fn find_kernel(&mut self, cpu: usize) -> Result<(), String> {
        if self.kernel.is_some() {
            return Ok(());
        }
        let mut cpus = vec![cpu];
        cpus.extend((0..self.vm.config().cpus as usize).filter(|&n| n != cpu));
        for cpu in cpus {
            let r = self.registers(cpu, false)?;
            if r.sregs.cr0 & (1 << 31) == 0 || r.sregs.efer & (1 << 10) == 0 {
                continue;
            }
            let space = AddressSpace {
                memory: &self.vm,
                registers: &r,
            };
            let mut anchors = Vec::new();
            if let Some(lstar) = r.msr(0xc000_0082).filter(|&v| v >> 63 != 0 && canonical(v)) {
                anchors.push(lstar);
            }
            if r.regs.rip >> 63 != 0 && canonical(r.regs.rip) {
                anchors.push(r.regs.rip);
            }
            let mut gate = [0; 16];
            if space.read(r.sregs.idt.base, &mut gate).is_ok() {
                let target = u16::from_le_bytes([gate[0], gate[1]]) as u64
                    | ((u16::from_le_bytes([gate[6], gate[7]]) as u64) << 16)
                    | ((u32::from_le_bytes(gate[8..12].try_into().unwrap()) as u64) << 32);
                if canonical(target) && target >> 63 != 0 {
                    anchors.push(target);
                }
            }
            for anchor in anchors {
                let top = anchor & !0xffff;
                for distance in 0..1024u64 {
                    let Some(base) = top.checked_sub(distance * 65536) else {
                        break;
                    };
                    let mut magic = [0; 2];
                    if space.read(base, &mut magic).is_err() || magic != *b"MZ" {
                        continue;
                    }
                    let Ok(image) = Image::read_amd64(&space, base) else {
                        continue;
                    };
                    if image.name.eq_ignore_ascii_case("ntoskrnl.exe")
                        || image
                            .pdb
                            .as_ref()
                            .is_some_and(|p| p.name.eq_ignore_ascii_case("ntkrnlmp.pdb"))
                    {
                        if anchor < base || anchor - base >= image.size as u64 {
                            continue;
                        }
                        self.kernel = Some(image);
                        self.kernel_regs = Some(r);
                        return Ok(());
                    }
                }
            }
        }
        Err("kernel not mapped in the stopped CPU contexts yet; resume Windows, pause, then retry (KPTI user contexts may not map ntoskrnl)".into())
    }
    fn kernel_registers(&self) -> Result<&Registers, String> {
        self.kernel_regs
            .as_ref()
            .ok_or_else(|| "call kernel.info first".into())
    }
    fn kernel_symbols(&self) -> Result<&Symbols, String> {
        self.symbols
            .get(&self.kernel.as_ref().ok_or("call kernel.info first")?.base)
            .ok_or_else(|| "call kernel.symbols first".into())
    }
    fn processes(&mut self, cpu: usize) -> Result<Vec<process::Process>, String> {
        self.find_kernel(cpu)?;
        let space = AddressSpace {
            memory: &self.vm,
            registers: self.kernel_registers()?,
        };
        process::list(&space, self.kernel_symbols()?).map_err(string)
    }
    fn command(&mut self, q: &Value) -> Result<Value, String> {
        let op = q["op"].as_str().ok_or("missing op")?;
        let cpu = q.get("cpu").map(number).transpose()?.unwrap_or(0) as usize;
        if cpu >= self.vm.config().cpus as usize {
            return Err("invalid CPU index".into());
        }
        if self.run_to.is_some()
            && matches!(
                op,
                "pause"
                    | "resume"
                    | "step"
                    | "step_over"
                    | "step_out"
                    | "restart"
                    | "snapshot"
                    | "shutdown"
            )
        {
            self.vm.pause().map_err(string)?;
            self.cancel_step()?;
        }
        match op {
            "status" => {
                let agent = self.vm.agent().status();
                return Ok(
                    json!({"state":format!("{:?}",self.vm.state()),"architecture":"x86_64","cpus":self.vm.config().cpus,
                "guest_agent":{"connected":agent.connected,"version":agent.agent,"features":agent.features},"breakpoints":self.breakpoints,"watchpoints":self.watches.len(),
                "note":"hypervisor debugger; no guest driver or Windows debug boot required"}),
                );
            }
            "pause" => {
                self.vm.pause().map_err(string)?;
                return Ok(json!({"state":"Paused"}));
            }
            "restart" => {
                if self.vm.state() == RunState::Stopped {
                    return Err("the VM is stopped; start a new lab".into());
                }
                let paused = match q.get("paused").filter(|v| !v.is_null()) {
                    Some(v) => v.as_bool().ok_or("paused: expected a boolean")?,
                    None => false,
                };
                // Pause first: Vmm::reset resumes by itself when it finds the VM
                // running, which would execute boot code before we clear state.
                self.vm.pause().map_err(string)?;
                self.pending_step_events()?;
                self.vm.reset().map_err(string)?;
                self.breakpoints.clear();
                self.watches.clear();
                self.stopped.clear();
                self.kernel = None;
                self.kernel_regs = None;
                self.symbols.clear();
                self.selected = None;
                self.debug(&self.breakpoints, &self.watches)?;
                if !paused {
                    self.vm.resume().map_err(string)?;
                }
                return Ok(json!({"state":if paused {"Paused"} else {"Running"}}));
            }
            "snapshot" => return self.snapshot(q),
            "snapshots" => return self.snapshots(),
            "resume" => {
                self.vm.pause().map_err(string)?;
                self.debug(&self.breakpoints, &self.watches)?;
                for (&cpu, &reason) in &self.stopped {
                    if reason == DebugReason::Breakpoint {
                        let r = self.registers(cpu, false)?;
                        self.vm
                            .debug_write_register(
                                cpu,
                                Register::X86(X86Register::Rflags),
                                r.regs.rflags | (1 << 16),
                            )
                            .map_err(string)?;
                    }
                }
                self.stopped.clear();
                self.vm.resume().map_err(string)?;
                return Ok(json!({"state":"Running"}));
            }
            "step" => {
                self.vm.pause().map_err(string)?;
                if self.pending_step_events()? {
                    return Err(
                        "pending VM stops were reported; inspect them and retry stepping".into(),
                    );
                }
                self.vm
                    .debug_step(cpu, DebugConfig::default())
                    .map_err(string)?;
                self.stopped.remove(&cpu);
                return Ok(json!({"state":"Running","note":"poll events.poll for SingleStep"}));
            }
            "shutdown" => {
                self.vm.shutdown();
                return Ok(json!({"state":"Stopped"}));
            }
            "screen" => {
                // Untrusted remote paths are confined to this lab's export directory.
                let name = q["path"].as_str().ok_or("missing screenshot path")?;
                let name = std::path::Path::new(name);
                if name.components().count() != 1
                    || !matches!(
                        name.components().next(),
                        Some(std::path::Component::Normal(_))
                    )
                {
                    return Err("screenshot path must be a single filename under the lab's exports directory".into());
                }
                let path = self.state.join("exports").join(name);
                let mode = self.vm.screendump(&path).map_err(string)?;
                return Ok(json!({"path":path,"width":mode.0,"height":mode.1}));
            }
            "key" => {
                let code = number(&q["code"])?;
                if code > 767 {
                    return Err("invalid evdev code".into());
                }
                let down = q["down"].as_bool().ok_or("missing down boolean")?;
                self.input
                    .write_all(
                        &vroom_display::ToVmm::Key {
                            code: code as u16,
                            down,
                        }
                        .encode(),
                    )
                    .map_err(string)?;
                return Ok(json!(true));
            }
            _ => {}
        }
        if self.vm.state() != RunState::Paused {
            return Err("pause every CPU before inspecting or modifying the guest".into());
        }
        if matches!(
            op,
            "breakpoints" | "watchpoints" | "write_register" | "write_virtual" | "write_physical"
        ) {
            if self.pending_step_events()? {
                return Err(
                    "pending VM stops were reported; inspect them and retry the mutation".into(),
                );
            }
        }
        match op {
            "callbacks" | "stack" | "image" | "export" | "process_event" | "cache" => {
                self.analysis_command(op, cpu, q)
            }
            "registers" => Ok(json!(self.registers(cpu, false)?)),
            "step_over" | "step_out" => self.step_return(cpu, op == "step_over", q),
            "dump" => self.memory_dump(cpu, q),
            "search" => self.memory_search(cpu, q),
            "write_register" => {
                let name = q["register"].as_str().ok_or("missing register")?;
                let register =
                    Register::X86(X86Register::parse(name).ok_or("an AMD64 register is required")?);
                self.vm
                    .debug_write_register(cpu, register, number(&q["value"])?)
                    .map_err(string)?;
                Ok(json!(true))
            }
            "regions" => Ok(json!(self.vm.debug_memory_regions().map_err(string)?)),
            "read_physical" | "read_virtual" | "translate" => {
                let address = number(&q["address"])?;
                let n = q.get("length").map(number).transpose()?.unwrap_or(256);
                if n > 65536 {
                    return Err("length is limited to 65536 bytes".into());
                }
                let r = self.registers(cpu, q["context"].as_bool().unwrap_or(true))?;
                let s = AddressSpace {
                    memory: &self.vm,
                    registers: &r,
                };
                if op == "translate" {
                    return Ok(json!(s.translate(address).map_err(string)?));
                }
                if op == "read_virtual" && q["skip_unmapped"].as_bool() == Some(true) {
                    return sparse_read(address, n, |at, data| s.read(at, data).map_err(string));
                }
                let mut data = vec![0; n as usize];
                if op == "read_physical" {
                    self.vm
                        .debug_read_physical(address, &mut data)
                        .map_err(string)?;
                } else {
                    s.read(address, &mut data).map_err(string)?;
                }
                Ok(json!({"address":address,"hex":hex(&data)}))
            }
            "write_physical" | "write_virtual" => {
                let address = number(&q["address"])?;
                let data = bytes(&q["hex"])?;
                address
                    .checked_add(data.len() as u64)
                    .ok_or("write range overflow")?;
                if op == "write_physical" {
                    self.vm
                        .debug_write_physical(&[(address, &data)])
                        .map_err(string)?;
                } else {
                    let r = self.registers(cpu, q["context"].as_bool().unwrap_or(true))?;
                    let s = AddressSpace {
                        memory: &self.vm,
                        registers: &r,
                    };
                    let mut spans = Vec::new();
                    let mut offset = 0;
                    while offset < data.len() {
                        let at = address + offset as u64;
                        let n = (4096 - (at & 4095) as usize).min(data.len() - offset);
                        let t = s.translate(at).map_err(string)?;
                        spans.push((t.physical, &data[offset..offset + n]));
                        offset += n;
                    }
                    self.vm.debug_write_physical(&spans).map_err(string)?;
                }
                Ok(json!({"written":data.len()}))
            }
            "breakpoints" => {
                let points: Vec<u64> = q["addresses"]
                    .as_array()
                    .ok_or("missing addresses")?
                    .iter()
                    .map(number)
                    .collect::<Result<_, _>>()?;
                if points.iter().any(|&p| !canonical(p)) {
                    return Err("noncanonical breakpoint address".into());
                }
                self.debug(&points, &self.watches)?;
                self.breakpoints = points;
                Ok(json!({"addresses":self.breakpoints}))
            }
            "watchpoints" => {
                let mut watches = Vec::new();
                for w in q["watches"].as_array().ok_or("missing watches")? {
                    let mode = w["mode"].as_str().ok_or("missing mode: w or rw")?;
                    if !matches!(mode, "w" | "rw") {
                        return Err("AMD64 supports w or rw data watches, not read-only".into());
                    }
                    let size = number(&w["size"])?;
                    if !matches!(size, 1 | 2 | 4 | 8) {
                        return Err("watchpoint size must be 1/2/4/8".into());
                    }
                    let address = number(&w["address"])?;
                    if !canonical(address) {
                        return Err("noncanonical watchpoint".into());
                    }
                    watches.push(Watchpoint {
                        address,
                        size: size as u8,
                        read: mode == "rw",
                        write: true,
                    });
                }
                self.debug(&self.breakpoints, &watches)?;
                self.watches = watches;
                Ok(json!({"count":self.watches.len()}))
            }
            "kernel" => {
                self.find_kernel(cpu)?;
                let k = self.kernel.as_ref().unwrap();
                Ok(
                    json!({"architecture":"AMD64","base":k.base,"size":k.size,"name":k.name,"pdb":k.pdb,"export_count":k.exports.len()}),
                )
            }
            "symbols" => {
                self.find_kernel(cpu)?;
                let base = q
                    .get("base")
                    .map(number)
                    .transpose()?
                    .unwrap_or(self.kernel.as_ref().unwrap().base);
                let r = self.kernel_registers()?;
                let s = AddressSpace {
                    memory: &self.vm,
                    registers: r,
                };
                let image = Image::read_amd64(&s, base).map_err(string)?;
                let identity = image.pdb.as_ref().ok_or("image has no PDB identity")?;
                let path = vroom_kernel::symbols::fetch(&self.state.join("symbols"), identity)
                    .map_err(string)?;
                let symbols = Symbols::load(&path, identity, base).map_err(string)?;
                let result = json!({"base":base,"path":path,"functions":symbols.functions.len(),"types":symbols.types.len()});
                self.symbols.insert(base, symbols);
                // Keep a stable kernel CR3: the current process can exit after resume.
                if base == self.kernel.as_ref().unwrap().base {
                    if let Ok(all) = self.processes(cpu) {
                        if let Some(system) = all.into_iter().find(|p| p.pid == 4) {
                            if let Some(r) = self.kernel_regs.as_mut() {
                                r.sregs.cr3 = system.directory_table;
                            }
                        }
                    }
                }
                Ok(result)
            }
            "resolve" | "functions" => {
                let base = q
                    .get("base")
                    .map(number)
                    .transpose()?
                    .unwrap_or(self.kernel.as_ref().ok_or("call kernel.info first")?.base);
                let symbols = self.symbols.get(&base).ok_or("call kernel.symbols first")?;
                if op == "resolve" {
                    let name = q["name"]
                        .as_str()
                        .ok_or("missing name")?
                        .split('!')
                        .next_back()
                        .unwrap();
                    Ok(json!({"name":name,"address":symbols.resolve(name).map_err(string)?}))
                } else {
                    let prefix = q["prefix"].as_str().unwrap_or("");
                    Ok(json!(symbols
                        .functions
                        .iter()
                        .filter(|(name, _)| name.starts_with(prefix))
                        .map(|(name, address)| json!({"name":name,"address":address}))
                        .collect::<Vec<_>>()))
                }
            }
            "processes" => Ok(json!(self.processes(cpu)?)),
            "process" => {
                self.selected = match q.get("pid").filter(|v| !v.is_null()) {
                    Some(pid) => {
                        let pid = number(pid)?;
                        Some(
                            self.processes(cpu)?
                                .into_iter()
                                .find(|p| p.pid == pid)
                                .ok_or("process not found")?,
                        )
                    }
                    None => None,
                };
                Ok(json!(self.selected))
            }
            "modules" => {
                self.find_kernel(cpu)?;
                let s = AddressSpace {
                    memory: &self.vm,
                    registers: self.kernel_registers()?,
                };
                let symbols = self.kernel_symbols()?;
                Ok(json!(vroom_kernel::windows::modules(
                    &s,
                    symbols.resolve("PsLoadedModuleList").map_err(string)?,
                    &symbols.module_layout().map_err(string)?
                )
                .map_err(string)?))
            }
            "peb" | "user_modules" => {
                let pid = number(&q["pid"])?;
                let p = self
                    .processes(cpu)?
                    .into_iter()
                    .find(|p| p.pid == pid)
                    .ok_or("process not found")?;
                let mut r = self.kernel_registers()?.clone();
                r.sregs.cr3 = p.directory_table;
                let s = AddressSpace {
                    memory: &self.vm,
                    registers: &r,
                };
                let symbols = self.kernel_symbols()?;
                if op == "peb" {
                    Ok(json!(process::peb(&s, symbols, &p).map_err(string)?))
                } else {
                    Ok(json!(process::modules(&s, symbols, &p).map_err(string)?))
                }
            }
            _ => Err(format!("unsupported AMD64 operation {op}")),
        }
    }
}

pub(super) fn run(args: Args) -> Result<(), String> {
    let config = vroom_config::VmConfig::load(&args.config).map_err(string)?;
    if config.nested {
        return Err("disable nested virtualization for kernel debugging".into());
    }
    let state = config.default_state_dir(&args.config);
    std::fs::create_dir_all(state.join("exports")).map_err(string)?;
    let start = args
        .restore
        .as_ref()
        .map(|p| super::restore_dir(p, &state).map(StartMode::RestorePaused))
        .transpose()?
        .unwrap_or(StartMode::BootPaused);
    let (vm, input) = start_vm(config.clone(), &state, start)?;
    let mut session = Session {
        vm,
        input,
        state,
        breakpoints: vec![],
        watches: vec![],
        stopped: BTreeMap::new(),
        run_to: None,
        kernel: None,
        kernel_regs: None,
        symbols: BTreeMap::new(),
        selected: None,
    };
    session.vm.pause().map_err(string)?;
    let (tx, rx) = mpsc::channel();
    let endpoint = args
        .agent_socket
        .as_ref()
        .map(|path| agent::start_supported(path, tx.clone(), Some(OPS)))
        .transpose()?;
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: the handler only stores an atomic flag.
        unsafe {
            libc::signal(
                signal,
                super::on_stop_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
            );
        }
    }
    let (stdio, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if stdio.send(line).is_err() {
                break;
            }
        }
    });
    super::output(
        json!({"event":"ready","architecture":"x86_64","state":"Paused","cpus":session.vm.config().cpus}),
    )?;
    super::report_ready();
    while !super::STOP.load(std::sync::atomic::Ordering::Acquire)
        && session.vm.state() != RunState::Stopped
    {
        // Busy automatic tracepoints must not starve pause/shutdown RPCs.
        for _ in 0..64 {
            let Some(event) = session.vm.next_event(Duration::ZERO) else {
                break;
            };
            match event {
                VmEvent::Debug(stop) => {
                    session.vm.pause().map_err(string)?;
                    if session.run_to.is_some() {
                        let Some(reason) = session.step_stopped(&stop)? else {
                            continue;
                        };
                        super::output(
                            json!({"event":"stopped","cpu":stop.cpu,"pc":stop.pc,"reason":reason,"syndrome":stop.syndrome}),
                        )?;
                        continue;
                    }
                    // Disarm single-step after its first exit; reapply persistent slots.
                    session.debug(&session.breakpoints, &session.watches)?;
                    session.stopped.insert(stop.cpu as usize, stop.reason);
                    super::output(
                        json!({"event":"stopped","cpu":stop.cpu,"pc":stop.pc,"reason":format!("{:?}",stop.reason),"syndrome":stop.syndrome}),
                    )?;
                }
                VmEvent::Shutdown | VmEvent::Reset => {
                    session.vm.shutdown();
                    super::output(json!({"event":"shutdown","note":format!("{event:?}")}))?;
                }
                VmEvent::Crashed(error) => {
                    return Err(format!("guest: {error}"));
                }
                other => {
                    super::output(json!({"event":"vm","message":format!("{other:?}")}))?;
                }
            }
        }
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(f) => {
                let op = f.request["op"].as_str().unwrap_or_default();
                if op.starts_with("guest_") {
                    guest::run_remote(&session.vm, &session.state.join("exports"), f);
                } else if op == "restore" {
                    match session.restore_plan(&f.request) {
                        Ok((dir, resume)) => match session.restore(config.clone(), dir, resume) {
                            Ok((new_session, reply)) => {
                                session = new_session;
                                let _ = f.reply.send(Ok(reply));
                            }
                            Err(e) => {
                                let _ = f.reply.send(Err(e.clone()));
                                return Err(e);
                            }
                        },
                        Err(e) => {
                            let _ = f.reply.send(Err(e));
                        }
                    }
                } else {
                    let result = session.command(&f.request);
                    let _ = f.reply.send(result);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) if endpoint.is_none() => break,
            Err(_) => {}
        }
        for _ in 0..64 {
            let Ok(line) = lines.try_recv() else { break };
            let result = match serde_json::from_str::<Value>(&line).map_err(string) {
                Ok(q) if q["op"] == "restore" => match session.restore_plan(&q) {
                    Ok((dir, resume)) => match session.restore(config.clone(), dir, resume) {
                        Ok((new_session, reply)) => {
                            session = new_session;
                            Ok(reply)
                        }
                        Err(e) => {
                            super::output(json!({"error":e}))?;
                            return Err(e);
                        }
                    },
                    Err(e) => Err(e),
                },
                Ok(q) => session.command(&q),
                Err(e) => Err(e),
            };
            super::output(match result {
                Ok(v) => json!({"result":v}),
                Err(e) => json!({"error":e}),
            })?;
        }
    }
    session.vm.shutdown();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_hex_and_numbers() {
        assert_eq!(bytes(&json!("ab cd")).unwrap(), [0xab, 0xcd]);
        for invalid in ["abc", "💥", "xx"] {
            assert!(bytes(&json!(invalid)).is_err());
        }
        assert_eq!(number(&json!("0xffff")), Ok(65535));
        assert!(number(&json!(-1)).is_err());
    }

    #[test]
    fn sparse_reads_merge_adjacent_pages_and_report_request_range() {
        let result = sparse_read(4094, 12291, |at, data| {
            data.fill(0xab);
            if (8192..16384).contains(&at) {
                Err("unmapped".into())
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(result["address"], 4094);
        assert_eq!(result["length"], 12291);
        assert_eq!(
            result["runs"],
            json!([
                {"address":4094,"hex":"ab".repeat(4098)},
                {"address":16384,"hex":"ab"}
            ])
        );
        assert_eq!(result["holes"], json!([{"address":8192,"length":8192}]));
        assert!(sparse_read(u64::MAX, 1, |_, _| Ok(())).is_err());
    }
}

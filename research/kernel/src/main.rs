//! Line-delimited JSON control endpoint; the Textual frontend owns this process.
//! `--agent-socket` also serves agents (JSON-RPC 2.0, see agent.rs).
mod agent;

use clap::Parser;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;
use vroom_hv::debug::{DebugConfig, Watchpoint};
use vroom_kernel::arm64::{AddressSpace, Registers};
use vroom_kernel::cache::{CachedMemory, PageCache};
use vroom_kernel::{process, symbols::Symbols, windows::Image};
use vroom_vmm::{RunState, StartMode, VmEvent, Vmm, VmmOptions};

#[derive(Debug, Parser)]
#[command(about = "Windows ARM64 kernel debugger backend (JSON lines on stdio)")]
struct Args {
    /// A disposable vroom VM config. Boots paused; resume explicitly.
    config: PathBuf,
    /// Restore a vroom checkpoint instead of cold booting.
    #[arg(long)]
    restore: Option<PathBuf>,
    /// Also serve agents (JSON-RPC 2.0, one object per line) on this
    /// owner-only Unix socket. The lab then keeps running when stdin closes.
    #[arg(long)]
    agent_socket: Option<PathBuf>,
}

/// A request from the frontend on stdin, or from an agent.
enum Input {
    Stdio(Result<Value, String>),
    Agent(agent::Forward),
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

fn vector_length(request: &Value) -> Result<Option<u16>, String> {
    request
        .get("vector_length")
        .map(|v| {
            let n = number(v)?;
            if !(16..=256).contains(&n) || n % 16 != 0 {
                return Err("vector_length: 16..256 bytes, multiple of 16".into());
            }
            Ok(n as u16)
        })
        .transpose()
}

fn output(v: Value) -> Result<(), String> {
    if v.get("event").is_some() {
        agent::record_event(&v);
    }
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &v).map_err(|e| e.to_string())?;
    writeln!(out)
        .and_then(|_| out.flush())
        .map_err(|e| e.to_string())
}

struct Session {
    vm: Vmm,
    breakpoints: Vec<u64>,
    breakpoint_contexts: BTreeMap<u64, process::Process>,
    watchpoints: Vec<Watchpoint>,
    watch_stopped: Option<usize>,
    step_cpu: Option<usize>,
    run_to: Option<vroom_kernel::control::RunTo>,
    kernel: Option<vroom_kernel::windows::Image>,
    symbols: Option<vroom_kernel::symbols::Symbols>,
    pages: PageCache,
    process_cache: std::cell::RefCell<Option<Vec<process::Process>>>,
    module_cache: std::cell::RefCell<Option<Vec<vroom_kernel::windows::Module>>>,
    unwind: vroom_kernel::unwind::Tables,
    export_cache: BTreeMap<String, Value>,
    local_images: BTreeMap<(u64, u64), PathBuf>,
    process: Option<process::Process>,
    module_symbols: BTreeMap<(u64, u64), Symbols>,
    cache: PathBuf,
    exports: PathBuf,
    continue_cpus: std::collections::VecDeque<usize>,
    continuing: bool,
    input: std::os::unix::net::UnixStream,
}

impl Session {
    fn registers(&self, cpu: usize, context: bool) -> Result<Registers, String> {
        let state = self.vm.debug_registers(cpu).map_err(|e| e.to_string())?;
        let mut regs = Registers::from_state(&state).map_err(|e| e.to_string())?;
        if context {
            if let Some(process) = &self.process {
                let active = self
                    .processes(cpu)?
                    .into_iter()
                    .find(|p| p.eprocess == process.eprocess && p.pid == process.pid)
                    .ok_or("selected process exited; select a live process")?;
                regs.ttbr0 = active.directory_table;
                regs.tcr &= !(1 << 7); // Inspection may walk a stopped EL1 context's TTBR0.
            }
        }
        Ok(regs)
    }
    fn processes(&self, cpu: usize) -> Result<Vec<process::Process>, String> {
        if let Some(cached) = self.process_cache.borrow().as_ref() {
            return Ok(cached.clone());
        }
        let regs = self.registers(cpu, false)?;
        let space = AddressSpace {
            memory: &CachedMemory {
                memory: &self.vm,
                cache: &self.pages,
            },
            registers: &regs,
        };
        let symbols = self.symbols.as_ref().ok_or("load kernel symbols first")?;
        let rows = process::list(&space, symbols).map_err(|e| e.to_string())?;
        *self.process_cache.borrow_mut() = Some(rows.clone());
        Ok(rows)
    }
    fn kernel_modules(&self, cpu: usize) -> Result<Vec<vroom_kernel::windows::Module>, String> {
        if let Some(cached) = self.module_cache.borrow().as_ref() {
            return Ok(cached.clone());
        }
        let regs = self.registers(cpu, false)?;
        let space = AddressSpace {
            memory: &CachedMemory {
                memory: &self.vm,
                cache: &self.pages,
            },
            registers: &regs,
        };
        let symbols = self.symbols.as_ref().ok_or("load kernel symbols first")?;
        let rows = vroom_kernel::windows::modules(
            &space,
            symbols
                .resolve("PsLoadedModuleList")
                .map_err(|e| e.to_string())?,
            &symbols.module_layout().map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        *self.module_cache.borrow_mut() = Some(rows.clone());
        Ok(rows)
    }
    fn selected_symbols(&self, base: Option<u64>) -> Result<&Symbols, String> {
        match base {
            None => self.symbols.as_ref(),
            Some(base) if self.kernel.as_ref().is_some_and(|k| k.base == base) => {
                self.symbols.as_ref()
            }
            Some(base) => self.module_symbols.get(&(
                if base >> 63 != 0 {
                    0
                } else {
                    self.process.as_ref().map_or(0, |p| p.eprocess)
                },
                base,
            )),
        }
        .ok_or_else(|| "load matching symbols first".into())
    }

    fn invalidate(&mut self) {
        self.pages.clear();
        self.process_cache.borrow_mut().take();
        self.module_cache.borrow_mut().take();
        self.unwind.clear();
        self.export_cache.clear();
        self.module_symbols.clear();
    }
    fn execution_breakpoints(&self) -> Vec<u64> {
        let mut addresses = self.breakpoints.clone();
        if let Some(run) = &self.run_to {
            if !addresses.contains(&run.address) {
                addresses.push(run.address);
            }
        }
        addresses
    }
    fn cancel_run_to(&mut self) -> Result<(), String> {
        self.run_to = None;
        self.configure()
    }
    fn configure(&self) -> Result<(), String> {
        let addresses = self.execution_breakpoints();
        for cpu in 0..self.vm.config().cpus as usize {
            self.vm
                .debug_configure(
                    cpu,
                    DebugConfig {
                        enabled: !addresses.is_empty() || !self.watchpoints.is_empty(),
                        single_step: false,
                        breakpoints: addresses.clone(),
                        watchpoints: self.watchpoints.clone(),
                        trap_software_breakpoints: false,
                        trap_brk_immediate: None,
                    },
                )
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn command(&mut self, request: &Value) -> Result<Value, String> {
        let cpu = request.get("cpu").map(number).transpose()?.unwrap_or(0) as usize;
        let op = request["op"].as_str().ok_or("missing op")?;
        if !matches!(op, "status" | "pause" | "shutdown" | "key")
            && self.vm.state() != RunState::Paused
        {
            return Err("pause the VM before inspection or control".into());
        }
        match op {
            "status" => Ok(
                json!({"state": format!("{:?}", self.vm.state()), "cpus": self.vm.config().cpus, "breakpoints": self.breakpoints, "temporary":self.run_to.as_ref().map(|r|r.address)}),
            ),
            "pause" => {
                self.continue_cpus.clear();
                self.continuing = false;
                self.vm.pause().map_err(|e| e.to_string())?;
                self.cancel_run_to()?;
                Ok(json!("paused"))
            }
            "resume" => {
                if self.vm.state() != RunState::Paused {
                    return Err("VM must be paused".into());
                }
                if !self.breakpoint_contexts.is_empty() {
                    let active = self.processes(cpu)?;
                    for process in self.breakpoint_contexts.values_mut() {
                        *process = active
                            .iter()
                            .find(|p| p.eprocess == process.eprocess && p.pid == process.pid)
                            .ok_or("breakpoint process exited; clear its breakpoint")?
                            .clone();
                    }
                }
                self.configure()?;
                self.continue_cpus.clear();
                for cpu in 0..self.vm.config().cpus as usize {
                    let state = self.vm.debug_registers(cpu).map_err(|e| e.to_string())?;
                    let regs = Registers::from_state(&state).map_err(|e| e.to_string())?;
                    if state.powered_on
                        && (self.execution_breakpoints().contains(&regs.pc)
                            || self.watch_stopped == Some(cpu))
                    {
                        self.continue_cpus.push_back(cpu);
                    }
                }
                self.watch_stopped = None;
                self.invalidate();
                self.continuing = true;
                self.continue_next()?;
                Ok(json!("running"))
            }
            "write_register" => {
                let register = vroom_hv::debug::Register::parse(
                    request["register"].as_str().ok_or("missing register")?,
                )
                .map_err(|e| e.to_string())?;
                let value = number(&request["value"])?;
                self.vm
                    .debug_write_register(cpu, register, value)
                    .map_err(|e| e.to_string())?;
                self.invalidate();
                Ok(json!(self.registers(cpu, false)?))
            }
            "write_virtual" | "write_physical" => {
                let address = number(&request["address"])?;
                let hex = request["hex"].as_str().ok_or("missing hex bytes")?;
                if hex.is_empty() || hex.len() > 32768 || hex.len() % 2 != 0 || !hex.is_ascii() {
                    return Err("write requires 1..16384 hex-encoded bytes".into());
                }
                let data = (0..hex.len())
                    .step_by(2)
                    .map(|i| {
                        u8::from_str_radix(&hex[i..i + 2], 16)
                            .map_err(|_| "invalid hex byte".to_owned())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let mappings = if op == "write_physical" {
                    vec![(address, 0..data.len())]
                } else {
                    let regs = self.registers(cpu, request["context"].as_bool().unwrap_or(true))?;
                    AddressSpace {
                        memory: &self.vm,
                        registers: &regs,
                    }
                    .write_spans(address, data.len())
                    .map_err(|e| e.to_string())?
                };
                let spans: Vec<_> = mappings
                    .iter()
                    .map(|(pa, range)| (*pa, &data[range.clone()]))
                    .collect();
                self.vm
                    .debug_write_physical(&spans)
                    .map_err(|e| e.to_string())?;
                self.invalidate();
                Ok(json!({"address":address, "written":data.len()}))
            }
            "step_over" | "step_out" => {
                self.cancel_run_to()?;
                let mut original = self.registers(cpu, false)?;
                original.sve_vl = vector_length(request)?;
                let space = AddressSpace {
                    memory: &CachedMemory {
                        memory: &self.vm,
                        cache: &self.pages,
                    },
                    registers: &original,
                };
                let (address, sp) = if op == "step_over" {
                    let mut bytes = [0; 4];
                    space
                        .read(original.pc, &mut bytes)
                        .map_err(|e| e.to_string())?;
                    if !vroom_kernel::control::is_call(u32::from_le_bytes(bytes)) {
                        return self.command(&json!({"op":"step", "cpu":cpu}));
                    }
                    (
                        original.pc.checked_add(4).ok_or("PC overflow")?,
                        original.sp,
                    )
                } else {
                    let mut modules = self.kernel_modules(cpu)?;
                    if original.pc >> 63 == 0 {
                        let process = self
                            .processes(cpu)?
                            .into_iter()
                            .find(|p| {
                                p.directory_table & 0x0000_ffff_ffff_f000
                                    == original.ttbr0 & 0x0000_ffff_ffff_f000
                            })
                            .ok_or("CPU process not found")?;
                        modules.extend(
                            process::modules(
                                &space,
                                self.symbols.as_ref().ok_or("load kernel symbols first")?,
                                &process,
                            )
                            .map_err(|e| e.to_string())?,
                        );
                    }
                    let module = modules
                        .iter()
                        .find(|m| original.pc >= m.base && original.pc - m.base < m.size as u64)
                        .ok_or("no module for CPU's current function")?;
                    let mut caller = original.clone();
                    self.unwind
                        .step(&space, module, &mut caller, false)
                        .map_err(|e| e.to_string())?;
                    (caller.pc, caller.sp)
                };
                if address == 0
                    || address & 3 != 0
                    || sp & 15 != 0
                    || (address == original.pc && sp == original.sp)
                {
                    return Err("invalid caller/return location".into());
                }
                // Validate the destination while stopped, before arming any control.
                space.translate(address).map_err(|e| e.to_string())?;
                if self.breakpoints.len() == 4 && !self.breakpoints.contains(&address) {
                    return Err("step-over/out needs one free hardware breakpoint slot".into());
                }
                self.run_to = Some(vroom_kernel::control::RunTo {
                    address,
                    sp,
                    ttbr0: original.ttbr0,
                    reason: if op == "step_over" {
                        "StepOver"
                    } else {
                        "StepOut"
                    },
                });
                match self.command(&json!({"op":"resume", "cpu":cpu})) {
                    Ok(_) => Ok(json!({"state":"running", "until":address})),
                    Err(error) => {
                        let _ = self.cancel_run_to();
                        Err(error)
                    }
                }
            }
            "registers" => {
                let regs = self.vm.debug_registers(cpu).map_err(|e| e.to_string())?;
                Ok(json!(
                    Registers::from_state(&regs).map_err(|e| e.to_string())?
                ))
            }
            "process_event" => {
                let state = self.vm.debug_registers(cpu).map_err(|e| e.to_string())?;
                let regs = Registers::from_state(&state).map_err(|e| e.to_string())?;
                let space = AddressSpace {
                    memory: &CachedMemory {
                        memory: &self.vm,
                        cache: &self.pages,
                    },
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
                if let Some(image) = &self.kernel {
                    return Ok(
                        json!({"base":image.base,"size":image.size,"name":image.name,"pdb":image.pdb,"export_count":image.exports.len()}),
                    );
                }
                let state = self.vm.debug_registers(cpu).map_err(|e| e.to_string())?;
                let regs = Registers::from_state(&state).map_err(|e| e.to_string())?;
                let space = AddressSpace {
                    memory: &CachedMemory {
                        memory: &self.vm,
                        cache: &self.pages,
                    },
                    registers: &regs,
                };
                let image = vroom_kernel::windows::discover_kernel(&space, &[regs.vbar, regs.pc])
                    .map_err(|e| e.to_string())?;
                let result = json!({"base":image.base,"size":image.size,"name":image.name,"pdb":image.pdb,"export_count":image.exports.len()});
                self.kernel = Some(image);
                Ok(result)
            }
            "symbols" => {
                let base = request.get("base").map(number).transpose()?;
                if let Ok(symbols) = self.selected_symbols(base) {
                    return Ok(
                        json!({"path":symbols.path,"count":symbols.names.len(),"types":symbols.types,"cached":true}),
                    );
                }
                if let Some(base) = base {
                    let regs = self.registers(cpu, request["context"].as_bool().unwrap_or(true))?;
                    let space = AddressSpace {
                        memory: &CachedMemory {
                            memory: &self.vm,
                            cache: &self.pages,
                        },
                        registers: &regs,
                    };
                    let image = Image::read(&space, base).map_err(|e| e.to_string())?;
                    let identity = image.pdb.as_ref().ok_or("image has no PDB identity")?;
                    let path = vroom_kernel::symbols::fetch(&self.cache, identity)
                        .map_err(|e| e.to_string())?;
                    let symbols =
                        Symbols::load(&path, identity, base).map_err(|e| e.to_string())?;
                    let result =
                        json!({"path":path,"count":symbols.names.len(),"types":symbols.types});
                    let context = if base >> 63 != 0 {
                        0
                    } else {
                        self.process.as_ref().map_or(0, |p| p.eprocess)
                    };
                    self.module_symbols.insert((context, base), symbols);
                    return Ok(result);
                }
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
            "functions" => {
                let symbols =
                    self.selected_symbols(request.get("base").map(number).transpose()?)?;
                let prefix = request["prefix"].as_str().unwrap_or("");
                let offset = request
                    .get("offset")
                    .map(number)
                    .transpose()?
                    .unwrap_or(0)
                    .min(1_000_000) as usize;
                let limit = request
                    .get("limit")
                    .map(number)
                    .transpose()?
                    .unwrap_or(200)
                    .min(1000) as usize;
                // Sorted index + binary lower bound + constant-time slice skip:
                // O(log n + page), including deep pages.
                let start = symbols
                    .functions
                    .partition_point(|(name, _)| name.as_str() < prefix);
                let mut iter = symbols
                    .functions
                    .iter()
                    .skip(start.saturating_add(offset))
                    .take_while(|(name, _)| name.starts_with(prefix));
                let rows: Vec<_> = iter
                    .by_ref()
                    .take(limit)
                    .map(|(name, address)| json!({"name":name,"address":address}))
                    .collect();
                Ok(json!({"rows":rows,"more":iter.next().is_some(),"offset":offset}))
            }
            "processes" | "process" | "peb" | "user_modules" => {
                if op == "process" && request.get("pid").is_none() {
                    self.process = None;
                    return Ok(json!({"context":"cpu"}));
                }
                let symbols = self.symbols.as_ref().ok_or("load kernel symbols first")?;
                let regs = self.registers(cpu, matches!(op, "peb" | "user_modules"))?;
                let space = AddressSpace {
                    memory: &CachedMemory {
                        memory: &self.vm,
                        cache: &self.pages,
                    },
                    registers: &regs,
                };
                if op == "peb" || op == "user_modules" {
                    let selected = self.process.as_ref().ok_or("select a process first")?;
                    let active = self
                        .processes(cpu)?
                        .into_iter()
                        .find(|p| p.eprocess == selected.eprocess && p.pid == selected.pid)
                        .ok_or("selected process exited")?;
                    return if op == "peb" {
                        process::peb(&space, symbols, &active).map(|v| json!(v))
                    } else {
                        process::modules(&space, symbols, &active).map(|v| json!(v))
                    }
                    .map_err(|e| e.to_string());
                }
                let processes = self.processes(cpu)?;
                if op == "processes" {
                    return Ok(json!(processes));
                }
                let pid = number(&request["pid"])?;
                let process = processes
                    .into_iter()
                    .find(|p| p.pid == pid)
                    .ok_or("PID not found")?;
                let result = json!(process);
                self.process = Some(process);
                Ok(result)
            }
            "stack" => {
                // CPU stack always uses the actual CPU address space, even when
                // another process has been selected for memory inspection.
                let mut regs = self.registers(cpu, false)?;
                regs.tcr &= !(1 << 7);
                regs.sve_vl = vector_length(request)?;
                let space = AddressSpace {
                    memory: &CachedMemory {
                        memory: &self.vm,
                        cache: &self.pages,
                    },
                    registers: &regs,
                };
                let symbols = self.symbols.as_ref().ok_or("load kernel symbols first")?;
                let mut modules = self.kernel_modules(cpu)?;
                let mut stack_process = 0;
                {
                    let processes = self.processes(cpu)?;
                    if let Some(process) = processes.iter().find(|p| {
                        p.directory_table & 0x0000fffffffff000 == regs.ttbr0 & 0x0000fffffffff000
                    }) {
                        stack_process = process.eprocess;
                        if let Ok(user) = process::modules(&space, symbols, process) {
                            modules.extend(user);
                        }
                    }
                }
                let mut stack =
                    vroom_kernel::unwind::walk(&space, &regs, &modules, &mut self.unwind);
                for frame in &mut stack.frames {
                    let module = modules
                        .iter()
                        .find(|m| frame.pc >= m.base && frame.pc - m.base < m.size as u64)
                        .ok_or("stack frame module disappeared")?;
                    let symbols = if self.kernel.as_ref().is_some_and(|k| k.base == module.base) {
                        self.symbols.as_ref()
                    } else {
                        self.module_symbols.get(&(
                            if module.base >> 63 != 0 {
                                0
                            } else {
                                stack_process
                            },
                            module.base,
                        ))
                    };
                    let found =
                        symbols.and_then(|s| s.addresses.range(module.base..=frame.pc).next_back());
                    frame.symbol = Some(match found {
                        Some((&a, name)) => format!("{}!{}+{:#x}", module.name, name, frame.pc - a),
                        None => format!("{}+{:#x}", module.name, frame.pc - module.base),
                    });
                }
                Ok(json!(stack))
            }
            "cache" => Ok(
                json!({"physical_pages":self.pages.pages(),"capacity_pages":8192,"module_pdbs":self.module_symbols.len()}),
            ),
            "resolve" => {
                let name = request["name"].as_str().ok_or("missing name")?;
                let base = request.get("base").map(number).transpose()?;
                if base.is_some() {
                    return Ok(json!(self
                        .selected_symbols(base)?
                        .resolve(name)
                        .map_err(|e| e.to_string())?));
                }
                if let Ok(symbols) = self.selected_symbols(None) {
                    return Ok(json!(symbols.resolve(name).map_err(|e| e.to_string())?));
                }
                self.kernel
                    .as_ref()
                    .and_then(|k| k.exports.get(name.strip_prefix("nt!").unwrap_or(name)))
                    .map(|&a| json!(a))
                    .ok_or_else(|| "symbol not found; load matching PDB symbols".into())
            }
            "export" | "image" => {
                let base = number(&request["base"])?;
                let cache_key = format!(
                    "{op}:{cpu}:{base}:{}:{}:{}",
                    self.process.as_ref().map_or(0, |p| p.eprocess),
                    request["allow_missing"],
                    request["path"]
                );
                if let Some(result) = self.export_cache.get(&cache_key) {
                    return Ok(result.clone());
                }
                let regs = self.registers(cpu, true)?;
                let space = AddressSpace {
                    memory: &CachedMemory {
                        memory: &self.vm,
                        cache: &self.pages,
                    },
                    registers: &regs,
                };
                let image =
                    vroom_kernel::windows::Image::read(&space, base).map_err(|e| e.to_string())?;
                let partial = request["allow_missing"].as_bool().unwrap_or(false);
                let (bytes, missing, image, source) = if op == "image" {
                    let context = (self.process.as_ref().map_or(0, |p| p.eprocess), base);
                    let path = if let Some(path) = request["path"].as_str() {
                        PathBuf::from(path)
                    } else if let Some(path) = self.local_images.get(&context) {
                        path.clone()
                    } else {
                        vroom_kernel::binary::fetch(&self.cache, &image)
                            .map_err(|e| e.to_string())?
                    };
                    let disk = vroom_kernel::binary::DiskImage::load(&path, &image)
                        .map_err(|e| e.to_string())?;
                    if request["path"].is_string() {
                        self.local_images.insert(context, path.clone());
                    }
                    let (bytes, missing) =
                        disk.image.export(&disk, false).map_err(|e| e.to_string())?;
                    (
                        bytes,
                        missing,
                        disk.image,
                        json!({"kind":"file","path":path,"runtime_base":base}),
                    )
                } else {
                    let (bytes, missing) =
                        image.export(&space, partial).map_err(|e| e.to_string())?;
                    (
                        bytes,
                        missing,
                        image,
                        json!({"kind":"live","runtime_base":base}),
                    )
                };
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
                let metadata_image = json!({"base":image.base,"size":image.size,"name":image.name,
                    "timestamp":image.timestamp,"pdb":image.pdb,"relocation_directory":image.relocation_directory});
                let manifest = json!({"path":path,"image":metadata_image,"missing_page_rvas":missing,"source":source,"layout":"flat PE; section file offsets equal RVAs; analysis only"});
                let metadata = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(path.with_extension("json"))
                    .map_err(|e| e.to_string())?;
                serde_json::to_writer_pretty(metadata, &manifest).map_err(|e| e.to_string())?;
                if op == "image" {
                    // A supplied local image becomes the default static source
                    // for this module, including subsequent dec-file requests.
                    for allow in [Value::Null, json!(false)] {
                        let key = format!(
                            "{op}:{cpu}:{base}:{}:{}:{}",
                            self.process.as_ref().map_or(0, |p| p.eprocess),
                            allow,
                            Value::Null
                        );
                        self.export_cache.insert(key, manifest.clone());
                    }
                }
                self.export_cache.insert(cache_key, manifest.clone());
                Ok(manifest)
            }
            "modules" | "callbacks" => {
                let symbols = self
                    .symbols
                    .as_ref()
                    .ok_or("load matching kernel symbols first")?;
                let state = self.vm.debug_registers(cpu).map_err(|e| e.to_string())?;
                let regs = Registers::from_state(&state).map_err(|e| e.to_string())?;
                let space = AddressSpace {
                    memory: &CachedMemory {
                        memory: &self.vm,
                        cache: &self.pages,
                    },
                    registers: &regs,
                };
                let modules = self.kernel_modules(cpu)?;
                if op == "callbacks" {
                    return Ok(json!(vroom_kernel::windows::process_callbacks(
                        &space, symbols, &modules
                    )
                    .map_err(|e| e.to_string())?));
                }
                Ok(json!(modules))
            }
            "dump" => {
                let address = number(&request["address"])?;
                let length = number(&request["length"])?;
                if length == 0 || length > 64 * 1024 * 1024 || address.checked_add(length).is_none()
                {
                    return Err("dump length must be 1..64 MiB without address overflow".into());
                }
                let regs = self.registers(cpu, true)?;
                let space = AddressSpace {
                    memory: &CachedMemory {
                        memory: &self.vm,
                        cache: &self.pages,
                    },
                    registers: &regs,
                };
                std::fs::create_dir_all(&self.exports).map_err(|e| e.to_string())?;
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|e| e.to_string())?
                    .as_nanos();
                let path = self
                    .exports
                    .join(format!("memory-{address:016x}-{stamp}.bin"));
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(|e| e.to_string())?;
                let result = (|| {
                    let mut bytes = vec![0; 65536];
                    let mut done = 0;
                    while done < length {
                        let n = (length - done).min(bytes.len() as u64) as usize;
                        space
                            .read(address + done, &mut bytes[..n])
                            .map_err(|e| e.to_string())?;
                        file.write_all(&bytes[..n]).map_err(|e| e.to_string())?;
                        done += n as u64;
                    }
                    Ok::<_, String>(
                        json!({"path":path,"address":address,"length":length,"cpu":cpu,
                        "pid":self.process.as_ref().map(|p|p.pid),"format":"raw virtual memory"}),
                    )
                })();
                if result.is_err() {
                    drop(file);
                    let _ = std::fs::remove_file(&path);
                }
                result
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
                    let regs = self.registers(cpu, request["context"].as_bool().unwrap_or(true))?;
                    let space = AddressSpace {
                        memory: &CachedMemory {
                            memory: &self.vm,
                            cache: &self.pages,
                        },
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
            "watchpoints" => {
                let watches = request["watches"]
                    .as_array()
                    .ok_or("expected watches array")?;
                if watches.len() > 2 {
                    return Err("at most two hardware watchpoints".into());
                }
                let watches = watches
                    .iter()
                    .map(|w| {
                        let size = number(&w["size"])?;
                        if size > 8 {
                            return Err("watch size exceeds 8".to_owned());
                        }
                        let mode = w["mode"].as_str().ok_or("missing watch mode: r/w/rw")?;
                        if !matches!(mode, "r" | "w" | "rw") {
                            return Err("invalid watch mode".into());
                        }
                        let watch = Watchpoint {
                            address: number(&w["address"])?,
                            size: size as u8,
                            read: mode.contains('r'),
                            write: mode.contains('w'),
                        };
                        watch.arm64_control().map_err(|e| e.to_string())?;
                        Ok(watch)
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                let old = std::mem::replace(&mut self.watchpoints, watches);
                if let Err(error) = self.configure() {
                    self.watchpoints = old;
                    let rollback = self.configure();
                    return Err(format!("{error}; rollback: {rollback:?}"));
                }
                Ok(json!(request["watches"]))
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
                let mut contexts = BTreeMap::new();
                for &address in &addresses {
                    if address >> 63 == 0 {
                        let process = self
                            .breakpoint_contexts
                            .get(&address)
                            .or(self.process.as_ref())
                            .ok_or("select a process before setting a user-address breakpoint")?;
                        contexts.insert(address, process.clone());
                    }
                }
                let old = std::mem::replace(&mut self.breakpoints, addresses);
                if let Err(e) = self.configure() {
                    self.breakpoints = old;
                    let rollback = self.configure();
                    return Err(format!("{e}; rollback: {rollback:?}; VM remains paused"));
                }
                self.breakpoint_contexts = contexts;
                Ok(json!(self.breakpoints))
            }
            "step" => {
                self.cancel_run_to()?;
                self.invalidate();
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
    let args_agent_socket = args.agent_socket.clone();
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
    if hv.arch() != vroom_hv::Arch::Aarch64 {
        return Err("Windows debugger requires an ARM64 host (HVF or KVM)".into());
    }
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
            // The lab doesn't sandbox the VMM it drives.
            sandbox: false,
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
        breakpoint_contexts: BTreeMap::new(),
        watchpoints: Vec::new(),
        watch_stopped: None,
        step_cpu: None,
        run_to: None,
        pages: PageCache::default(),
        process_cache: Default::default(),
        module_cache: Default::default(),
        unwind: Default::default(),
        export_cache: BTreeMap::new(),
        local_images: BTreeMap::new(),
        process: None,
        module_symbols: BTreeMap::new(),
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
    let (tx, rx) = mpsc::sync_channel::<Input>(8);
    let _endpoint = match &args_agent_socket {
        Some(path) => {
            let (forward, requests) = mpsc::channel();
            let endpoint = agent::start(path, forward)?;
            let tx = tx.clone();
            std::thread::spawn(move || {
                for request in requests {
                    if tx.send(Input::Agent(request)).is_err() {
                        break;
                    }
                }
            });
            eprintln!("vroom-lab: agents: {}", path.display());
            Some(endpoint)
        }
        None => None,
    };
    std::thread::spawn(move || {
        let mut input = std::io::stdin().lock();
        loop {
            let mut line = Vec::new();
            let n = std::io::Read::take(&mut input, 65537).read_until(b'\n', &mut line);
            match n {
                Ok(0) | Err(_) => break,
                Ok(_) if line.len() > 65536 => {
                    let _ = tx.send(Input::Stdio(Err("request exceeds 64 KiB".to_owned())));
                    break;
                }
                Ok(_) => {
                    if tx
                        .send(Input::Stdio(
                            serde_json::from_slice::<Value>(&line).map_err(|e| e.to_string()),
                        ))
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
                    session.watch_stopped = (stop.reason
                        == vroom_hv::debug::DebugReason::Watchpoint)
                        .then_some(stop.cpu as usize);
                    if session.continuing && stop.reason == vroom_hv::debug::DebugReason::SingleStep
                    {
                        session.continue_next()?;
                        continue;
                    }
                    let mut reason = format!("{:?}", stop.reason);
                    if stop.reason == vroom_hv::debug::DebugReason::Breakpoint {
                        let regs = session.registers(stop.cpu as usize, false)?;
                        let user_hit = session.breakpoints.contains(&stop.pc)
                            && session.breakpoint_contexts.get(&stop.pc).map_or(true, |p| {
                                p.directory_table & 0x0000_ffff_ffff_f000
                                    == regs.ttbr0 & 0x0000_ffff_ffff_f000
                            });
                        let temporary_hit = session
                            .run_to
                            .as_ref()
                            .is_some_and(|run| run.matches(&regs));
                        if temporary_hit {
                            reason = session
                                .run_to
                                .as_ref()
                                .expect("matched temporary")
                                .reason
                                .into();
                        } else if !user_hit
                            && (session.breakpoints.contains(&stop.pc)
                                || session
                                    .run_to
                                    .as_ref()
                                    .is_some_and(|run| run.address == stop.pc))
                        {
                            session.continue_cpus.clear();
                            session.continue_cpus.push_back(stop.cpu as usize);
                            session.continuing = true;
                            session.continue_next()?;
                            continue;
                        }
                    }
                    session.cancel_run_to()?;
                    session.continuing = false;
                    session.continue_cpus.clear();
                    output(json!({"event":"stopped", "cpu":stop.cpu, "pc":stop.pc,
                        "reason":reason, "syndrome":stop.syndrome}))?;
                }
                VmEvent::Shutdown => {
                    session.vm.shutdown();
                    output(json!({"event":"shutdown"}))?;
                    return Ok(());
                }
                VmEvent::Crashed(e) => {
                    session.vm.pause().map_err(|e| e.to_string())?;
                    session.cancel_run_to()?;
                    output(json!({"event":"fault", "message":e}))?;
                }
                VmEvent::Reset => {
                    session.vm.pause().map_err(|e| e.to_string())?;
                    session.cancel_run_to()?;
                    output(
                        json!({"event":"reset_requested", "message":"guest requested reset; restart the lab to reboot"}),
                    )?;
                }
                _ => {}
            }
        }
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(Input::Stdio(Ok(request))) => {
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
            Ok(Input::Stdio(Err(error))) => output(json!({"id":null,"error":error}))?,
            Ok(Input::Agent(forward)) => {
                let _ = forward.reply.send(session.command(&forward.request));
                if session.vm.state() == RunState::Stopped {
                    output(json!({"event":"shutdown"}))?;
                    return Ok(());
                }
            }
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

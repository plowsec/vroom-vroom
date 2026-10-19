//! Agent endpoint: line-delimited JSON-RPC 2.0 on an owner-only Unix socket,
//! the same protocol as angr_tui's (`initialize`, `describe`, `commands.list`,
//! `commands.run`, `results.page`, `jobs.*`), plus `events.poll` for debugger
//! stops. Debugger commands run on the lab's main thread, in order with the
//! TUI's; fuzzer commands run `vroom-fuzz` in its own process (one VM each).

use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const NOT_INITIALIZED: i64 = -32001;
const UNKNOWN_HANDLE: i64 = -32002;
const SERVICE_STOPPED: i64 = -32004;

const MAX_LINE: usize = 8 << 20;
const DEFAULT_ITEMS: usize = 200;
const MAX_PAGE: usize = 500;
const KEPT_EVENTS: usize = 1024;

const METHODS: &[&str] = &[
    "initialize",
    "describe",
    "commands.list",
    "commands.run",
    "results.page",
    "events.poll",
    "jobs.start",
    "jobs.status",
    "jobs.result",
    "jobs.cancel",
];

/// A debugger request for the lab's main thread, and where its answer goes.
pub struct Forward {
    pub request: Value,
    pub reply: mpsc::Sender<Result<Value, String>>,
}

struct Error {
    code: i64,
    message: String,
}

fn error(code: i64, message: impl Into<String>) -> Error {
    Error { code, message: message.into() }
}

// ---- events ---------------------------------------------------------------

/// Debugger events (stops, faults, shutdown), numbered, for `events.poll`.
#[derive(Default)]
struct Events {
    log: Mutex<(u64, VecDeque<Value>)>,
    more: Condvar,
}

fn events() -> &'static Events {
    static EVENTS: OnceLock<Events> = OnceLock::new();
    EVENTS.get_or_init(Events::default)
}

/// Keep an event the lab sent to its frontend for agents too.
pub fn record_event(event: &Value) {
    let e = events();
    let mut log = e.log.lock().unwrap_or_else(|p| p.into_inner());
    log.0 += 1;
    let mut event = event.clone();
    event["seq"] = json!(log.0);
    log.1.push_back(event);
    while log.1.len() > KEPT_EVENTS {
        log.1.pop_front();
    }
    e.more.notify_all();
}

/// Events after `after` (default: from now on), waiting up to `wait` for one.
fn poll_events(after: Option<u64>, wait: Duration, limit: usize) -> Value {
    let e = events();
    let deadline = Instant::now() + wait;
    let mut log = e.log.lock().unwrap_or_else(|p| p.into_inner());
    let after = after.unwrap_or(log.0);
    while log.0 <= after {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        log = e.more.wait_timeout(log, left).unwrap_or_else(|p| p.into_inner()).0;
    }
    let rows: Vec<Value> =
        log.1.iter().filter(|v| v["seq"].as_u64().is_some_and(|s| s > after)).take(limit).cloned().collect();
    let next = rows.last().and_then(|v| v["seq"].as_u64()).unwrap_or(after);
    json!({"events": rows, "next": next, "latest": log.0})
}

// ---- commands -------------------------------------------------------------

struct Param {
    name: &'static str,
    kind: &'static str,
    required: bool,
    help: &'static str,
}

const fn p(name: &'static str, kind: &'static str, required: bool, help: &'static str) -> Param {
    Param { name, kind, required, help }
}

const CPU: Param = p("cpu", "integer", false, "vCPU index (default 0)");
const CONTEXT: Param = p("context", "boolean", false, "use the selected process's address space (default true)");
const ADDRESS: Param = p("address", "address", true, "integer or \"0x...\"");
const VL: Param = p("vector_length", "integer", false, "SVE vector length in bytes, if the CPU uses SVE");

#[derive(Clone, Copy)]
enum Target {
    /// A vroom-lab debugger op, run on the main thread.
    Op(&'static str),
    FuzzCheck,
    FuzzRun,
    FuzzProbe,
    FuzzTrace,
    FuzzCoverage,
    FuzzBlocks,
    FuzzDiff,
    FuzzCrashes,
}

struct Command {
    name: &'static str,
    category: &'static str,
    summary: &'static str,
    params: &'static [Param],
    /// Changes guest state, the VM, or files.
    mutates: bool,
    /// "read", "control" (runs the guest or a VM), "write" (guest memory,
    /// registers, files) or "destructive" (ends the session).
    risk: &'static str,
    /// Long running: only through `jobs.start`.
    job_only: bool,
    target: Target,
}

const fn c(
    name: &'static str,
    category: &'static str,
    summary: &'static str,
    params: &'static [Param],
    risk: &'static str,
    target: Target,
) -> Command {
    let mutates = !matches!(risk.as_bytes(), b"read");
    Command { name, category, summary, params, mutates, risk, job_only: false, target }
}

const fn job(mut command: Command) -> Command {
    command.job_only = true;
    command
}

static COMMANDS: &[Command] = &[
    // VM control.
    c("vm.status", "vm", "Run state, CPU count, breakpoints", &[], "read", Target::Op("status")),
    c("vm.pause", "vm", "Stop every CPU", &[], "control", Target::Op("pause")),
    c("vm.resume", "vm", "Run until a breakpoint, watchpoint or pause; poll events.poll for the stop", &[CPU], "control", Target::Op("resume")),
    c("vm.step", "vm", "Execute one instruction on a CPU", &[CPU], "control", Target::Op("step")),
    c("vm.step_over", "vm", "Step over a call (runs to the next instruction)", &[CPU, VL], "control", Target::Op("step_over")),
    c("vm.step_out", "vm", "Run until the current function returns", &[CPU, VL], "control", Target::Op("step_out")),
    c("vm.snapshot", "vm", "Save a snapshot (the VM keeps its state; running or paused). Into this VM's snapshots, where the app lists them, unless path is given", &[
        p("name", "string", false, "folder name under the VM's snapshots, default snap-<unix time>"),
        p("path", "path", false, "a folder elsewhere instead (e.g. for vroom-fuzz)"),
        p("description", "string", false, "default vroom-lab"),
    ], "write", Target::Op("snapshot")),
    c("vm.snapshots", "vm", "This VM's snapshots, oldest first", &[], "read", Target::Op("snapshots")),
    c("vm.restore", "vm", "Restart this VM from a snapshot (paused, breakpoints and caches cleared); refused if its disk layers changed since", &[
        p("name", "string", false, "a snapshot of this VM (snap-...)"),
        p("path", "path", false, "or a snapshot folder"),
    ], "destructive", Target::Op("restore")),
    c("vm.screenshot", "vm", "Save the guest screen as PNG", &[p("path", "path", true, "PNG file to write")], "write", Target::Op("screen")),
    c("vm.key", "vm", "Press or release a key (USB HID usage code)", &[p("code", "integer", true, "HID usage code"), p("down", "boolean", true, "true = press, false = release")], "control", Target::Op("key")),
    c("vm.shutdown", "vm", "Stop the VM and end the lab", &[], "destructive", Target::Op("shutdown")),
    c("guest.exec", "guest", "Run a command in the running guest through its agent (Windows: cmd.exe as SYSTEM; Linux: sh as root)", &[
        p("command", "string", true, "command line"),
        p("powershell", "boolean", false, "PowerShell instead (Linux: pwsh)"),
        p("timeout", "integer", false, "seconds before it is killed, default 60"),
        p("max_output", "integer", false, "bytes kept of stdout and of stderr, default 1048576"),
    ], "write", Target::Op("guest_exec")),
    c("guest.upload", "guest", "Copy a host file into the running guest (made or replaced once whole); size and SHA-256", &[
        p("host_path", "path", true, "absolute, or under the lab's exports directory"),
        p("guest_path", "string", true, "the guest's path (Windows: C:\\...)"),
    ], "write", Target::Op("guest_upload")),
    c("guest.download", "guest", "Copy a file out of the running guest; size and SHA-256", &[
        p("guest_path", "string", true, "the guest's path"),
        p("host_path", "path", false, "new file: absolute, or under the exports directory (default: exports/guest-<stamp>-<name>)"),
    ], "write", Target::Op("guest_download")),
    c("guest.update_tools", "guest", "Install the lab's guest tools in the running guest (target/guest-tools); the agent restarts", &[], "write", Target::Op("guest_update_tools")),
    // CPU.
    c("cpu.registers", "cpu", "General, PC, SP, PSTATE and translation registers", &[CPU], "read", Target::Op("registers")),
    c("cpu.write_register", "cpu", "Set a register (x0..x30, pc, sp, pstate, ttbr0, ...)", &[CPU, p("register", "string", true, "register name"), p("value", "address", true, "new value")], "write", Target::Op("write_register")),
    // Memory.
    c("memory.regions", "memory", "Guest RAM regions (physical)", &[], "read", Target::Op("regions")),
    c("memory.read_virtual", "memory", "Read guest virtual memory as hex", &[CPU, ADDRESS, p("length", "integer", false, "bytes, default 256, at most 65536"), CONTEXT], "read", Target::Op("read_virtual")),
    c("memory.read_physical", "memory", "Read guest physical memory as hex", &[ADDRESS, p("length", "integer", false, "bytes, default 256, at most 65536")], "read", Target::Op("read_physical")),
    c("memory.translate", "memory", "Walk the page tables for a virtual address", &[CPU, ADDRESS, CONTEXT], "read", Target::Op("translate")),
    c("memory.write_virtual", "memory", "Write hex bytes to guest virtual memory", &[CPU, ADDRESS, p("hex", "string", true, "bytes as hex"), CONTEXT], "write", Target::Op("write_virtual")),
    c("memory.write_physical", "memory", "Write hex bytes to guest physical memory", &[ADDRESS, p("hex", "string", true, "bytes as hex")], "write", Target::Op("write_physical")),
    c("memory.dump", "memory", "Dump a virtual range to a file under the lab's exports directory", &[CPU, ADDRESS, p("length", "integer", true, "bytes")], "write", Target::Op("dump")),
    // Breakpoints.
    c("debug.breakpoints", "debug", "Replace the hardware execution breakpoints (at most 4, aligned)", &[p("addresses", "array", true, "addresses; [] clears")], "control", Target::Op("breakpoints")),
    c("debug.watchpoints", "debug", "Replace the hardware watchpoints (at most 2)", &[p("watches", "array", true, "[{address, size, mode: r|w|rw}]; [] clears")], "control", Target::Op("watchpoints")),
    // Windows kernel.
    c("kernel.info", "kernel", "Find ntoskrnl: base, size, PDB", &[CPU], "read", Target::Op("kernel")),
    c("kernel.symbols", "kernel", "Load PDB symbols for the kernel or a module base (slow the first time)", &[CPU, p("base", "address", false, "module base; kernel if omitted"), CONTEXT], "read", Target::Op("symbols")),
    c("kernel.functions", "kernel", "List functions by name prefix", &[p("base", "address", false, "module base"), p("prefix", "string", false, "name prefix")], "read", Target::Op("functions")),
    c("kernel.resolve", "kernel", "Address of a symbol (nt!Name or Name)", &[p("name", "string", true, "symbol"), p("base", "address", false, "module base")], "read", Target::Op("resolve")),
    c("kernel.processes", "kernel", "Running processes", &[CPU], "read", Target::Op("processes")),
    c("kernel.process", "kernel", "Select a process for memory and symbols (no pid: back to the CPU's)", &[CPU, p("pid", "integer", false, "process id")], "read", Target::Op("process")),
    c("kernel.peb", "kernel", "A process's PEB", &[CPU, p("pid", "integer", true, "process id")], "read", Target::Op("peb")),
    c("kernel.user_modules", "kernel", "A process's loaded DLLs", &[CPU, p("pid", "integer", true, "process id")], "read", Target::Op("user_modules")),
    c("kernel.modules", "kernel", "Loaded kernel modules (drivers)", &[CPU], "read", Target::Op("modules")),
    c("kernel.callbacks", "kernel", "Registered kernel callbacks", &[CPU], "read", Target::Op("callbacks")),
    c("kernel.stack", "kernel", "Unwound call stack of a CPU", &[CPU, VL], "read", Target::Op("stack")),
    c("kernel.image", "kernel", "Module image sections and exports (optionally from a file on disk)", &[CPU, p("base", "address", true, "module base"), p("path", "path", false, "the module's file"), p("allow_missing", "boolean", false, "accept paged-out pages")], "read", Target::Op("image")),
    c("kernel.export", "kernel", "Write a module image to a file for a disassembler", &[CPU, p("base", "address", true, "module base"), p("path", "path", false, "the module's file"), p("allow_missing", "boolean", false, "accept paged-out pages")], "write", Target::Op("export")),
    c("kernel.process_event", "kernel", "Decode a process create/exit callback's arguments at a stop", &[CPU], "read", Target::Op("process_event")),
    c("kernel.cache", "kernel", "Debugger page and symbol cache sizes", &[], "read", Target::Op("cache")),
    // Snapshot fuzzer.
    c("fuzz.check", "fuzz", "Validate a harness and show what it declares", &[p("harness", "path", true, "harness JSON")], "read", Target::FuzzCheck),
    job(c("fuzz.run", "fuzz", "Fuzzing campaign from a snapshot (its own VM); results under output", &[
        p("harness", "path", true, "harness JSON"),
        p("seed", "path", true, "seed file (payload bytes with ioctl)"),
        p("output", "path", true, "new run directory"),
        p("ioctl", "address", false, "fixed IOCTL code"),
        p("iterations", "integer", false, "inputs to run"),
        p("duration_secs", "integer", false, "or seconds to run"),
        p("state_dir", "path", false, "VM state directory, one per parallel run"),
        p("repeat_seed", "boolean", false, "replay the seed"),
        p("no_boundary_seeds", "boolean", false, "skip size boundary inputs"),
    ], "control", Target::FuzzRun)),
    job(c("fuzz.probe", "fuzz", "Try every IOCTL in the harness range from the snapshot", &[
        p("harness", "path", true, "harness JSON"),
        p("payload", "path", true, "payload bytes"),
        p("output", "path", true, "new probe directory"),
        p("state_dir", "path", false, "VM state directory, one per parallel run"),
    ], "control", Target::FuzzProbe)),
    job(c("fuzz.trace", "fuzz", "Replay one input (a saved crash) an instruction at a time from the snapshot: a symbolized trace file, how it ended, the call tree and stack", &[
        p("harness", "path", true, "harness JSON"),
        p("input", "path", true, "input file as the fuzzer saved it"),
        p("output", "path", true, "new trace file"),
        p("max", "integer", false, "instructions at most, default 200000"),
        p("registers", "boolean", false, "general registers on every line"),
        p("blocks", "path", false, "block list to name functions from too"),
        p("state_dir", "path", false, "VM state directory, one per parallel run"),
    ], "control", Target::FuzzTrace)),
    c("fuzz.coverage", "fuzz", "Per-function block coverage of a run", &[p("run", "path", true, "run directory (coverage.json, blocks.json)")], "read", Target::FuzzCoverage),
    c("fuzz.blocks", "fuzz", "Blocks of a run with hit counts; filter by function or covered", &[p("run", "path", true, "run directory"), p("function", "string", false, "only this function"), p("covered", "boolean", false, "only covered (true) or uncovered (false)")], "read", Target::FuzzBlocks),
    c("fuzz.diff", "fuzz", "Blocks covered by one run and not the other", &[p("a", "path", true, "run directory"), p("b", "path", true, "run directory")], "read", Target::FuzzDiff),
    c("fuzz.crashes", "fuzz", "Findings saved by a run, with their inputs", &[p("run", "path", true, "run directory")], "read", Target::FuzzCrashes),
];

fn find(name: &str) -> Result<&'static Command, Error> {
    COMMANDS
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| error(METHOD_NOT_FOUND, format!("unknown command: {name}; see commands.list")))
}

fn describe_command(c: &Command) -> Value {
    json!({
        "name": c.name,
        "category": c.category,
        "summary": c.summary,
        "params": c.params.iter().map(|p| json!({"name":p.name,"type":p.kind,"required":p.required,"help":p.help})).collect::<Vec<_>>(),
        "mutates": c.mutates,
        "risk": c.risk,
        "job_only": c.job_only,
    })
}

fn check_args(c: &Command, args: &Map<String, Value>) -> Result<(), Error> {
    for name in args.keys() {
        if !c.params.iter().any(|p| p.name == name) {
            return Err(error(INVALID_PARAMS, format!("{}: unknown argument {name}", c.name)));
        }
    }
    for p in c.params.iter().filter(|p| p.required) {
        if args.get(p.name).is_none_or(Value::is_null) {
            return Err(error(INVALID_PARAMS, format!("{}: missing argument {}", c.name, p.name)));
        }
    }
    Ok(())
}

// ---- results and jobs -----------------------------------------------------

struct Stored {
    owner: String,
    items: Vec<Value>,
    at: Instant,
}

enum State {
    Running,
    Done(Value),
    Failed(String),
    Cancelled,
}

struct Job {
    owner: String,
    command: &'static str,
    started: Instant,
    state: State,
    child: Option<Child>,
    /// Where a fuzzer job's progress lines go.
    log: Option<PathBuf>,
}

pub struct Shared {
    forward: mpsc::Sender<Forward>,
    results: Mutex<HashMap<String, Stored>>,
    jobs: Mutex<HashMap<String, Job>>,
    counter: AtomicU64,
    fuzz: Option<PathBuf>,
}

impl Shared {
    fn handle(&self, kind: &str) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        format!("{kind}-{n:x}{t:08x}")
    }

    /// A large array result goes to the store; the reply has its first page.
    fn finish(&self, value: Value, limit: usize, owner: &str) -> Value {
        let Value::Array(items) = value else { return value };
        if items.len() <= limit {
            return Value::Array(items);
        }
        let total = items.len();
        let first: Vec<Value> = items[..limit].to_vec();
        let handle = self.handle("result");
        let mut results = self.results.lock().unwrap_or_else(|p| p.into_inner());
        results.retain(|_, s| s.at.elapsed() < Duration::from_secs(15 * 60));
        if results.len() >= 64 {
            if let Some(oldest) = results.iter().min_by_key(|(_, s)| s.at).map(|(k, _)| k.clone()) {
                results.remove(&oldest);
            }
        }
        results.insert(handle.clone(), Stored { owner: owner.into(), items, at: Instant::now() });
        json!({"items": first, "total": total, "handle": handle, "next_offset": limit})
    }

    fn run(&self, c: &'static Command, args: Map<String, Value>) -> Result<Value, String> {
        match c.target {
            Target::Op(op) => {
                let mut request = Value::Object(args);
                request["op"] = json!(op);
                let (reply, answer) = mpsc::channel();
                self.forward.send(Forward { request, reply }).map_err(|_| "the lab has stopped".to_string())?;
                answer.recv().map_err(|_| "the lab has stopped".to_string())?
            }
            Target::FuzzCheck => {
                let out = Process::new(self.fuzz()?)
                    .arg("check")
                    .arg(path(&args, "harness")?)
                    .output()
                    .map_err(|e| format!("vroom-fuzz: {e}"))?;
                let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
                if !out.status.success() {
                    return Err(text.trim().to_string());
                }
                Ok(json!({"report": text.trim()}))
            }
            Target::FuzzCoverage => coverage(&path(&args, "run")?),
            Target::FuzzBlocks => blocks(&path(&args, "run")?, args.get("function").and_then(Value::as_str), args.get("covered").and_then(Value::as_bool)),
            Target::FuzzDiff => diff(&path(&args, "a")?, &path(&args, "b")?),
            Target::FuzzCrashes => crashes(&path(&args, "run")?),
            Target::FuzzRun | Target::FuzzProbe | Target::FuzzTrace => Err(format!("{} is long running: start it with jobs.start", c.name)),
        }
    }

    fn fuzz(&self) -> Result<PathBuf, String> {
        self.fuzz.clone().ok_or_else(|| {
            "vroom-fuzz not found: build and sign it (cargo build --release -p vroom-fuzz; scripts/sign.sh target/release/vroom-fuzz) or set VROOM_FUZZ".into()
        })
    }

    /// The vroom-fuzz process for a fuzz.run, fuzz.probe or fuzz.trace job.
    fn spawn_fuzzer(&self, c: &Command, args: &Map<String, Value>) -> Result<(Child, PathBuf), String> {
        let output = path(args, "output")?;
        if output.exists() {
            return Err(format!("output already exists: {}; choose a new path", output.display()));
        }
        let fuzz = self.fuzz()?;
        if cfg!(target_os = "macos") {
            // A rebuild drops the signature: the VM would fail with HV_DENIED.
            let signed = Process::new("/usr/bin/codesign")
                .args(["-d", "--entitlements", "-"])
                .arg(&fuzz)
                .output()
                .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("com.apple.security.hypervisor"));
            if !signed {
                return Err(format!("{} lacks the hypervisor entitlement: scripts/sign.sh {}", fuzz.display(), fuzz.display()));
            }
        }
        let mut cmd = Process::new(fuzz);
        if matches!(c.target, Target::FuzzTrace) {
            // A file: its directory must be there.
            if let Some(dir) = output.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            cmd.arg("trace").arg(path(args, "harness")?).arg(path(args, "input")?).arg("--out").arg(&output);
            if let Some(v) = args.get("max") {
                cmd.arg("--max").arg(crate::number(v)?.to_string());
            }
            if args.get("registers").and_then(Value::as_bool) == Some(true) {
                cmd.arg("--registers");
            }
            for (name, flag) in [("blocks", "--blocks"), ("state_dir", "--state-dir")] {
                if let Some(v) = args.get(name).and_then(Value::as_str) {
                    cmd.arg(flag).arg(v);
                }
            }
        } else {
            std::fs::create_dir_all(&output).map_err(|e| e.to_string())?;
        }
        if matches!(c.target, Target::FuzzRun) {
            cmd.arg("run").arg(path(args, "harness")?).arg(path(args, "seed")?).arg("--output").arg(&output);
            if let Some(v) = args.get("ioctl") {
                cmd.arg("--ioctl").arg(format!("{:#x}", crate::number(v)?));
            }
            for (name, flag) in [("iterations", "--iterations"), ("duration_secs", "--duration-secs")] {
                if let Some(v) = args.get(name) {
                    cmd.arg(flag).arg(crate::number(v)?.to_string());
                }
            }
            if let Some(v) = args.get("state_dir").and_then(Value::as_str) {
                cmd.arg("--state-dir").arg(v);
            }
            for (name, flag) in [("repeat_seed", "--repeat-seed"), ("no_boundary_seeds", "--no-boundary-seeds")] {
                if args.get(name).and_then(Value::as_bool) == Some(true) {
                    cmd.arg(flag);
                }
            }
        } else if matches!(c.target, Target::FuzzProbe) {
            // probe refuses an existing output directory: it makes its own.
            std::fs::remove_dir(&output).map_err(|e| e.to_string())?;
            cmd.arg("probe").arg(path(args, "harness")?).arg(path(args, "payload")?).arg("--output").arg(&output);
            if let Some(v) = args.get("state_dir").and_then(Value::as_str) {
                cmd.arg("--state-dir").arg(v);
            }
        }
        let log = output.with_extension("agent.log");
        let results = std::fs::File::create(output.with_extension("agent.jsonl")).map_err(|e| e.to_string())?;
        let progress = std::fs::File::create(&log).map_err(|e| e.to_string())?;
        let child = cmd
            .stdin(Stdio::null())
            .stdout(results)
            .stderr(progress)
            .spawn()
            .map_err(|e| format!("vroom-fuzz: {e}"))?;
        Ok((child, log))
    }
}

fn path(args: &Map<String, Value>, name: &str) -> Result<PathBuf, String> {
    let s = args.get(name).and_then(Value::as_str).ok_or_else(|| format!("{name}: expected a path string"))?;
    if s.is_empty() {
        return Err(format!("{name}: empty path"));
    }
    Ok(PathBuf::from(s))
}

fn read_json(path: &Path) -> Result<Value, String> {
    serde_json::from_slice(&std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// A run's blocks with their hit counts (0 = not reached).
fn run_blocks(run: &Path) -> Result<Vec<(Value, u64)>, String> {
    let map = read_json(&run.join("coverage.json"))?;
    let blocks = read_json(&run.join("blocks.json"))?;
    let hits = map["blocks"].as_object().cloned().unwrap_or_default();
    Ok(blocks
        .as_array()
        .ok_or("blocks.json: expected an array")?
        .iter()
        .map(|b| {
            let id = b["id"].as_u64().map(|i| i.to_string()).unwrap_or_default();
            (b.clone(), hits.get(&id).and_then(|h| h["hits"].as_u64()).unwrap_or(0))
        })
        .collect())
}

fn function_of(block: &Value) -> String {
    block["function"].as_str().filter(|f| !f.is_empty()).unwrap_or("<unknown>").to_string()
}

fn coverage(run: &Path) -> Result<Value, String> {
    let map = read_json(&run.join("coverage.json"))?;
    let blocks = run_blocks(run)?;
    let mut functions: BTreeMap<String, (usize, usize, u64)> = BTreeMap::new();
    for (block, hits) in &blocks {
        let row = functions.entry(function_of(block)).or_default();
        row.0 += 1;
        row.1 += usize::from(*hits > 0);
        row.2 += hits;
    }
    let covered = blocks.iter().filter(|(_, h)| *h > 0).count();
    let mut rows: Vec<Value> = functions
        .into_iter()
        .map(|(name, (total, hit, hits))| {
            json!({"function": name, "blocks": total, "covered": hit, "percent": (hit * 1000 / total.max(1)) as f64 / 10.0, "hits": hits})
        })
        .collect();
    rows.sort_by(|a, b| b["covered"].as_u64().cmp(&a["covered"].as_u64()));
    Ok(json!({
        "executions": map["executions"],
        "edges": map["edges"].as_array().map_or(0, Vec::len),
        "blocks": blocks.len(),
        "covered": covered,
        "percent": (covered * 1000 / blocks.len().max(1)) as f64 / 10.0,
        "functions": rows,
    }))
}

fn block_row(block: &Value, hits: u64) -> Value {
    json!({"id": block["id"], "va": block["va"].as_u64().map(|v| format!("{v:#x}")), "size": block["size"], "function": function_of(block), "module": block["module"], "hits": hits})
}

fn blocks(run: &Path, function: Option<&str>, covered: Option<bool>) -> Result<Value, String> {
    Ok(Value::Array(
        run_blocks(run)?
            .into_iter()
            .filter(|(b, h)| function.is_none_or(|f| function_of(b) == f) && covered.is_none_or(|c| (*h > 0) == c))
            .map(|(b, h)| block_row(&b, h))
            .collect(),
    ))
}

fn diff(a: &Path, b: &Path) -> Result<Value, String> {
    let a_blocks = run_blocks(a)?;
    let b_hit: std::collections::HashSet<u64> =
        run_blocks(b)?.into_iter().filter(|(_, h)| *h > 0).filter_map(|(b, _)| b["va"].as_u64()).collect();
    let a_hit: std::collections::HashSet<u64> =
        a_blocks.iter().filter(|(_, h)| *h > 0).filter_map(|(b, _)| b["va"].as_u64()).collect();
    let only_a: Vec<Value> = a_blocks
        .iter()
        .filter(|(b, h)| *h > 0 && b["va"].as_u64().is_some_and(|va| !b_hit.contains(&va)))
        .map(|(b, h)| block_row(b, *h))
        .collect();
    let only_b: Vec<Value> = run_blocks(b)?
        .into_iter()
        .filter(|(b, h)| *h > 0 && b["va"].as_u64().is_some_and(|va| !a_hit.contains(&va)))
        .map(|(b, h)| block_row(&b, h))
        .collect();
    Ok(json!({"only_a": only_a, "only_b": only_b, "a_covered": a_hit.len(), "b_covered": b_hit.len()}))
}

fn crashes(run: &Path) -> Result<Value, String> {
    let dir = run.join("crashes");
    let mut rows = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().is_some_and(|e| e == "json") {
            let mut finding = read_json(&path)?;
            let input = path.with_extension("bin");
            finding["input_path"] = json!(input);
            finding["input_hex"] = json!(std::fs::read(&input)
                .map(|b| b.iter().take(4096).map(|x| format!("{x:02x}")).collect::<String>())
                .unwrap_or_default());
            rows.push(finding);
        }
    }
    Ok(Value::Array(rows))
}

// ---- JSON-RPC -------------------------------------------------------------

struct Connection {
    shared: Arc<Shared>,
    actor: Option<String>,
}

fn param_object(params: &Value) -> Result<Map<String, Value>, Error> {
    match params {
        Value::Null => Ok(Map::new()),
        Value::Object(m) => Ok(m.clone()),
        _ => Err(error(INVALID_PARAMS, "params must be an object")),
    }
}

fn allow(params: &Map<String, Value>, names: &[&str]) -> Result<(), Error> {
    match params.keys().find(|k| !names.contains(&k.as_str())) {
        Some(k) => Err(error(INVALID_PARAMS, format!("unexpected parameter {k}"))),
        None => Ok(()),
    }
}

fn limit(params: &Map<String, Value>, default: usize) -> Result<usize, Error> {
    match params.get("limit") {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_PAGE as u64).contains(n))
            .map(|n| n as usize)
            .ok_or_else(|| error(INVALID_PARAMS, format!("limit: 1..{MAX_PAGE}"))),
    }
}

impl Connection {
    fn actor(&self) -> Result<&str, Error> {
        self.actor.as_deref().ok_or_else(|| error(NOT_INITIALIZED, "call initialize with an actor first"))
    }

    fn call(&mut self, method: &str, params: &Value) -> Result<Value, Error> {
        let params = param_object(params)?;
        if method == "initialize" {
            allow(&params, &["actor"])?;
            let actor = params.get("actor").and_then(Value::as_str).unwrap_or("");
            let ok = !actor.is_empty()
                && actor.len() <= 64
                && actor.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c));
            if !ok {
                return Err(error(INVALID_PARAMS, "actor: 1-64 letters, digits, '_', '.', '-'"));
            }
            self.actor = Some(actor.into());
            return Ok(json!({"actor": actor, "server": "vroom-lab", "protocol": "jsonrpc-2.0/lines", "methods": METHODS}));
        }
        let actor = self.actor()?.to_string();
        match method {
            "describe" => {
                allow(&params, &[])?;
                let status = self.shared.run(find("vm.status")?, Map::new()).unwrap_or_else(|e| json!({"error": e}));
                let mut categories: Vec<&str> = COMMANDS.iter().map(|c| c.category).collect();
                categories.dedup();
                Ok(json!({
                    "server": "vroom-lab",
                    "summary": "vroom VMM: Windows ARM64 kernel debugger on a live VM, plus the snapshot fuzzer and its coverage",
                    "vm": status,
                    "categories": categories,
                    "commands": COMMANDS.len(),
                    "fuzzer": self.shared.fuzz.as_ref().map(|p| p.display().to_string()),
                    "notes": [
                        "Addresses are integers or \"0x...\" strings; results give integers.",
                        "Inspection needs a paused VM: vm.pause, or wait for a stop with events.poll after vm.resume.",
                        "The lab starts paused (restore too): vm.resume runs the guest; its network, agent and screen are frozen until then.",
                        "guest.* needs a running VM and a guest agent (vm.status: guest_agent); guest.exec needs current guest tools: guest.update_tools installs them.",
                        "Commands with mutates=true change the guest, the VM or files; risk says how much.",
                        "fuzz.run and fuzz.probe start their own VM from a snapshot: run them with jobs.start."
                    ],
                }))
            }
            "commands.list" => {
                allow(&params, &["category"])?;
                let category = params.get("category").and_then(Value::as_str).unwrap_or("");
                Ok(Value::Array(COMMANDS.iter().filter(|c| category.is_empty() || c.category == category).map(describe_command).collect()))
            }
            "commands.run" => {
                allow(&params, &["command", "args", "limit"])?;
                let (command, args) = command_args(&params)?;
                if command.job_only {
                    return Err(error(INVALID_PARAMS, format!("{} is long running: start it with jobs.start", command.name)));
                }
                let limit = limit(&params, DEFAULT_ITEMS)?;
                let value = self.shared.run(command, args).map_err(|e| error(INTERNAL_ERROR, e))?;
                Ok(self.shared.finish(value, limit, &actor))
            }
            "results.page" => {
                allow(&params, &["handle", "offset", "limit"])?;
                let handle = params.get("handle").and_then(Value::as_str).ok_or_else(|| error(INVALID_PARAMS, "missing handle"))?;
                let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
                let limit = limit(&params, DEFAULT_ITEMS)?;
                let results = self.shared.results.lock().unwrap_or_else(|p| p.into_inner());
                let stored = results.get(handle).filter(|s| s.owner == actor).ok_or_else(|| error(UNKNOWN_HANDLE, "unknown or expired handle"))?;
                let end = (offset + limit).min(stored.items.len());
                let items = stored.items.get(offset.min(end)..end).unwrap_or(&[]).to_vec();
                Ok(json!({"items": items, "total": stored.items.len(), "next_offset": (end < stored.items.len()).then_some(end)}))
            }
            "events.poll" => {
                allow(&params, &["after", "wait_ms", "limit"])?;
                let after = params.get("after").and_then(Value::as_u64);
                let wait = params.get("wait_ms").and_then(Value::as_u64).unwrap_or(0).min(30_000);
                Ok(poll_events(after, Duration::from_millis(wait), limit(&params, 100)?))
            }
            "jobs.start" => {
                allow(&params, &["command", "args"])?;
                let (command, args) = command_args(&params)?;
                self.start_job(command, args, &actor)
            }
            "jobs.status" | "jobs.result" | "jobs.cancel" => {
                allow(&params, &["handle", "limit"])?;
                let handle = params.get("handle").and_then(Value::as_str).ok_or_else(|| error(INVALID_PARAMS, "missing handle"))?.to_string();
                let limit = limit(&params, DEFAULT_ITEMS)?;
                self.job(method, &handle, &actor, limit)
            }
            _ => Err(error(METHOD_NOT_FOUND, format!("unknown method: {method}"))),
        }
    }

    fn start_job(&self, command: &'static Command, args: Map<String, Value>, actor: &str) -> Result<Value, Error> {
        let handle = self.shared.handle("job");
        let (child, log) = if matches!(command.target, Target::FuzzRun | Target::FuzzProbe | Target::FuzzTrace) {
            let (child, log) = self.shared.spawn_fuzzer(command, &args).map_err(|e| error(INTERNAL_ERROR, e))?;
            (Some(child), Some(log))
        } else {
            (None, None)
        };
        let fuzzer = child.is_some();
        self.shared.jobs.lock().unwrap_or_else(|p| p.into_inner()).insert(
            handle.clone(),
            Job { owner: actor.into(), command: command.name, started: Instant::now(), state: State::Running, child, log },
        );
        if !fuzzer {
            let shared = self.shared.clone();
            let id = handle.clone();
            std::thread::spawn(move || {
                let state = match shared.run(command, args) {
                    Ok(v) => State::Done(v),
                    Err(e) => State::Failed(e),
                };
                if let Some(job) = shared.jobs.lock().unwrap_or_else(|p| p.into_inner()).get_mut(&id) {
                    if matches!(job.state, State::Running) {
                        job.state = state;
                    }
                }
            });
        }
        Ok(json!({"handle": handle, "command": command.name, "state": "running"}))
    }

    fn job(&self, method: &str, handle: &str, actor: &str, limit: usize) -> Result<Value, Error> {
        let mut jobs = self.shared.jobs.lock().unwrap_or_else(|p| p.into_inner());
        let job = jobs.get_mut(handle).filter(|j| j.owner == actor).ok_or_else(|| error(UNKNOWN_HANDLE, "unknown job"))?;
        // A fuzzer job is done when its process is.
        if let (State::Running, Some(child)) = (&job.state, job.child.as_mut()) {
            if let Ok(Some(status)) = child.try_wait() {
                let tail = job.log.as_deref().map(|l| last_lines(&tail(l, 4000), 6)).unwrap_or_default();
                job.state = if status.success() {
                    State::Done(json!({"exit": status.code(), "log": tail}))
                } else {
                    State::Failed(format!("vroom-fuzz exited with {status}: {tail}"))
                };
                job.child = None;
            }
        }
        if method == "jobs.cancel" {
            if matches!(job.state, State::Running) {
                if let Some(child) = job.child.as_mut() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                job.child = None;
                job.state = State::Cancelled;
            }
        }
        let state = match &job.state {
            State::Running => "running",
            State::Done(_) => "done",
            State::Failed(_) => "failed",
            State::Cancelled => "cancelled",
        };
        let mut status = json!({"handle": handle, "command": job.command, "state": state, "seconds": job.started.elapsed().as_secs_f64()});
        if let Some(log) = &job.log {
            status["log"] = json!(log);
            status["results"] = json!(log.with_extension("jsonl"));
            status["progress"] = json!(tail(log, 600).lines().last().unwrap_or(""));
        }
        if method != "jobs.result" {
            return Ok(status);
        }
        match &job.state {
            State::Done(v) => {
                let v = v.clone();
                drop(jobs);
                status["result"] = self.shared.finish(v, limit, actor);
                Ok(status)
            }
            State::Failed(e) => Err(error(INTERNAL_ERROR, e.clone())),
            State::Running => Err(error(INVALID_REQUEST, "job is still running; poll jobs.status")),
            State::Cancelled => Err(error(INVALID_REQUEST, "job was cancelled")),
        }
    }
}

fn command_args(params: &Map<String, Value>) -> Result<(&'static Command, Map<String, Value>), Error> {
    let name = params.get("command").and_then(Value::as_str).ok_or_else(|| error(INVALID_PARAMS, "missing command"))?;
    let command = find(name)?;
    let args = param_object(params.get("args").unwrap_or(&Value::Null))?;
    check_args(command, &args)?;
    Ok((command, args))
}

fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn tail(path: &Path, bytes: u64) -> String {
    let Ok(mut file) = std::fs::File::open(path) else { return String::new() };
    let len = file.metadata().map_or(0, |m| m.len());
    let _ = std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(len.saturating_sub(bytes)));
    let mut out = String::new();
    let _ = file.read_to_string(&mut out);
    out
}

fn reply(id: &Value, outcome: Result<Value, Error>) -> Value {
    match outcome {
        Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
        Err(e) => json!({"jsonrpc":"2.0","id":id,"error":{"code":e.code,"message":e.message}}),
    }
}

/// One request line to its reply line (None for notifications).
fn handle_line(connection: &mut Connection, line: &[u8]) -> Option<Value> {
    let request: Value = match serde_json::from_slice(line) {
        Ok(v) => v,
        Err(e) => return Some(reply(&Value::Null, Err(error(PARSE_ERROR, format!("parse error: {e}"))))),
    };
    let Some(object) = request.as_object() else {
        return Some(reply(&Value::Null, Err(error(INVALID_REQUEST, "request must be an object"))));
    };
    let id = object.get("id").cloned();
    let valid_id = id.as_ref().is_none_or(|v| v.is_null() || v.is_string() || v.is_i64() || v.is_u64());
    let method = object.get("method").and_then(Value::as_str).filter(|m| !m.is_empty());
    let (Some(method), true, true) = (method, object.get("jsonrpc") == Some(&json!("2.0")), valid_id) else {
        let id = if valid_id { id.unwrap_or(Value::Null) } else { Value::Null };
        return Some(reply(&id, Err(error(INVALID_REQUEST, "request needs jsonrpc \"2.0\", a method string and a string or integer id"))));
    };
    let outcome = connection.call(method, object.get("params").unwrap_or(&Value::Null));
    id.map(|id| reply(&id, outcome))
}

fn serve(stream: UnixStream, shared: Arc<Shared>) {
    let Ok(write) = stream.try_clone() else { return };
    let mut write = write;
    let mut read = BufReader::new(stream);
    let mut connection = Connection { shared, actor: None };
    loop {
        let mut line = Vec::new();
        match (&mut read).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let answer = if line.len() > MAX_LINE {
            // Too long: report it and drop the connection (its rest is garbage).
            let r = reply(&Value::Null, Err(error(PARSE_ERROR, format!("request exceeds {MAX_LINE} bytes"))));
            let _ = writeln!(write, "{r}");
            return;
        } else if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        } else {
            handle_line(&mut connection, &line)
        };
        if let Some(answer) = answer {
            if writeln!(write, "{answer}").and_then(|_| write.flush()).is_err() {
                return;
            }
        }
    }
}

/// The calling user must own the other end.
fn same_user(stream: &UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: a connected Unix socket and two out pointers.
    let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    // SAFETY: no preconditions.
    rc == 0 && uid == unsafe { libc::geteuid() }
}

/// Removes the socket when the lab ends.
pub struct Endpoint {
    path: PathBuf,
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.path).is_ok_and(|m| m.file_type().is_socket()) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Where vroom-fuzz is: $VROOM_FUZZ, next to this binary, or target/release.
fn find_fuzzer() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("VROOM_FUZZ") {
        return Some(PathBuf::from(p));
    }
    let exe = std::env::current_exe().ok()?;
    let mut candidates = vec![exe.with_file_name("vroom-fuzz")];
    for dir in exe.ancestors().skip(1) {
        if dir.file_name().is_some_and(|n| n == "target") {
            candidates.push(dir.join("release/vroom-fuzz"));
            break;
        }
    }
    candidates.push(PathBuf::from("target/release/vroom-fuzz"));
    candidates.into_iter().find(|p| p.is_file())
}

/// Listen on `path` (owner-only); debugger commands go to `forward`.
pub fn start(path: &Path, forward: mpsc::Sender<Forward>) -> Result<Endpoint, String> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(m) if !m.file_type().is_socket() => {
            return Err(format!("refusing to replace {}: not a socket", path.display()));
        }
        Ok(_) => {
            if UnixStream::connect(path).is_ok() {
                return Err(format!("an agent endpoint already listens at {}", path.display()));
            }
            std::fs::remove_file(path).map_err(|e| e.to_string())?;
        }
        Err(_) => {}
    }
    // sockaddr_un holds 104 bytes on macOS (108 on Linux), terminator included.
    if path.as_os_str().len() >= 104 {
        return Err(format!("{}: socket paths must be under 104 bytes; use a shorter or relative path", path.display()));
    }
    let listener = UnixListener::bind(path).map_err(|e| format!("{}: {e}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    let shared = Arc::new(Shared {
        forward,
        results: Mutex::default(),
        jobs: Mutex::default(),
        counter: AtomicU64::new(1),
        fuzz: find_fuzzer(),
    });
    std::thread::Builder::new()
        .name("agent-rpc".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                if !same_user(&stream) {
                    let mut stream = stream;
                    let r = reply(&Value::Null, Err(error(SERVICE_STOPPED, "connections only from the lab's user")));
                    let _ = writeln!(stream, "{r}");
                    continue;
                }
                let shared = shared.clone();
                let _ = std::thread::Builder::new().name("agent-client".into()).spawn(move || serve(stream, shared));
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(Endpoint { path: path.to_path_buf() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> (Connection, mpsc::Receiver<Forward>) {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared { forward: tx, results: Mutex::default(), jobs: Mutex::default(), counter: AtomicU64::new(1), fuzz: None });
        (Connection { shared, actor: None }, rx)
    }

    fn call(c: &mut Connection, line: &str) -> Value {
        handle_line(c, line.as_bytes()).expect("a reply")
    }

    #[test]
    fn needs_initialize_and_valid_requests() {
        let (mut c, _rx) = connection();
        assert_eq!(call(&mut c, "nope")["error"]["code"], PARSE_ERROR);
        assert_eq!(call(&mut c, r#"{"id":1,"method":"describe"}"#)["error"]["code"], INVALID_REQUEST);
        assert_eq!(call(&mut c, r#"{"jsonrpc":"2.0","id":1,"method":"commands.list"}"#)["error"]["code"], NOT_INITIALIZED);
        assert_eq!(call(&mut c, r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"actor":"bad actor"}}"#)["error"]["code"], INVALID_PARAMS);
        assert_eq!(call(&mut c, r#"{"jsonrpc":"2.0","id":3,"method":"initialize","params":{"actor":"pentest"}}"#)["result"]["actor"], "pentest");
        assert!(handle_line(&mut c, br#"{"jsonrpc":"2.0","method":"commands.list"}"#).is_none(), "notifications get no reply");
        let list = call(&mut c, r#"{"jsonrpc":"2.0","id":"x","method":"commands.list","params":{"category":"fuzz"}}"#);
        assert!(list["result"].as_array().unwrap().iter().all(|c| c["category"] == "fuzz"));
        assert_eq!(call(&mut c, r#"{"jsonrpc":"2.0","id":4,"method":"nope"}"#)["error"]["code"], METHOD_NOT_FOUND);
    }

    #[test]
    fn debugger_commands_go_to_the_lab_with_checked_arguments() {
        let (mut c, rx) = connection();
        call(&mut c, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"actor":"a"}}"#);
        let bad = call(&mut c, r#"{"jsonrpc":"2.0","id":2,"method":"commands.run","params":{"command":"memory.read_virtual","args":{"length":4}}}"#);
        assert_eq!(bad["error"]["code"], INVALID_PARAMS);
        let lab = std::thread::spawn(move || {
            let f = rx.recv().unwrap();
            assert_eq!(f.request["op"], "read_virtual");
            assert_eq!(f.request["address"], "0x1000");
            f.reply.send(Ok(json!({"hex":"00"}))).unwrap();
        });
        let ok = call(&mut c, r#"{"jsonrpc":"2.0","id":3,"method":"commands.run","params":{"command":"memory.read_virtual","args":{"address":"0x1000"}}}"#);
        assert_eq!(ok["result"]["hex"], "00");
        lab.join().unwrap();
        let job = call(&mut c, r#"{"jsonrpc":"2.0","id":4,"method":"commands.run","params":{"command":"fuzz.run","args":{"harness":"h","seed":"s","output":"o"}}}"#);
        assert_eq!(job["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn large_results_are_paged() {
        let (mut c, _rx) = connection();
        call(&mut c, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"actor":"a"}}"#);
        let first = c.shared.finish(Value::Array((0..450).map(|i| json!(i)).collect()), 200, "a");
        assert_eq!(first["total"], 450);
        let handle = first["handle"].as_str().unwrap();
        let page = call(&mut c, &format!(r#"{{"jsonrpc":"2.0","id":2,"method":"results.page","params":{{"handle":"{handle}","offset":400}}}}"#));
        assert_eq!(page["result"]["items"].as_array().unwrap().len(), 50);
        assert_eq!(page["result"]["next_offset"], Value::Null);
        call(&mut c, r#"{"jsonrpc":"2.0","id":3,"method":"initialize","params":{"actor":"b"}}"#);
        let other = call(&mut c, &format!(r#"{{"jsonrpc":"2.0","id":4,"method":"results.page","params":{{"handle":"{handle}"}}}}"#));
        assert_eq!(other["error"]["code"], UNKNOWN_HANDLE, "results belong to their actor");
    }

    #[test]
    fn events_are_numbered_and_polled() {
        let before = poll_events(None, Duration::ZERO, 1000)["latest"].as_u64().unwrap();
        record_event(&json!({"event":"stopped","pc":4}));
        let polled = poll_events(Some(before), Duration::from_millis(10), 10);
        assert_eq!(polled["events"][0]["event"], "stopped");
        assert_eq!(polled["events"][0]["seq"], before + 1);
        assert_eq!(poll_events(Some(before + 1), Duration::from_millis(5), 10)["events"].as_array().unwrap().len(), 0);
        // Without `after`, only what comes next.
        let waiter = std::thread::spawn(|| poll_events(None, Duration::from_secs(5), 10));
        std::thread::sleep(Duration::from_millis(50));
        record_event(&json!({"event":"fault"}));
        let next = waiter.join().unwrap();
        assert_eq!(next["events"].as_array().unwrap().len(), 1);
        assert_eq!(next["events"][0]["event"], "fault");
    }

    #[test]
    fn coverage_of_a_run_directory() {
        let dir = std::env::temp_dir().join(format!("vroom-agent-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("crashes")).unwrap();
        std::fs::write(dir.join("coverage.json"), r#"{"executions":3,"new_edges":1,"blocks":{"1":{"hits":2,"first_input":"x"}},"edges":[[1,2]]}"#).unwrap();
        std::fs::write(dir.join("blocks.json"), r#"[{"id":1,"va":4096,"size":4,"function":"f"},{"id":2,"va":4100,"size":4,"function":"f"},{"id":3,"va":8192,"size":8,"function":"g"}]"#).unwrap();
        let summary = coverage(&dir).unwrap();
        assert_eq!((summary["blocks"].as_u64(), summary["covered"].as_u64()), (Some(3), Some(1)));
        assert_eq!(summary["functions"][0]["function"], "f");
        assert_eq!(summary["functions"][0]["percent"], 50.0);
        assert_eq!(blocks(&dir, None, Some(false)).unwrap().as_array().unwrap().len(), 2);
        assert_eq!(diff(&dir, &dir).unwrap()["only_a"].as_array().unwrap().len(), 0);
        assert_eq!(crashes(&dir).unwrap().as_array().unwrap().len(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

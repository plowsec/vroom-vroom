//! Line-delimited JSON control endpoint; the Textual frontend owns this process.
//! `--agent-socket` also serves agents (JSON-RPC 2.0, see agent.rs).
mod agent;
mod guest;

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
    /// Restore a vroom snapshot instead of cold booting: its folder, or the
    /// name of one of this VM's snapshots (snap-...).
    #[arg(long)]
    restore: Option<PathBuf>,
    /// Also serve agents (JSON-RPC 2.0, one object per line) on this
    /// owner-only Unix socket. The lab then keeps running when stdin closes.
    #[arg(long)]
    agent_socket: Option<PathBuf>,
    /// Detach into the background (its own session: shells and agent
    /// harnesses that kill what a command started leave it running) and
    /// return once the agent socket accepts. Log: the socket path + ".log".
    #[arg(long, requires = "agent_socket")]
    daemon: bool,
}

/// `--restore`'s folder: as given, or a snapshot of this VM by name.
/// Anything else is explained (a path with a space split in two by the
/// shell looks like this), with the VM's snapshots to pick from.
fn restore_dir(given: &std::path::Path, state_dir: &std::path::Path) -> Result<PathBuf, String> {
    // A snapshot folder holds manifest.json.
    let snapshots = state_dir.join("snapshots");
    if given.join("manifest.json").is_file() {
        return Ok(given.to_path_buf());
    }
    if given.components().count() == 1 && snapshots.join(given).join("manifest.json").is_file() {
        return Ok(snapshots.join(given));
    }
    let mut names: Vec<String> = std::fs::read_dir(&snapshots)
        .map(|d| d.flatten().filter(|e| e.path().join("manifest.json").is_file()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    names.sort();
    let what = if given.exists() { "not a vroom snapshot (no manifest.json in it)" } else { "no such folder" };
    let mut why = format!("{}: {what}. Quote paths with spaces.", given.display());
    if names.is_empty() {
        why.push_str(&format!(" This VM has no snapshots in {}.", snapshots.display()));
    } else {
        why.push_str(&format!(" This VM's snapshots (pass a name): {}", names.join(", ")));
    }
    Err(why)
}

/// Write end of the pipe the `--daemon` parent waits on.
static READY: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// Fork before any thread exists. The parent waits for the child to say
/// it listens (or to die) and exits; the child carries on as the lab.
fn daemonize(socket: &std::path::Path) -> Result<(), String> {
    let log_path = PathBuf::from(format!("{}.log", socket.display()));
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("{}: {e}", log_path.display()))?;
    let mut fds = [0; 2];
    // SAFETY: two out fds.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    for fd in fds {
        // Helpers the lab starts (swtpm...) must not hold the pipe open.
        // SAFETY: our own new fds.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    // SAFETY: single-threaded here (called before anything spawns).
    match unsafe { libc::fork() } {
        -1 => Err(std::io::Error::last_os_error().to_string()),
        0 => {
            use std::os::fd::AsRawFd;
            // SAFETY: plain fd and session calls in the new child.
            unsafe {
                libc::close(fds[0]);
                libc::setsid();
                let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
                libc::dup2(null, 0);
                libc::dup2(log.as_raw_fd(), 1);
                libc::dup2(log.as_raw_fd(), 2);
            }
            READY.store(fds[1], std::sync::atomic::Ordering::Release);
            Ok(())
        }
        child => {
            // SAFETY: our pipe's write end.
            unsafe { libc::close(fds[1]) };
            let mut ready = String::new();
            // SAFETY: our pipe's read end, owned from here on.
            let pipe = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fds[0]) };
            // One line: "ok", or EOF if the child exits first.
            let _ = std::io::BufRead::read_line(&mut std::io::BufReader::new(pipe), &mut ready);
            if ready.starts_with("ok") {
                println!(
                    "vroom-lab: running (pid {child}); agents: {}; log: {}",
                    socket.display(),
                    log_path.display()
                );
                std::process::exit(0);
            }
            let tail = std::fs::read_to_string(&log_path).unwrap_or_default();
            let tail: Vec<&str> = tail.lines().rev().take(5).collect();
            eprintln!("vroom-lab: failed to start; {}:", log_path.display());
            for line in tail.into_iter().rev() {
                eprintln!("  {line}");
            }
            std::process::exit(1);
        }
    }
}

/// Set by SIGTERM, SIGINT or SIGHUP: stop the VM and remove the socket.
static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_stop_signal(_: libc::c_int) {
    STOP.store(true, std::sync::atomic::Ordering::Release);
}

/// Tell a waiting `--daemon` parent the agent socket accepts.
fn report_ready() {
    let fd = READY.swap(-1, std::sync::atomic::Ordering::AcqRel);
    if fd >= 0 {
        // SAFETY: the pipe's write end, written once and closed.
        unsafe {
            libc::write(fd, b"ok\n".as_ptr().cast(), 3);
            libc::close(fd);
        }
    }
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

/// Bytes a tracepoint may read per hit.
const MAX_TRACE_BYTES: u64 = 4096;
const DEFAULT_TRACE_CAPACITY: usize = 4096;
const MAX_TRACE_CAPACITY: u64 = 1 << 20;
/// Debug events handled before the loop looks at requests again.
const EVENT_BURST: usize = 256;
const MAX_TRACE_ACTIONS: usize = 16;
const MAX_TRACE_CAPTURES: usize = 16;

/// An execution breakpoint that records the CPU and continues.
struct Tracepoint {
    address: u64,
    /// The register holding the pointer to read, as named by the caller.
    register: Option<(String, vroom_hv::debug::Register)>,
    length: u64,
    /// Recorded on every hit, before the actions run.
    captures: Vec<Capture>,
    max_hits: Option<u64>,
    stop: bool,
    /// Applied in order on every hit, before the record is appended.
    actions: Vec<Action>,
    hits: u64,
    /// A user-address tracepoint's process (None: any).
    context: Option<process::Process>,
}

impl Tracepoint {
    fn armed(&self) -> bool {
        !self.max_hits.is_some_and(|max| self.hits >= max)
    }
    fn json(&self) -> Value {
        json!({"address": self.address, "register": self.register.as_ref().map(|r| &r.0), "length": self.length,
            "captures": self.captures.iter().map(|c| &c.source).collect::<Vec<_>>(), "max_hits": self.max_hits, "stop": self.stop,
            "actions": self.actions.iter().map(|a| &a.source).collect::<Vec<_>>(), "hits": self.hits, "armed": self.armed(),
            "pid": self.context.as_ref().map(|p| p.pid)})
    }
}

/// What a tracepoint hit asks of the stopped CPU.
struct TraceHit {
    stop: bool,
    /// An action moved its PC off the tracepoint: it resumes from there
    /// without a step, so a breakpoint at the new PC still hits.
    moved: bool,
}

struct TraceRecord {
    seq: u64,
    tracepoint: usize,
    hit: u64,
    regs: Registers,
    pointer: Option<u64>,
    data: Option<Result<Vec<u8>, String>>,
    /// One per capture, in the tracepoint's order.
    captures: Vec<(String, Captured)>,
    actions_applied: usize,
    action_error: Option<String>,
}

impl TraceRecord {
    fn json(&self) -> Value {
        let r = &self.regs;
        let mut v = json!({"seq": self.seq, "tracepoint": self.tracepoint, "hit": self.hit, "cpu": r.cpu, "pc": r.pc,
            "sp": r.sp, "pstate": r.pstate, "ttbr0": r.ttbr0, "x": r.x, "actions_applied": self.actions_applied});
        if let Some(e) = &self.action_error {
            v["action_error"] = json!(e);
        }
        if let Some(pointer) = self.pointer {
            v["pointer"] = json!(pointer);
        }
        match &self.data {
            Some(Ok(bytes)) => v["hex"] = json!(hex_string(bytes)),
            Some(Err(e)) => v["read_error"] = json!(e),
            None => {}
        }
        if !self.captures.is_empty() {
            let (mut values, mut errors, mut addresses) = (serde_json::Map::new(), serde_json::Map::new(), serde_json::Map::new());
            for (name, captured) in &self.captures {
                match captured {
                    Captured::Register(value) => {
                        values.insert(name.clone(), json!(value));
                    }
                    Captured::Memory { address, data } => {
                        if let Some(address) = address {
                            addresses.insert(name.clone(), json!(address));
                        }
                        match data {
                            Ok(bytes) => values.insert(name.clone(), json!(hex_string(bytes))),
                            Err(e) => errors.insert(name.clone(), json!(e)),
                        };
                    }
                }
            }
            v["captures"] = Value::Object(values);
            v["capture_errors"] = Value::Object(errors);
            v["capture_addresses"] = Value::Object(addresses);
        }
        v
    }
}

fn hex_string(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn register_value(regs: &Registers, register: vroom_hv::debug::Register) -> u64 {
    use vroom_hv::debug::Register as R;
    match register {
        R::X(n) => regs.x[n as usize],
        R::Pc => regs.pc,
        R::Sp => regs.sp,
        R::Pstate => regs.pstate,
        R::Ttbr0 => regs.ttbr0,
        R::Ttbr1 => regs.ttbr1,
        R::Tcr => regs.tcr,
        R::Sctlr => regs.sctlr,
        R::Vbar => regs.vbar,
    }
}

fn set_register_value(regs: &mut Registers, register: vroom_hv::debug::Register, value: u64) {
    use vroom_hv::debug::Register as R;
    *match register {
        R::X(n) => &mut regs.x[n as usize],
        R::Pc => &mut regs.pc,
        R::Sp => &mut regs.sp,
        R::Pstate => &mut regs.pstate,
        R::Ttbr0 => &mut regs.ttbr0,
        R::Ttbr1 => &mut regs.ttbr1,
        R::Tcr => &mut regs.tcr,
        R::Sctlr => &mut regs.sctlr,
        R::Vbar => &mut regs.vbar,
    } = value;
}

/// A register an action names: wN is xN's low half, written zero-extended.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ActionRegister {
    register: vroom_hv::debug::Register,
    w: bool,
}

impl ActionRegister {
    fn parse(v: &Value) -> Result<Self, String> {
        let name = v.as_str().ok_or("register: expected a name like \"x0\", \"w0\" or \"pc\"")?.to_ascii_lowercase();
        if let Some(n) = name.strip_prefix('w').and_then(|n| n.parse::<u8>().ok()).filter(|&n| n <= 30) {
            return Ok(Self { register: vroom_hv::debug::Register::X(n), w: true });
        }
        let register = vroom_hv::debug::Register::parse(&name).map_err(|e| format!("register {name}: {e}"))?;
        Ok(Self { register, w: false })
    }
    fn width(&self, value: u64) -> u64 {
        if self.w { value as u32 as u64 } else { value }
    }
    fn get(&self, regs: &Registers) -> u64 {
        self.width(register_value(regs, self.register))
    }
    fn name(&self) -> String {
        match self.register {
            vroom_hv::debug::Register::X(n) => format!("{}{n}", if self.w { 'w' } else { 'x' }),
            r => format!("{r:?}").to_ascii_lowercase(),
        }
    }
}

/// A number, hex string, or negative (wrapping) number or string.
fn action_value(v: &Value) -> Result<u64, String> {
    if let Some(n) = v.as_i64().filter(|n| *n < 0) {
        return Ok(n as u64);
    }
    match v.as_str().and_then(|s| s.strip_prefix('-')) {
        Some(s) => number(&json!(s)).map(u64::wrapping_neg),
        None => number(v),
    }
}

fn hex_bytes(hex: &str) -> Result<Vec<u8>, String> {
    if hex.len() % 2 != 0 || !hex.is_ascii() {
        return Err("expected an even number of hex digits".into());
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| "invalid hex byte".to_owned()))
        .collect()
}

#[derive(Debug, PartialEq)]
enum Operation {
    Set(ActionRegister, u64),
    Add(ActionRegister, u64),
    Write { register: ActionRegister, offset: u64, bytes: Vec<u8> },
}

/// What an action does to a stopped CPU.
#[derive(Debug, PartialEq)]
enum Effect {
    Register(vroom_hv::debug::Register, u64),
    Memory(u64, Vec<u8>),
}

/// One entry of a tracepoint's actions.
struct Action {
    operation: Operation,
    /// Only when this register holds this value.
    skip_if: Option<(ActionRegister, u64)>,
    /// A failure stops the VM instead of only being recorded.
    stop: bool,
    source: Value,
}

impl Action {
    fn parse(v: &Value) -> Result<Self, String> {
        const KEYS: &str = "set_register, add_register, write_memory, value, skip_if, stop";
        let object = v.as_object().ok_or("expected an object")?;
        if let Some(key) = object.keys().find(|k| !KEYS.split(", ").any(|known| known == k.as_str())) {
            return Err(format!("unknown key {key} ({KEYS})"));
        }
        let present = |k: &str| object.get(k).filter(|v| !v.is_null());
        let value = present("value").map(|v| action_value(v).map_err(|e| format!("value: {e}"))).transpose()?;
        let operation = match (present("set_register"), present("add_register"), present("write_memory")) {
            (Some(r), None, None) => Operation::Set(ActionRegister::parse(r)?, value.ok_or("set_register needs a value")?),
            (None, Some(r), None) => Operation::Add(ActionRegister::parse(r)?, value.ok_or("add_register needs a value")?),
            (None, None, Some(w)) => {
                if value.is_some() {
                    return Err("write_memory takes hex, not value".into());
                }
                let w = w.as_object().ok_or("write_memory: expected {register, offset, hex}")?;
                if let Some(key) = w.keys().find(|k| !matches!(k.as_str(), "register" | "offset" | "hex")) {
                    return Err(format!("write_memory: unknown key {key} (register, offset, hex)"));
                }
                let register = ActionRegister::parse(w.get("register").ok_or("write_memory needs the register holding the address")?)?;
                let offset = w.get("offset").filter(|v| !v.is_null()).map(action_value).transpose().map_err(|e| format!("offset: {e}"))?.unwrap_or(0);
                let bytes = hex_bytes(w.get("hex").and_then(Value::as_str).ok_or("write_memory needs hex bytes")?)
                    .map_err(|e| format!("write_memory hex: {e}"))?;
                if bytes.is_empty() || bytes.len() as u64 > MAX_TRACE_BYTES {
                    return Err(format!("write_memory writes 1..{MAX_TRACE_BYTES} bytes"));
                }
                Operation::Write { register, offset, bytes }
            }
            (None, None, None) => return Err("needs one of set_register, add_register, write_memory".into()),
            _ => return Err("only one of set_register, add_register, write_memory per action".into()),
        };
        let skip_if = present("skip_if")
            .map(|c| {
                let c = c.as_object().ok_or("skip_if: expected {register, equals}")?;
                if let Some(key) = c.keys().find(|k| !matches!(k.as_str(), "register" | "equals")) {
                    return Err(format!("skip_if: unknown key {key} (register, equals)"));
                }
                let register = ActionRegister::parse(c.get("register").ok_or("skip_if needs a register")?)?;
                let equals = action_value(c.get("equals").ok_or("skip_if needs equals")?).map_err(|e| format!("equals: {e}"))?;
                Ok::<_, String>((register, register.width(equals)))
            })
            .transpose()?;
        let stop = present("stop").map(|v| v.as_bool().ok_or("stop: expected a boolean")).transpose()?.unwrap_or(false);
        Ok(Action { operation, skip_if, stop, source: v.clone() })
    }

    /// The effect at these registers; None: skip_if's register does not
    /// hold its value.
    fn effect(&self, regs: &Registers) -> Result<Option<Effect>, String> {
        if self.skip_if.is_some_and(|(register, equals)| register.get(regs) != equals) {
            return Ok(None);
        }
        let effect = match &self.operation {
            Operation::Set(r, value) => Effect::Register(r.register, r.width(*value)),
            Operation::Add(r, value) => Effect::Register(r.register, r.width(r.get(regs).wrapping_add(*value))),
            Operation::Write { register, offset, bytes } => Effect::Memory(register.get(regs).wrapping_add(*offset), bytes.clone()),
        };
        if let Effect::Register(register, value) = effect {
            register.validate(value).map_err(|e| format!("{register:?} = {value:#x}: {e}"))?;
        }
        Ok(Some(effect))
    }
}

/// One entry of a tracepoint's captures: a register's value, or bytes at
/// the address a register holds plus an offset.
struct Capture {
    name: String,
    register: ActionRegister,
    /// (offset, length) of a memory capture.
    memory: Option<(u64, u64)>,
    source: Value,
}

/// What a capture recorded at one hit.
#[derive(Debug, PartialEq)]
enum Captured {
    Register(u64),
    /// address: None when the register held no pointer.
    Memory { address: Option<u64>, data: Result<Vec<u8>, String> },
}

impl Capture {
    fn parse(v: &Value) -> Result<Self, String> {
        let object = v.as_object().ok_or("expected an object")?;
        if let Some(key) = object.keys().find(|k| !matches!(k.as_str(), "name" | "register" | "offset" | "length")) {
            return Err(format!("unknown key {key} (name, register, offset, length)"));
        }
        let present = |k: &str| object.get(k).filter(|v| !v.is_null());
        let name = present("name").and_then(Value::as_str).filter(|n| !n.is_empty()).ok_or("needs a name")?.to_owned();
        let length = present("length").map(number).transpose().map_err(|e| format!("length: {e}"))?;
        let offset = present("offset").map(action_value).transpose().map_err(|e| format!("offset: {e}"))?;
        let register = match present("register") {
            Some(r) => ActionRegister::parse(r)?,
            None if length.is_some() || offset.is_some() => return Err("a memory capture needs the register holding the address".into()),
            None => ActionRegister::parse(&json!(name)).map_err(|e| format!("{e} (a memory capture names a register and a length)"))?,
        };
        let memory = match length {
            Some(length) if length == 0 || length > MAX_TRACE_BYTES => return Err(format!("length is 1..{MAX_TRACE_BYTES}")),
            Some(length) => Some((offset.unwrap_or(0), length)),
            None if offset.is_some() => return Err("offset needs a length".into()),
            None => None,
        };
        Ok(Capture { name, register, memory, source: v.clone() })
    }

    fn capture(&self, regs: &Registers, read: impl Fn(u64, &mut [u8]) -> Result<(), String>) -> Captured {
        let value = self.register.get(regs);
        let Some((offset, length)) = self.memory else {
            return Captured::Register(value);
        };
        if value == 0 {
            return Captured::Memory { address: None, data: Err(format!("{} is a null pointer", self.register.name())) };
        }
        let address = value.wrapping_add(offset);
        let mut bytes = vec![0; length as usize];
        Captured::Memory { address: Some(address), data: read(address, &mut bytes).map(|_| bytes).map_err(|e| format!("read {address:#x}: {e}")) }
    }
}

/// Apply an effect to a stopped CPU and to its registers as recorded.
fn apply_effect(vm: &Vmm, regs: &mut Registers, effect: Effect) -> Result<(), String> {
    match effect {
        Effect::Register(register, value) => {
            vm.debug_write_register(regs.cpu as usize, register, value).map_err(|e| e.to_string())?;
            set_register_value(regs, register, value);
        }
        Effect::Memory(address, bytes) => {
            // A kernel stop may still write its caller's user pointers.
            let mut walk = regs.clone();
            walk.tcr &= !(1 << 7);
            let mappings = AddressSpace { memory: vm, registers: &walk }
                .write_spans(address, bytes.len())
                .map_err(|e| format!("write {address:#x}: {e}"))?;
            let spans: Vec<_> = mappings.iter().map(|(pa, range)| (*pa, &bytes[range.clone()])).collect();
            vm.debug_write_physical(&spans).map_err(|e| format!("write {address:#x}: {e}"))?;
        }
    }
    Ok(())
}

/// Hardware breakpoint slots these breakpoints and tracepoints take.
fn breakpoint_slots(breakpoints: &[u64], tracepoints: &[Tracepoint]) -> usize {
    let mut addresses: Vec<u64> = breakpoints.to_vec();
    addresses.extend(tracepoints.iter().filter(|t| t.armed()).map(|t| t.address));
    addresses.sort_unstable();
    addresses.dedup();
    addresses.len()
}

fn same_space(process: &process::Process, regs: &Registers) -> bool {
    process.directory_table & 0x0000_ffff_ffff_f000 == regs.ttbr0 & 0x0000_ffff_ffff_f000
}

/// One entry of debug.tracepoints.
fn parse_tracepoint(i: usize, t: &Value, previous: &[Tracepoint], selected: Option<&process::Process>) -> Result<Tracepoint, String> {
    let fail = |e: String| format!("tracepoint {i}: {e}");
    let object = t.as_object().ok_or_else(|| fail("expected an object".into()))?;
    if let Some(key) = object.keys().find(|k| !matches!(k.as_str(), "address" | "register" | "length" | "captures" | "max_hits" | "stop" | "actions")) {
        return Err(fail(format!("unknown key {key} (address, register, length, captures, max_hits, stop, actions)")));
    }
    let present = |k: &str| object.get(k).filter(|v| !v.is_null());
    let address = number(present("address").ok_or_else(|| fail("missing address".into()))?).map_err(fail)?;
    if address & 3 != 0 {
        return Err(fail("address must be 4-byte aligned".into()));
    }
    let register = present("register")
        .map(|v| {
            let name = v.as_str().ok_or("register: expected a name like \"x0\"")?.to_ascii_lowercase();
            let register = vroom_hv::debug::Register::parse(&name).map_err(|e| format!("register {name}: {e}"))?;
            Ok::<_, String>((name, register))
        })
        .transpose()
        .map_err(fail)?;
    let length = present("length").map(number).transpose().map_err(fail)?.unwrap_or(0);
    if length > MAX_TRACE_BYTES {
        return Err(fail(format!("length is at most {MAX_TRACE_BYTES}")));
    }
    if length > 0 && register.is_none() {
        return Err(fail("length needs the register holding the pointer to read".into()));
    }
    let captures: Vec<Capture> = match present("captures") {
        None => Vec::new(),
        Some(list) => {
            let list = list.as_array().ok_or_else(|| fail("captures: expected an array".into()))?;
            if list.len() > MAX_TRACE_CAPTURES {
                return Err(fail(format!("at most {MAX_TRACE_CAPTURES} captures")));
            }
            list.iter()
                .enumerate()
                .map(|(j, c)| Capture::parse(c).map_err(|e| fail(format!("capture {j}: {e}"))))
                .collect::<Result<_, _>>()?
        }
    };
    for (j, c) in captures.iter().enumerate() {
        if captures[..j].iter().any(|d| d.name == c.name) {
            return Err(fail(format!("capture {j}: name {} is taken", c.name)));
        }
    }
    let max_hits = present("max_hits").map(number).transpose().map_err(fail)?;
    if max_hits == Some(0) {
        return Err(fail("max_hits is at least 1 (omit it for no limit)".into()));
    }
    let stop = present("stop").map(|v| v.as_bool().ok_or("stop: expected a boolean")).transpose().map_err(|e| fail(e.into()))?.unwrap_or(false);
    let actions = match present("actions") {
        None => Vec::new(),
        Some(list) => {
            let list = list.as_array().ok_or_else(|| fail("actions: expected an array".into()))?;
            if list.len() > MAX_TRACE_ACTIONS {
                return Err(fail(format!("at most {MAX_TRACE_ACTIONS} actions")));
            }
            list.iter()
                .enumerate()
                .map(|(j, a)| Action::parse(a).map_err(|e| fail(format!("action {j}: {e}"))))
                .collect::<Result<_, _>>()?
        }
    };
    // A user address traces in the selected process; with none, in all.
    let context = if address >> 63 == 0 {
        previous.iter().find(|t| t.address == address).map_or(selected, |t| t.context.as_ref()).cloned()
    } else {
        None
    };
    Ok(Tracepoint { address, register, length, captures, max_hits, stop, actions, hits: 0, context })
}

struct Session {
    vm: Vmm,
    breakpoints: Vec<u64>,
    breakpoint_contexts: BTreeMap<u64, process::Process>,
    tracepoints: Vec<Tracepoint>,
    traces: std::collections::VecDeque<TraceRecord>,
    trace_capacity: usize,
    trace_seq: u64,
    trace_dropped: u64,
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
    state_dir: PathBuf,
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
        for t in self.tracepoints.iter().filter(|t| t.armed()) {
            if !addresses.contains(&t.address) {
                addresses.push(t.address);
            }
        }
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
        if !matches!(op, "status" | "pause" | "shutdown" | "key" | "snapshot" | "snapshots" | "traces")
            && self.vm.state() != RunState::Paused
        {
            return Err("pause the VM before inspection or control".into());
        }
        match op {
            "status" => {
                let agent = self.vm.agent().status();
                let mut s = json!({"state": format!("{:?}", self.vm.state()), "cpus": self.vm.config().cpus, "breakpoints": self.breakpoints, "temporary":self.run_to.as_ref().map(|r|r.address), "tracepoints": self.tracepoints.iter().filter(|t| t.armed()).count(), "traces": self.traces.len(), "guest_agent": agent.connected.then_some(agent.agent)});
                if self.vm.state() == RunState::Paused {
                    s["note"] = json!("paused: vm.resume runs the guest; its network, agent and screen are frozen until then");
                }
                Ok(s)
            }
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
                if self.tracepoints.iter().any(|t| t.context.is_some()) {
                    let active = self.processes(cpu)?;
                    for process in self.tracepoints.iter_mut().filter_map(|t| t.context.as_mut()) {
                        *process = active
                            .iter()
                            .find(|p| p.eprocess == process.eprocess && p.pid == process.pid)
                            .ok_or("tracepoint process exited; clear its tracepoint")?
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
                let data = hex_bytes(hex)?;
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
                if breakpoint_slots(&self.breakpoints, &self.tracepoints) >= 4
                    && !self.execution_breakpoints().contains(&address)
                {
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
                let slots = breakpoint_slots(&addresses, &self.tracepoints);
                if slots > 4 {
                    return Err(format!(
                        "{} breakpoints and {} tracepoints need {slots} of the 4 hardware breakpoint slots; clear some (debug.tracepoints [])",
                        addresses.len(),
                        self.tracepoints.iter().filter(|t| t.armed()).count()
                    ));
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
            "tracepoints" => {
                let list = request["tracepoints"]
                    .as_array()
                    .ok_or("expected tracepoints array")?;
                let capacity = request.get("capacity").filter(|v| !v.is_null()).map(number).transpose()?;
                if capacity.is_some_and(|c| !(1..=MAX_TRACE_CAPACITY).contains(&c)) {
                    return Err(format!("capacity: 1..{MAX_TRACE_CAPACITY} records"));
                }
                let tracepoints = list
                    .iter()
                    .enumerate()
                    .map(|(i, t)| parse_tracepoint(i, t, &self.tracepoints, self.process.as_ref()))
                    .collect::<Result<Vec<_>, _>>()?;
                for (i, t) in tracepoints.iter().enumerate() {
                    if tracepoints[..i].iter().any(|u| u.address == t.address) {
                        return Err(format!("tracepoint {i}: {:#x} is traced already", t.address));
                    }
                }
                let slots = breakpoint_slots(&self.breakpoints, &tracepoints);
                if slots > 4 {
                    return Err(format!(
                        "{} tracepoints and {} breakpoints need {slots} hardware breakpoint slots; there are 4 (a tracepoint at a breakpoint's address shares its slot)",
                        tracepoints.len(),
                        self.breakpoints.len()
                    ));
                }
                let old = std::mem::replace(&mut self.tracepoints, tracepoints);
                if let Err(e) = self.configure() {
                    self.tracepoints = old;
                    let rollback = self.configure();
                    return Err(format!("{e}; rollback: {rollback:?}; VM remains paused"));
                }
                if let Some(capacity) = capacity {
                    self.trace_capacity = capacity as usize;
                    while self.traces.len() > self.trace_capacity {
                        self.traces.pop_front();
                        self.trace_dropped += 1;
                    }
                }
                Ok(json!({"tracepoints": self.tracepoints.iter().map(Tracepoint::json).collect::<Vec<_>>(),
                    "capacity": self.trace_capacity, "slots": slots}))
            }
            "traces" => {
                let limit = request.get("limit").filter(|v| !v.is_null()).map(number).transpose()?;
                let clear = match request.get("clear").filter(|v| !v.is_null()) {
                    Some(v) => v.as_bool().ok_or("clear: expected a boolean")?,
                    None => false,
                };
                let n = limit.map_or(self.traces.len(), |l| (l as usize).min(self.traces.len()));
                let records: Vec<Value> = if clear {
                    self.traces.drain(..n).map(|r| r.json()).collect()
                } else {
                    self.traces.iter().take(n).map(TraceRecord::json).collect()
                };
                Ok(json!({"records": records, "remaining": self.traces.len(), "recorded": self.trace_seq,
                    "dropped": self.trace_dropped, "capacity": self.trace_capacity,
                    "tracepoints": self.tracepoints.iter().map(Tracepoint::json).collect::<Vec<_>>()}))
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
            "snapshot" => {
                // Into this VM's snapshots (the app lists them), unless a
                // folder is given.
                let dir = match (request["path"].as_str(), request["name"].as_str()) {
                    (Some(p), _) => PathBuf::from(p),
                    (None, Some(n)) if !n.is_empty() && !n.contains('/') && !n.starts_with('.') => self.state_dir.join("snapshots").join(n),
                    (None, Some(n)) => return Err(format!("bad snapshot name {n:?}")),
                    (None, None) => {
                        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                        self.state_dir.join("snapshots").join(format!("snap-{t}"))
                    }
                };
                if dir.exists() {
                    return Err(format!("{} exists already", dir.display()));
                }
                let running = self.vm.state() == RunState::Running;
                if running {
                    self.vm.pause().map_err(|e| e.to_string())?;
                }
                // Saved without the debugger's traps.
                for cpu in 0..self.vm.config().cpus as usize {
                    self.vm.debug_configure(cpu, DebugConfig::default()).map_err(|e| e.to_string())?;
                }
                let saved = self.vm.snapshot(&dir, request["description"].as_str().unwrap_or("vroom-lab")).map_err(|e| e.to_string());
                self.configure()?;
                if running {
                    self.vm.resume().map_err(|e| e.to_string())?;
                }
                saved?;
                let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                Ok(json!({"name": name, "path": dir}))
            }
            "snapshots" => {
                let list = vroom_state::list(&self.state_dir).map_err(|e| e.to_string())?;
                Ok(Value::Array(
                    list.into_iter()
                        .map(|s| json!({"name": s.name, "path": s.path, "created_unix": s.created_unix, "description": s.description, "bytes": s.bytes}))
                        .collect(),
                ))
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

    /// A breakpoint stop at a tracepoint's address: run its actions and
    /// record it if it is armed and in its process. None: no tracepoint there.
    fn trace_hit(&mut self, regs: &Registers) -> Result<Option<TraceHit>, String> {
        let Some(index) = self.tracepoints.iter().position(|t| t.address == regs.pc) else {
            return Ok(None);
        };
        let t = &self.tracepoints[index];
        if !t.armed() || t.context.as_ref().is_some_and(|p| !same_space(p, regs)) {
            return Ok(Some(TraceHit { stop: false, moved: false }));
        }
        // A kernel stop may still read its caller's user pointers.
        let mut walk = regs.clone();
        walk.tcr &= !(1 << 7);
        let read = |address: u64, bytes: &mut [u8]| AddressSpace { memory: &self.vm, registers: &walk }.read(address, bytes).map_err(|e| e.to_string());
        let pointer = t.register.as_ref().map(|r| register_value(regs, r.1));
        let data = pointer.filter(|_| t.length > 0).map(|pointer| {
            let mut bytes = vec![0; t.length as usize];
            read(pointer, &mut bytes).map(|_| bytes)
        });
        let captures = t.captures.iter().map(|c| (c.name.clone(), c.capture(regs, read))).collect();
        // The record keeps the registers as hit; actions see earlier ones' writes.
        let mut current = regs.clone();
        let (mut actions_applied, mut action_error, mut failed_stop) = (0, None, false);
        for (j, action) in t.actions.iter().enumerate() {
            match action.effect(&current).and_then(|e| e.map(|e| apply_effect(&self.vm, &mut current, e)).transpose()) {
                Ok(Some(())) => actions_applied += 1,
                Ok(None) => {}
                Err(e) => {
                    action_error = Some(format!("action {j}: {e}"));
                    failed_stop = action.stop;
                    break;
                }
            }
        }
        let t = &mut self.tracepoints[index];
        t.hits += 1;
        let (hit, stop, exhausted) = (t.hits, t.stop || failed_stop, !t.armed());
        self.trace_seq += 1;
        if self.traces.len() >= self.trace_capacity {
            self.traces.pop_front();
            self.trace_dropped += 1;
        }
        self.traces.push_back(TraceRecord { seq: self.trace_seq, tracepoint: index, hit, regs: regs.clone(), pointer, data,
            captures, actions_applied, action_error });
        if exhausted {
            self.configure()?;
        }
        Ok(Some(TraceHit { stop, moved: current.pc != regs.pc }))
    }

    /// A step that a stop on another CPU paused before it ran: that CPU
    /// still sits on its breakpoint, so step it first.
    fn requeue_step(&mut self, stepping: Option<usize>) -> Result<(), String> {
        let Some(cpu) = stepping.filter(|c| !self.continue_cpus.contains(c)) else {
            return Ok(());
        };
        let state = self.vm.debug_registers(cpu).map_err(|e| e.to_string())?;
        let pc = Registers::from_state(&state).map_err(|e| e.to_string())?.pc;
        if self.execution_breakpoints().contains(&pc) {
            self.continue_cpus.push_front(cpu);
        }
        Ok(())
    }

    /// Tracepoints keep the event loop polling the VM instead of requests.
    fn tracing(&self) -> bool {
        self.continuing || (!self.tracepoints.is_empty() && self.vm.state() == RunState::Running)
    }
}

/// Start the VM, paused (cold boot, or `restore`: a snapshot's folder or
/// name), and the session around it.
fn boot(config_path: &std::path::Path, restore: Option<&std::path::Path>) -> Result<Session, String> {
    let config = vroom_config::VmConfig::load(config_path).map_err(|e| e.to_string())?;
    if config.nested {
        return Err("disable nested virtualization for kernel debugging".into());
    }
    let state_dir = config.default_state_dir(config_path);
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
    let start = restore
        .map(|r| restore_dir(r, &state_dir).map(StartMode::RestorePaused))
        .transpose()?
        .unwrap_or(StartMode::BootPaused);
    let mut vm = Vmm::start(
        config,
        hv,
        VmmOptions {
            serial_out: Box::new(serial),
            start,
            clipboard: None,
            ui: Some(vmm_viewer),
            state_dir: state_dir.clone(),
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
    let session = Session {
        vm,
        breakpoints: Vec::new(),
        breakpoint_contexts: BTreeMap::new(),
        tracepoints: Vec::new(),
        traces: Default::default(),
        trace_capacity: DEFAULT_TRACE_CAPACITY,
        trace_seq: 0,
        trace_dropped: 0,
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
        state_dir,
        continue_cpus: Default::default(),
        continuing: false,
        input,
    };
    session.configure()?;
    Ok(session)
}

fn run(args: Args) -> Result<(), String> {
    let args_agent_socket = args.agent_socket.clone();
    let mut session = boot(&args.config, args.restore.as_deref())?;
    output(json!({"event":"ready", "state":"Paused", "cpus": session.vm.config().cpus, "note": "paused: vm.resume runs the guest"}))?;
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
            for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
                // SAFETY: the handler only stores an atomic flag.
                unsafe { libc::signal(signal, on_stop_signal as extern "C" fn(libc::c_int) as libc::sighandler_t) };
            }
            report_ready();
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
        if STOP.load(std::sync::atomic::Ordering::Acquire) {
            session.vm.shutdown();
            output(json!({"event":"shutdown"}))?;
            return Ok(());
        }
        // Consume debug events first: RAM reads only happen after all-stop.
        // While tracing, wait on the VM's events and only peek at requests.
        let mut wait = if session.tracing() { Duration::from_millis(5) } else { Duration::ZERO };
        for _ in 0..EVENT_BURST {
            let Some(event) = session.vm.next_event(wait) else { break };
            wait = Duration::ZERO;
            match event {
                VmEvent::Debug(stop) => {
                    let was_running = session.vm.state() == RunState::Running;
                    let stepping = session.step_cpu.take();
                    session.vm.pause().map_err(|e| e.to_string())?;
                    session.configure()?;
                    session.watch_stopped = (stop.reason
                        == vroom_hv::debug::DebugReason::Watchpoint)
                        .then_some(stop.cpu as usize);
                    if stop.reason == vroom_hv::debug::DebugReason::SingleStep
                        && stepping != Some(stop.cpu as usize)
                        && was_running
                        && (session.continuing || stepping.is_none())
                    {
                        // The end of a step that another stop overtook.
                        if session.continuing {
                            session.requeue_step(stepping)?;
                            session.continue_next()?;
                        } else {
                            session.vm.resume().map_err(|e| e.to_string())?;
                        }
                        continue;
                    }
                    if session.continuing && stop.reason == vroom_hv::debug::DebugReason::SingleStep
                    {
                        session.continue_next()?;
                        continue;
                    }
                    let mut reason = format!("{:?}", stop.reason);
                    if stop.reason == vroom_hv::debug::DebugReason::Breakpoint {
                        let regs = session.registers(stop.cpu as usize, false)?;
                        let traced = session.trace_hit(&regs)?;
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
                        } else if !user_hit && traced.as_ref().is_some_and(|t| t.stop) {
                            reason = "Tracepoint".into();
                        } else if let Some(traced) = traced.filter(|_| !user_hit) {
                            // Recorded; a pause already asked for keeps the VM paused.
                            if was_running {
                                session.requeue_step(stepping)?;
                                if !traced.moved && !session.continue_cpus.contains(&(stop.cpu as usize)) {
                                    session.continue_cpus.push_back(stop.cpu as usize);
                                }
                                session.continuing = true;
                                session.continue_next()?;
                            }
                            continue;
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
        let wait = if session.tracing() { Duration::ZERO } else { Duration::from_millis(20) };
        match rx.recv_timeout(wait) {
            Ok(Input::Stdio(Ok(request))) => {
                let id = request.get("id").cloned().unwrap_or(Value::Null);
                let result = if request["op"] == "restore" {
                    let mut slot = Some(session);
                    let r = restore_in(&mut slot, &args.config, &request);
                    let Some(s) = slot else {
                        output(json!({"id":id,"error":r.err()}))?;
                        output(json!({"event":"shutdown"}))?;
                        return Ok(());
                    };
                    session = s;
                    r
                } else {
                    session.command(&request)
                };
                let response = match result {
                    Ok(result) => json!({"id":id,"result":result}),
                    Err(error) => json!({"id":id,"error":error}),
                };
                output(response)?;
                if session.vm.state() == RunState::Stopped {
                    return Ok(());
                }
            }
            Ok(Input::Stdio(Err(error))) => output(json!({"id":null,"error":error}))?,
            // Answered from its own thread: the guest runs meanwhile.
            Ok(Input::Agent(forward)) if forward.request["op"].as_str().is_some_and(|op| op.starts_with("guest_")) => {
                guest::run(&session.vm, &session.exports, forward);
            }
            Ok(Input::Agent(forward)) if forward.request["op"] == "restore" => {
                let mut slot = Some(session);
                let r = restore_in(&mut slot, &args.config, &forward.request);
                let _ = forward.reply.send(r);
                let Some(s) = slot else {
                    output(json!({"event":"shutdown"}))?;
                    return Ok(());
                };
                session = s;
            }
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

/// `restore`: this VM again, from one of its snapshots (or a snapshot
/// folder), paused. Checked first: a snapshot that can't start leaves the
/// running VM alone. Past that, a failure ends the lab: `slot` is empty
/// after it.
fn restore_in(slot: &mut Option<Session>, config_path: &std::path::Path, request: &Value) -> Result<Value, String> {
    let state_dir = slot.as_ref().map(|s| s.state_dir.clone()).ok_or("no VM")?;
    let given = request["name"].as_str().or(request["path"].as_str()).ok_or("missing name (or path)")?;
    let dir = restore_dir(std::path::Path::new(given), &state_dir)?;
    vroom_vmm::check_snapshot(&dir).map_err(|e| e.to_string())?;
    // The old VM lets go of its state dir, disks and network ports first.
    drop(slot.take());
    let session = boot(config_path, Some(&dir)).map_err(|e| format!("{e} (the lab stops: its VM is gone)"))?;
    *slot = Some(session);
    Ok(json!({"restored": dir, "state": "Paused", "note": "paused: vm.resume runs the guest"}))
}

fn main() {
    let args = Args::parse();
    // The VMM's warnings (a device's queue errors, a backend gone): stderr,
    // the socket's .log with --daemon.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    if args.daemon {
        if let Err(e) = daemonize(args.agent_socket.as_deref().expect("clap: requires agent_socket")) {
            eprintln!("vroom-lab: {e}");
            std::process::exit(1);
        }
    }
    if let Err(e) = run(args) {
        eprintln!("vroom-lab: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracepoints_parse_and_share_breakpoint_slots() {
        let t = parse_tracepoint(0, &json!({"address": "0xfffff80000001000", "register": "X1", "length": 16, "max_hits": 2}), &[], None).unwrap();
        assert_eq!((t.address, t.length, t.max_hits, t.stop), (0xfffff80000001000, 16, Some(2), false));
        assert_eq!(t.register.as_ref().unwrap().0, "x1");
        for bad in [
            json!({"address": "0xfffff80000001002"}),
            json!({"address": "0xfffff80000001000", "length": 8}),
            json!({"address": "0xfffff80000001000", "register": "x0", "length": 4097}),
            json!({"address": "0xfffff80000001000", "max_hits": 0}),
            json!({"address": "0xfffff80000001000", "lenght": 8}),
        ] {
            assert!(parse_tracepoint(0, &bad, &[], None).is_err(), "{bad}");
        }
        let mut traced: Vec<Tracepoint> = (0..3).map(|i| parse_tracepoint(0, &json!({"address": 0xfffff80000001000u64 + i * 4}), &[], None).unwrap()).collect();
        assert_eq!(breakpoint_slots(&[0xfffff80000001000, 0xfffff80000002000], &traced), 4);
        traced[0].hits = 1;
        traced[0].max_hits = Some(1);
        assert_eq!(breakpoint_slots(&[0xfffff80000002000, 0xfffff80000003000], &traced), 4, "a spent tracepoint frees its slot");
    }

    fn regs() -> Registers {
        let mut x = [0; 31];
        x[0] = 0xffff_ffff_0000_0005;
        x[21] = 0xfffff800_0000_2000;
        Registers { cpu: 1, x, pc: 0xfffff800_0000_1000, sp: 0xfffff800_0000_8000, pstate: 0x3c5, ttbr0: 0, ttbr1: 0, tcr: 0, sctlr: 0, vbar: 0, sve_vl: None }
    }

    fn action(v: Value) -> Action {
        Action::parse(&v).unwrap_or_else(|e| panic!("{v}: {e}"))
    }

    #[test]
    fn tracepoint_actions_parse() {
        use vroom_hv::debug::Register as R;
        let t = parse_tracepoint(0, &json!({"address": "0xfffff80000001000", "actions": [
            {"set_register": "x0", "value": 0},
            {"add_register": "PC", "value": 4},
            {"set_register": "w0", "value": "0xc0000022"},
            {"set_register": "x21", "value": "0x10"},
            {"write_memory": {"register": "x0", "offset": 0, "hex": "01000000"}},
            {"skip_if": {"register": "x0", "equals": 0}, "add_register": "pc", "value": 4, "stop": true},
        ]}), &[], None).unwrap();
        let ops: Vec<_> = t.actions.iter().map(|a| &a.operation).collect();
        let x0 = ActionRegister { register: R::X(0), w: false };
        assert_eq!(ops[0], &Operation::Set(x0, 0));
        assert_eq!(ops[1], &Operation::Add(ActionRegister { register: R::Pc, w: false }, 4));
        assert_eq!(ops[2], &Operation::Set(ActionRegister { register: R::X(0), w: true }, 0xc0000022));
        assert_eq!(ops[3], &Operation::Set(ActionRegister { register: R::X(21), w: false }, 0x10));
        assert_eq!(ops[4], &Operation::Write { register: x0, offset: 0, bytes: vec![1, 0, 0, 0] });
        assert_eq!((t.actions[5].skip_if, t.actions[5].stop), (Some((x0, 0)), true));
        assert_eq!(t.json()["actions"][1], json!({"add_register": "PC", "value": 4}));
        assert_eq!(action(json!({"add_register": "x1", "value": -8})).operation, Operation::Add(ActionRegister { register: R::X(1), w: false }, (-8i64) as u64));
        assert_eq!(action(json!({"add_register": "x1", "value": "-0x8"})).operation, Operation::Add(ActionRegister { register: R::X(1), w: false }, (-8i64) as u64));
        for (bad, message) in [
            (json!({"set_regsiter": "x0", "value": 0}), "unknown key set_regsiter"),
            (json!({"set_register": "x0"}), "needs a value"),
            (json!({"set_register": "x31", "value": 0}), "register x31"),
            (json!({"set_register": "w31", "value": 0}), "register w31"),
            (json!({"set_register": "x0", "add_register": "x1", "value": 0}), "only one of"),
            (json!({"value": 0}), "needs one of"),
            (json!({"write_memory": {"register": "x0", "hex": "010"}}), "write_memory hex"),
            (json!({"write_memory": {"register": "x0", "hex": ""}}), "1..4096 bytes"),
            (json!({"write_memory": {"register": "x0", "hex": "00", "length": 1}}), "write_memory: unknown key length"),
            (json!({"write_memory": {"hex": "00"}}), "needs the register"),
            (json!({"write_memory": {"register": "x0", "hex": "00"}, "value": 1}), "takes hex"),
            (json!({"skip_if": {"register": "x0", "equal": 0}, "add_register": "pc", "value": 4}), "skip_if: unknown key equal"),
            (json!({"skip_if": {"register": "x0"}, "add_register": "pc", "value": 4}), "needs equals"),
            (json!({"set_register": "x0", "value": 0, "stop": "yes"}), "stop: expected a boolean"),
        ] {
            let e = parse_tracepoint(2, &json!({"address": "0xfffff80000001000", "actions": [{"set_register": "x1", "value": 1}, bad]}), &[], None)
                .err()
                .unwrap_or_else(|| panic!("{bad} parsed"));
            assert!(e.starts_with("tracepoint 2: action 1: ") && e.contains(message), "{bad}: {e}");
        }
        assert!(parse_tracepoint(0, &json!({"address": "0xfffff80000001000", "actions": {}}), &[], None).err().unwrap().contains("expected an array"));
        let many = vec![json!({"set_register": "x0", "value": 0}); MAX_TRACE_ACTIONS + 1];
        assert!(parse_tracepoint(0, &json!({"address": "0xfffff80000001000", "actions": many}), &[], None).is_err());
    }

    #[test]
    fn tracepoint_actions_set_and_add_registers() {
        use vroom_hv::debug::Register as R;
        let r = regs();
        let effect = |v: Value| action(v).effect(&r);
        assert_eq!(effect(json!({"set_register": "x0", "value": 0})), Ok(Some(Effect::Register(R::X(0), 0))));
        assert_eq!(effect(json!({"set_register": "x21", "value": "0x1234"})), Ok(Some(Effect::Register(R::X(21), 0x1234))));
        // wN writes zero-extend; adds wrap at 32 bits.
        assert_eq!(effect(json!({"set_register": "w0", "value": "0x100000001"})), Ok(Some(Effect::Register(R::X(0), 1))));
        assert_eq!(effect(json!({"add_register": "w0", "value": "0xfffffffc"})), Ok(Some(Effect::Register(R::X(0), 1))));
        assert_eq!(effect(json!({"add_register": "x0", "value": 1})), Ok(Some(Effect::Register(R::X(0), 0xffff_ffff_0000_0006))));
        assert_eq!(effect(json!({"add_register": "pc", "value": 4})), Ok(Some(Effect::Register(R::Pc, 0xfffff800_0000_1004))));
        assert_eq!(effect(json!({"add_register": "pc", "value": -4})), Ok(Some(Effect::Register(R::Pc, 0xfffff800_0000_0ffc))));
        assert!(effect(json!({"add_register": "pc", "value": 2})).unwrap_err().contains("Pc"), "misaligned PC");
        assert_eq!(
            effect(json!({"write_memory": {"register": "x21", "offset": -16, "hex": "01000000"}})),
            Ok(Some(Effect::Memory(0xfffff800_0000_1ff0, vec![1, 0, 0, 0])))
        );
        let mut r = regs();
        set_register_value(&mut r, R::Pc, 0x4000);
        set_register_value(&mut r, R::X(30), 7);
        assert_eq!((r.pc, r.x[30]), (0x4000, 7));
    }

    #[test]
    fn tracepoint_actions_skip_if() {
        use vroom_hv::debug::Register as R;
        let skip = action(json!({"skip_if": {"register": "x0", "equals": 0}, "add_register": "pc", "value": 4}));
        let mut r = regs();
        assert_eq!(skip.effect(&r), Ok(None), "x0 is not 0");
        r.x[0] = 0;
        assert_eq!(skip.effect(&r), Ok(Some(Effect::Register(R::Pc, 0xfffff800_0000_1004))));
        // wN compares the low half; equals is truncated to it.
        let w = action(json!({"skip_if": {"register": "w0", "equals": "0x100000005"}, "set_register": "x1", "value": 1}));
        let r = regs();
        assert_eq!(w.effect(&r), Ok(Some(Effect::Register(R::X(1), 1))));
        let x = action(json!({"skip_if": {"register": "x0", "equals": 5}, "set_register": "x1", "value": 1}));
        assert_eq!(x.effect(&r), Ok(None), "x0's high half differs");
        let negative = action(json!({"skip_if": {"register": "x0", "equals": -1}, "set_register": "x1", "value": 1}));
        let mut r = regs();
        r.x[0] = u64::MAX;
        assert_eq!(negative.effect(&r), Ok(Some(Effect::Register(R::X(1), 1))));
    }
    fn tracepoint(v: Value) -> Result<Tracepoint, String> {
        let mut v = v;
        v["address"] = json!("0xfffff80000001000");
        parse_tracepoint(0, &v, &[], None)
    }

    #[test]
    fn tracepoint_captures_parse() {
        use vroom_hv::debug::Register as R;
        let t = tracepoint(json!({"register": "x0", "length": 8, "captures": [
            {"name": "w2"},
            {"name": "X2"},
            {"name": "lr"},
            {"name": "fp"},
            {"name": "stack", "register": "sp", "offset": 8, "length": 16},
            {"name": "ptr", "register": "x0", "length": 32},
            {"name": "back", "register": "x21", "offset": "-0x10", "length": 4},
            {"name": "status", "register": "w0"},
        ]}))
        .unwrap();
        let parsed: Vec<_> = t.captures.iter().map(|c| (c.name.as_str(), c.register, c.memory)).collect();
        let reg = |register, w| ActionRegister { register, w };
        assert_eq!(parsed, [
            ("w2", reg(R::X(2), true), None),
            ("X2", reg(R::X(2), false), None),
            ("lr", reg(R::X(30), false), None),
            ("fp", reg(R::X(29), false), None),
            ("stack", reg(R::Sp, false), Some((8, 16))),
            ("ptr", reg(R::X(0), false), Some((0, 32))),
            ("back", reg(R::X(21), false), Some(((-16i64) as u64, 4))),
            ("status", reg(R::X(0), true), None),
        ]);
        assert_eq!((t.register.as_ref().unwrap().0.as_str(), t.length), ("x0", 8), "register/length still parse");
        assert_eq!(t.json()["captures"][4], json!({"name": "stack", "register": "sp", "offset": 8, "length": 16}));
        for (bad, message) in [
            (json!({"name": "w31"}), "register w31"),
            (json!({"name": "x31"}), "register x31"),
            (json!({"name": "r0"}), "register r0"),
            (json!({"name": "buf", "length": 8}), "needs the register"),
            (json!({"name": "buf", "register": "x0", "offset": 8}), "offset needs a length"),
            (json!({"name": "buf", "register": "x0", "length": 0}), "length is 1..4096"),
            (json!({"name": "buf", "register": "x0", "length": 4097}), "length is 1..4096"),
            (json!({"name": "buf", "register": "q0", "length": 8}), "register q0"),
            (json!({"register": "x0"}), "needs a name"),
            (json!({"name": ""}), "needs a name"),
            (json!({"name": "x0", "lenght": 8}), "unknown key lenght"),
            (json!("x0"), "expected an object"),
        ] {
            let e = tracepoint(json!({"captures": [{"name": "x1"}, bad]})).err().unwrap_or_else(|| panic!("{bad} parsed"));
            assert!(e.starts_with("tracepoint 0: capture 1: ") && e.contains(message), "{bad}: {e}");
        }
        let e = tracepoint(json!({"captures": [{"name": "a", "register": "x0"}, {"name": "a", "register": "x1"}]})).err().unwrap();
        assert!(e.contains("capture 1: name a is taken"), "{e}");
        assert!(tracepoint(json!({"captures": {}})).err().unwrap().contains("expected an array"));
        assert!(tracepoint(json!({"captures": vec![json!({"name": "x0"}); MAX_TRACE_CAPTURES + 1]})).is_err());
    }

    fn no_memory(address: u64, _: &mut [u8]) -> Result<(), String> {
        Err(format!("{address:#x} is not mapped"))
    }

    #[test]
    fn tracepoint_captures_truncate_w_registers() {
        let mut r = regs();
        r.x[2] = 0xdead_beef_8000_0001;
        let captured = |name: &str| tracepoint(json!({"captures": [{"name": name}]})).unwrap().captures[0].capture(&r, no_memory);
        assert_eq!(captured("w2"), Captured::Register(0x8000_0001), "low half, zero-extended");
        assert_eq!(captured("x2"), Captured::Register(0xdead_beef_8000_0001));
        assert_eq!(captured("w0"), Captured::Register(5));
        assert_eq!(captured("sp"), Captured::Register(0xfffff800_0000_8000));
    }

    #[test]
    fn tracepoint_captures_several_and_a_failing_one() {
        let t = tracepoint(json!({"captures": [
            {"name": "w0"},
            {"name": "x0"},
            {"name": "stack", "register": "sp", "offset": 8, "length": 4},
            {"name": "unmapped", "register": "x21", "length": 4},
            {"name": "null", "register": "x3", "length": 4},
            {"name": "ptr", "register": "x21", "offset": "0x100", "length": 2},
        ]}))
        .unwrap();
        let r = regs();
        // Mapped: the 64 bytes above sp, and x21 + 0x100.
        let read = |address: u64, bytes: &mut [u8]| {
            let mapped = |base: u64, size: u64| address >= base && address + bytes.len() as u64 <= base + size;
            if !(mapped(r.sp, 64) || mapped(r.x[21] + 0x100, 16)) {
                return no_memory(address, bytes);
            }
            for (i, b) in bytes.iter_mut().enumerate() {
                *b = (address as u8).wrapping_add(i as u8);
            }
            Ok(())
        };
        let record = TraceRecord { seq: 1, tracepoint: 0, hit: 1, regs: r.clone(), pointer: None, data: None,
            captures: t.captures.iter().map(|c| (c.name.clone(), c.capture(&r, read))).collect(), actions_applied: 0, action_error: None };
        let v = record.json();
        assert_eq!(v["captures"], json!({"w0": 5, "x0": 0xffff_ffff_0000_0005u64, "stack": "08090a0b", "ptr": "0001"}));
        assert_eq!(v["capture_errors"], json!({"unmapped": "read 0xfffff80000002000: 0xfffff80000002000 is not mapped", "null": "x3 is a null pointer"}));
        assert_eq!(v["capture_addresses"], json!({"stack": 0xfffff800_0000_8008u64, "unmapped": 0xfffff800_0000_2000u64, "ptr": 0xfffff800_0000_2100u64}));
        let plain = TraceRecord { captures: Vec::new(), ..record };
        assert!(plain.json().get("captures").is_none(), "records of tracepoints without captures are unchanged");
    }
}

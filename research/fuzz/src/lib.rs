//! The optional vroom research runner.
//!
//! This crate owns the fuzzing policy: inputs, mutation, corpus files, bug
//! classification, coverage maps and the JSON records consumed by the TUI.
//! The VMM only supplies paused execution, private RAM reset and debug stops.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use vroom_kernel::arm64::{AddressSpace, Registers};

pub mod blocks;
pub mod campaign;
pub mod cmplog;
pub mod hub;
pub mod sync;
pub mod trace;
pub mod triage;
pub mod tui;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration: {0}")]
    Config(String),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("vmm: {0}")]
    Vmm(#[from] vroom_vmm::Error),
    #[error("hypervisor: {0}")]
    Hv(#[from] vroom_hv::Error),
    #[error("invalid harness: {0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A basic block known to the coverage database. Analysis manifests can keep
/// portable VAs; instrumentation resolves them to GPAs for this snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CoverageBlock {
    pub id: u64,
    /// Resolved from `va` when the campaign opens. Keeping GPA optional lets
    /// the analysis manifest stay portable across guest runs and ASLR bases.
    #[serde(default)]
    pub gpa: Option<u64>,
    pub va: Option<u64>,
    pub size: u32,
    /// In-function CFG successors used to rearm a small set of hardware
    /// breakpoints after a block is reached. Empty for older manifests.
    #[serde(default)]
    pub successors: Vec<u64>,
    #[serde(default)]
    pub module: String,
    #[serde(default)]
    pub function: String,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Harness {
    pub name: String,
    pub config: PathBuf,
    pub snapshot: PathBuf,
    pub state_dir: PathBuf,
    pub input_gpa: u64,
    pub input_capacity: usize,
    /// CPU whose saved execution context owns the target call. Kernel dispatch
    /// can be entered on any CPU; this defaults to CPU 0 for older harnesses.
    #[serde(default)]
    pub cpu: usize,
    /// Optional guest virtual address of the input buffer. It must translate
    /// to `input_gpa`; the runner passes or stores it for each testcase.
    #[serde(default)]
    pub input_va: Option<u64>,
    /// Treat the first four testcase bytes as a little-endian IOCTL code.
    #[serde(default)]
    pub ioctl_prefix: bool,
    /// ARM64 register names used by a call-site snapshot (for example HEVD's
    /// x1/x2/x3 DeviceIoControl arguments).
    #[serde(default)]
    pub ioctl_register: Option<String>,
    /// Optional target-specific IOCTL field in the saved handler state. When
    /// set, testcase IOCTL values are written here instead of to a call ABI
    /// register. This supports a snapshot taken at a driver's IRP handler.
    #[serde(default)]
    pub ioctl_va: Option<u64>,
    /// Optional guest VA of a saved pointer field that should point at
    /// `input_va` for each testcase (for example an IRP buffer pointer).
    #[serde(default)]
    pub input_pointer_va: Option<u64>,
    /// Optional guest VA of a saved 32-bit input-length field.
    #[serde(default)]
    pub input_length_va: Option<u64>,
    /// Optional guest VA of a saved 32-bit output-length field. Some drivers
    /// use this field as the copy size for METHOD_NEITHER requests.
    #[serde(default)]
    pub output_length_va: Option<u64>,
    #[serde(default)]
    pub input_register: Option<String>,
    #[serde(default)]
    pub input_length_register: Option<String>,
    /// Address immediately after the target call. A breakpoint here ends a
    /// successful iteration instead of waiting for the timeout.
    #[serde(default)]
    pub stop_pc: Option<u64>,
    /// First instrumented block to stop at. If omitted, use the snapshot PC.
    /// A user-mode address makes a client harness: the snapshot stands in a
    /// program that calls into the target (`target`) with each input, and
    /// stops at `stop_pc` in it; blocks coverage, the whole guest running.
    #[serde(default)]
    pub entry_pc: Option<u64>,
    /// Treat a guest reset as a kernel bugcheck. This is useful for drivers
    /// such as HEVD whose deliberate faults reset the lab guest.
    #[serde(default)]
    pub reset_is_bugcheck: bool,
    /// Guest VA of the kernel's bugcheck entry (Windows: `KeBugCheckEx`,
    /// moved to `KeBugCheck2` when the PDB has it: some bugchecks, like
    /// an exception in a system service, start there). Any thread reaching it ends the input with a bugcheck finding before
    /// the guest crashes, and the VM is reset in place instead of restored
    /// from disk. Takes a hardware breakpoint beyond the four the coverage
    /// breakpoints use.
    #[serde(default)]
    pub bugcheck_pc: Option<u64>,
    /// Reopen the VM after this many completed inputs. Zero keeps the COW VM
    /// resident; small values are useful on hosts whose hypervisor retains
    /// stale mappings across remaps.
    #[serde(default)]
    pub reopen_after: usize,
    /// Run each testcase on `cpu` alone, with its IRQs and FIQs masked and
    /// the other CPUs parked, so the target thread is never preempted and
    /// the rest of the guest doesn't run (or dirty RAM) meanwhile. The VM
    /// is reset after each testcase, so the masking never leaks. A target
    /// that waits on another CPU or an interrupt times out instead.
    #[serde(default)]
    pub isolate_cpu: bool,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    /// Client harnesses: keep the guest running this long after `stop_pc`,
    /// so what the calls left in flight (asynchronous completions, flushes,
    /// workers) runs within the input: its coverage and faults count.
    #[serde(default)]
    pub settle_ms: u64,
    #[serde(default)]
    pub max_input: usize,
    /// `edges` (default): follow the path with hardware breakpoints on each
    /// block's successors; every block an input reaches costs an exit, and
    /// edges and hit counts are recorded. `blocks`: a BRK on every block not
    /// covered yet, removed for good once hit; covered paths run without
    /// exits, only new blocks are recorded (no edges, no hit counts).
    #[serde(default)]
    pub coverage_mode: CoverageMode,
    #[serde(default)]
    pub coverage: Vec<CoverageBlock>,
    /// Optional block manifest. Relative paths are resolved next to the
    /// harness, which keeps target block lists separate from run settings.
    #[serde(default)]
    pub coverage_file: Option<PathBuf>,
    /// Optional CTL_CODE range for one-IOCTL-at-a-time snapshot probing.
    #[serde(default)]
    pub ioctl_probe: Option<IoctlProbe>,
    #[serde(default)]
    pub detectors: Vec<String>,
    /// Driver-specific exception dispatch PCs. Reaching one records a
    /// exception-dispatch finding even when the guest continues normally.
    #[serde(default)]
    pub exception_sites: Vec<u64>,
    /// The module under test, by name (`clfs.sys`), when `entry_pc` isn't
    /// in it (a user-mode client calling into a driver). Default: the image
    /// `entry_pc` is in.
    #[serde(default)]
    pub target: Option<String>,
    /// A client harness: the target's functions its calls enter by (the
    /// IRP dispatch), by name in the block list (a part of it will do), at
    /// most three. Comparison logging runs the guest at full speed to one
    /// and steps the target from there; with none, from the first new
    /// block (BRK) the client's thread reaches in the target.
    #[serde(default)]
    pub entries: Vec<String>,
    /// The input is calls for a client to make, one after the other (state
    /// carries from call to call): each `u32 code | u16 length | payload`,
    /// little-endian. Mutations also insert, drop, repeat, swap and splice
    /// calls.
    #[serde(default)]
    pub sequence: bool,
    /// Hardware watched memory ranges. Use `poisoned` for allocator redzones
    /// or freed objects whose address is known from a target harness.
    #[serde(default)]
    pub watchpoints: Vec<MemoryWatch>,
}

impl Harness {
    /// The snapshot stands in a user-mode program calling into the target.
    pub fn client(&self) -> bool {
        self.entry_pc.is_some_and(|pc| pc >> 63 == 0)
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CoverageMode {
    #[default]
    Edges,
    Blocks,
}

/// BRK immediate of the `blocks` coverage breakpoints ("vm"); other BRKs
/// (Windows uses 0xf000..) still go to the guest.
pub const COVERAGE_BRK: u16 = 0x766d;

fn brk_instruction(immediate: u16) -> [u8; 4] {
    (0xd420_0000u32 | u32::from(immediate) << 5).to_le_bytes()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryWatch {
    #[serde(default)]
    pub address: u64,
    pub size: u8,
    #[serde(default)]
    pub read: bool,
    #[serde(default)]
    pub write: bool,
    #[serde(default)]
    pub poisoned: bool,
    #[serde(default)]
    pub double_fetch: bool,
    /// When set, watch this offset from the current input's end instead of
    /// `address`. A negative value watches within the input buffer.
    #[serde(default)]
    pub input_end_offset: Option<i32>,
    /// Fill the watched bytes with this value before running the testcase.
    /// This makes a redzone watch an actual poison marker, not just a label.
    #[serde(default)]
    pub poison_value: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IoctlProbe {
    pub device_type: u16,
    pub function_start: u16,
    pub function_end: u16,
    pub methods: Vec<u8>,
    pub access: Vec<u8>,
    /// A known-invalid code used to identify the driver's default path.
    pub baseline_ioctl: u32,
}

impl IoctlProbe {
    pub fn codes(&self) -> Vec<u32> {
        let mut codes = BTreeSet::new();
        for function in self.function_start..=self.function_end {
            for &method in &self.methods {
                for &access in &self.access {
                    let code = ((self.device_type as u32) << 16)
                        | ((access as u32) << 14)
                        | ((function as u32) << 2)
                        | method as u32;
                    codes.insert(code);
                }
            }
        }
        codes.into_iter().collect()
    }
}

fn default_timeout() -> u64 {
    250
}

impl Harness {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path)?;
        let mut h: Self = serde_json::from_slice(&bytes)?;
        if let Some(blocks) = &h.coverage_file {
            let blocks = if blocks.is_absolute() {
                blocks.clone()
            } else {
                path.parent().unwrap_or_else(|| Path::new(".")).join(blocks)
            };
            h.coverage = serde_json::from_slice(&fs::read(blocks)?)?;
        }
        if h.name.trim().is_empty() || h.input_capacity == 0 {
            return Err(Error::Invalid(
                "name and input_capacity are required".into(),
            ));
        }
        if h.max_input == 0 {
            h.max_input = h.input_capacity;
        }
        let prefix_len = if h.ioctl_prefix { 4 } else { 0 };
        if h.max_input < prefix_len {
            return Err(Error::Invalid(format!(
                "max_input must be at least {prefix_len} bytes for this harness"
            )));
        }
        if h.max_input - prefix_len > h.input_capacity {
            return Err(Error::Invalid(
                "max_input payload exceeds input_capacity".into(),
            ));
        }
        if h.ioctl_prefix && h.ioctl_register.is_none() && h.ioctl_va.is_none() {
            return Err(Error::Invalid(
                "ioctl_prefix needs ioctl_register or ioctl_va".into(),
            ));
        }
        if h.ioctl_register.is_some() && h.ioctl_va.is_some() {
            return Err(Error::Invalid(
                "choose ioctl_register or ioctl_va, not both".into(),
            ));
        }
        if h.input_register.is_some() && h.input_pointer_va.is_some() {
            return Err(Error::Invalid(
                "choose input_register or input_pointer_va, not both".into(),
            ));
        }
        if h.input_length_register.is_some() && h.input_length_va.is_some() {
            return Err(Error::Invalid(
                "choose input_length_register or input_length_va, not both".into(),
            ));
        }
        if h.input_va.is_some() != (h.input_register.is_some() || h.input_pointer_va.is_some()) {
            return Err(Error::Invalid(
                "input_va needs input_register or input_pointer_va".into(),
            ));
        }
        if h.input_pointer_va.is_some_and(|va| va & 7 != 0)
            || h.input_length_va.is_some_and(|va| va & 3 != 0)
            || h.output_length_va.is_some_and(|va| va & 3 != 0)
        {
            return Err(Error::Invalid(
                "input pointer fields must be 8-byte aligned and length fields 4-byte aligned"
                    .into(),
            ));
        }
        if h.stop_pc.is_some_and(|pc| pc & 3 != 0)
            || h.entry_pc.is_some_and(|pc| pc & 3 != 0)
            || h.bugcheck_pc.is_some_and(|pc| pc & 3 != 0)
        {
            return Err(Error::Invalid(
                "entry_pc, stop_pc and bugcheck_pc must be instruction aligned".into(),
            ));
        }
        if h.coverage.iter().any(|b| {
            b.size == 0
                || b.va.is_some_and(|va| va & 3 != 0)
                || b.gpa.is_some_and(|gpa| gpa & 3 != 0)
                || (b.va.is_none() && b.gpa.is_none())
        }) {
            return Err(Error::Invalid(
                "coverage blocks need an aligned GPA or VA and a non-zero size".into(),
            ));
        }
        if h.coverage.iter().any(|block| block.successors.len() > 4) {
            return Err(Error::Invalid(
                "a coverage block has more than four successors; split its branch table".into(),
            ));
        }
        if h.stop_pc.is_some() && h.coverage.iter().any(|block| block.successors.len() == 4) {
            return Err(Error::Invalid(
                "stop_pc leaves room for at most three block successors".into(),
            ));
        }
        if h.exception_sites.iter().any(|pc| pc & 3 != 0) {
            return Err(Error::Invalid(
                "exception_sites must be instruction aligned".into(),
            ));
        }
        if h.settle_ms != 0 && (!h.client() || h.settle_ms >= h.timeout_ms) {
            return Err(Error::Invalid("settle_ms is for client harnesses, below timeout_ms".into()));
        }
        if h.client() {
            if h.coverage_mode != CoverageMode::Blocks || h.isolate_cpu || h.target.is_none() || h.stop_pc.is_none() {
                return Err(Error::Invalid(
                    "a client harness (user-mode entry_pc) needs coverage_mode blocks, isolate_cpu off, target and stop_pc".into(),
                ));
            }
        }
        // DebugStop does not expose the fired WVR index yet, so one watched
        // range is the only unambiguous configuration supported by the runner.
        if h.exception_sites.len() > 4 || h.watchpoints.len() > 1 {
            return Err(Error::Invalid(
                "ARM64 supports four exception sites and one unambiguous watched range".into(),
            ));
        }
        for watch in &h.watchpoints {
            if watch.input_end_offset.is_some() && h.input_va.is_none() {
                return Err(Error::Invalid(
                    "input_end_offset needs input_va in the harness".into(),
                ));
            }
            vroom_hv::debug::Watchpoint {
                address: watch.address,
                size: watch.size,
                read: watch.read,
                write: watch.write,
            }
            .arm64_control()
            .map_err(|error| Error::Invalid(error.to_string()))?;
            if watch.poisoned && !watch.read && !watch.write {
                return Err(Error::Invalid(
                    "poisoned watchpoints need read or write enabled".into(),
                ));
            }
            if watch.double_fetch && (!watch.read || watch.write) {
                return Err(Error::Invalid(
                    "double_fetch needs a read-only watchpoint".into(),
                ));
            }
        }
        if let Some(probe) = &h.ioctl_probe {
            if !h.ioctl_prefix {
                return Err(Error::Invalid("ioctl_probe needs ioctl_prefix".into()));
            }
            if probe.function_start > probe.function_end || probe.function_end > 0x0fff {
                return Err(Error::Invalid(
                    "ioctl_probe function range must be ordered and at most 0xfff".into(),
                ));
            }
            if probe.methods.is_empty()
                || probe.methods.iter().any(|value| *value > 3)
                || probe.access.is_empty()
                || probe.access.iter().any(|value| *value > 3)
            {
                return Err(Error::Invalid(
                    "ioctl_probe needs method and access values from 0 through 3".into(),
                ));
            }
            if probe.codes().len() > 65_536 {
                return Err(Error::Invalid("ioctl_probe has too many candidates".into()));
            }
        }
        let mut ids = BTreeSet::new();
        let mut gpas = BTreeSet::new();
        let mut vas = BTreeSet::new();
        for block in &h.coverage {
            if !ids.insert(block.id) {
                return Err(Error::Invalid(format!(
                    "duplicate coverage block id {}",
                    block.id
                )));
            }
            if block.gpa.is_some_and(|gpa| !gpas.insert(gpa)) {
                return Err(Error::Invalid(format!(
                    "duplicate coverage GPA {:?}",
                    block.gpa
                )));
            }
            if let Some(va) = block.va {
                if !vas.insert(va) {
                    return Err(Error::Invalid(format!("duplicate coverage VA {va:#x}")));
                }
            }
        }
        Ok(h)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BlockCoverage {
    pub hits: u64,
    pub first_input: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CoverageMap {
    pub executions: u64,
    pub new_edges: u64,
    pub blocks: BTreeMap<u64, BlockCoverage>,
    pub edges: BTreeSet<(u64, u64)>,
}

impl CoverageMap {
    pub fn observe(&mut self, block: &CoverageBlock, input: &str) -> bool {
        let row = self.blocks.entry(block.id).or_default();
        let fresh = row.hits == 0;
        row.hits += 1;
        if fresh {
            row.first_input = Some(input.to_string());
        }
        fresh
    }

    pub fn observe_edge(&mut self, from: u64, to: u64) -> bool {
        let fresh = self.edges.insert((from, to));
        if fresh {
            self.new_edges += 1;
        }
        fresh
    }

    /// Take coverage another instance reached for `input`. Returns the
    /// blocks and edges that are new here.
    pub fn merge(&mut self, blocks: &[u64], edges: &[(u64, u64)], input: &str) -> (usize, usize) {
        let mut new_blocks = 0;
        for id in blocks {
            let row = self.blocks.entry(*id).or_default();
            if row.hits == 0 {
                row.first_input = Some(input.to_string());
                new_blocks += 1;
            }
            row.hits += 1;
        }
        let new_edges = edges
            .iter()
            .filter(|(from, to)| self.observe_edge(*from, *to))
            .count();
        (new_blocks, new_edges)
    }

    pub fn finish_execution(&mut self) {
        self.executions += 1;
    }

    pub fn covered(&self) -> usize {
        self.blocks.values().filter(|b| b.hits > 0).count()
    }

    pub fn summary<'a>(
        &self,
        blocks: impl IntoIterator<Item = &'a CoverageBlock>,
    ) -> CoverageSummary {
        let all: Vec<_> = blocks.into_iter().collect();
        let mut functions = BTreeMap::<String, (usize, usize, u64)>::new();
        for block in all.iter() {
            let row = self.blocks.get(&block.id);
            let entry = functions
                .entry(if block.function.is_empty() {
                    "<unknown>".into()
                } else {
                    block.function.clone()
                })
                .or_default();
            entry.0 += 1;
            if row.is_some_and(|r| r.hits > 0) {
                entry.1 += 1;
            }
            entry.2 += row.map_or(0, |r| r.hits);
        }
        CoverageSummary {
            total_blocks: all.len(),
            covered_blocks: self.covered(),
            executions: self.executions,
            new_edges: self.new_edges,
            functions,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageSummary {
    pub total_blocks: usize,
    pub covered_blocks: usize,
    pub executions: u64,
    pub new_edges: u64,
    pub functions: BTreeMap<String, (usize, usize, u64)>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum BugClass {
    AccessViolation,
    BugCheck,
    PoolCorruption,
    DoubleFree,
    UseAfterFree,
    OutOfBounds,
    IntegerOverflow,
    Uninitialized,
    ExceptionDispatch,
    PoisonedAccess,
    DoubleFetch,
    Timeout,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub class: BugClass,
    pub detail: String,
    pub pc: Option<u64>,
    pub input: String,
    /// The crashing thread's call stack, symbolized (bugchecks, exception
    /// sites).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stack: Vec<String>,
}

/// Findings with the same signature are the same bug: the class and the
/// three innermost frames of the faulting code, by function (where in the
/// function, and the input that got there, vary between hits of one bug);
/// without a stack, the PC. A stack that runs through the kernel's
/// exception entry (`KeBugCheckEx` <- `MmAccessFault` <- ... <-
/// `Ki*ExceptionHandler` <- the faulting code) counts from below it.
pub fn signature(finding: &Finding) -> String {
    let stack: Vec<&str> = finding.stack.iter().map(String::as_str).filter(|f| !f.starts_with('(')).collect();
    let entry = stack.iter().rposition(|f| {
        let name = f.split_once('+').map_or(*f, |(function, _)| function);
        name.ends_with("ExceptionHandler") || name.ends_with("SynchronousException") || name.ends_with("DispatchException")
    });
    let frames: Vec<&str> = stack[entry.map_or(0, |i| i + 1)..]
        .iter()
        .filter(|f| !f.starts_with("nt!KeBugCheck"))
        .map(|f| if f.starts_with("<wild jump") { "<wild jump>" } else { f.split_once('+').map_or(*f, |(function, _)| function) })
        .take(3)
        .collect();
    if frames.is_empty() {
        format!("{:?}:{:?}", finding.class, finding.pc)
    } else {
        format!("{:?}:{}", finding.class, frames.join("<"))
    }
}

/// Classifies hypervisor and guest diagnostics without guessing from a mere
/// non-zero exit. Each detector can be enabled in a harness manifest.
pub fn detect(text: &str, pc: Option<u64>, input: &str, timeout: bool) -> Option<Finding> {
    let lower = text.to_ascii_lowercase();
    let class = if timeout {
        Some(BugClass::Timeout)
    } else if lower.contains("pool") && (lower.contains("corrupt") || lower.contains("overrun")) {
        Some(BugClass::PoolCorruption)
    } else if lower.contains("double free") || lower.contains("double-free") {
        Some(BugClass::DoubleFree)
    } else if lower.contains("use after free") || lower.contains("use-after-free") {
        Some(BugClass::UseAfterFree)
    } else if lower.contains("out of bounds") || lower.contains("out-of-bounds") {
        Some(BugClass::OutOfBounds)
    } else if lower.contains("integer overflow") || lower.contains("arithmetic overflow") {
        Some(BugClass::IntegerOverflow)
    } else if lower.contains("uninitialized") || lower.contains("uninitialised") {
        Some(BugClass::Uninitialized)
    } else if lower.contains("access violation")
        || lower.contains("segmentation fault")
        || lower.contains("translation fault")
    {
        Some(BugClass::AccessViolation)
    } else if lower.contains("bugcheck")
        || lower.contains("bug check")
        || lower.contains("kernel panic")
    {
        Some(BugClass::BugCheck)
    } else if lower.contains("crash") || lower.contains("fatal") {
        Some(BugClass::Unknown)
    } else {
        None
    };
    class.map(|class| Finding {
        class,
        detail: text.to_string(),
        pc,
        input: input.to_string(),
        stack: Vec::new(),
    })
}

#[derive(Debug, Clone)]
pub struct Mutator {
    state: u64,
    max_len: usize,
    /// Values the target compared inputs with (comparison logging).
    dictionary: Vec<u32>,
}

/// The calls of a sequence input: (code, payload). A last call cut short
/// keeps what's there; bytes too few for a header are dropped.
pub fn calls(input: &[u8]) -> Vec<(u32, Vec<u8>)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 6 <= input.len() {
        let code = u32::from_le_bytes(input[at..at + 4].try_into().expect("four bytes"));
        let length = u16::from_le_bytes(input[at + 4..at + 6].try_into().expect("two bytes")) as usize;
        let end = (at + 6 + length).min(input.len());
        out.push((code, input[at + 6..end].to_vec()));
        at = end;
    }
    out
}

fn calls_or(input: &[u8], fallback: &[(u32, Vec<u8>)]) -> Vec<(u32, Vec<u8>)> {
    let c = calls(input);
    if c.is_empty() { fallback.to_vec() } else { c }
}

/// A sequence input from calls.
pub fn sequence(calls: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (code, payload) in calls {
        let payload = &payload[..payload.len().min(u16::MAX as usize)];
        out.extend_from_slice(&code.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        out.extend_from_slice(payload);
    }
    out
}

/// Dictionary words kept at most.
const DICTIONARY: usize = 4096;

impl Mutator {
    pub fn new(seed: u64, max_len: usize) -> Self {
        Self {
            state: seed | 1,
            max_len,
            dictionary: Vec::new(),
        }
    }

    /// Remember a value the target compares with (not tiny ones: those
    /// the byte operations reach anyway).
    pub fn learn(&mut self, value: u32) {
        if value > 0xff && self.dictionary.len() < DICTIONARY && !self.dictionary.contains(&value) {
            self.dictionary.push(value);
        }
    }

    pub fn dictionary(&self) -> &[u32] {
        &self.dictionary
    }

    /// A dictionary word (the dictionary must not be empty).
    fn word(&mut self) -> u32 {
        let i = self.next() as usize % self.dictionary.len();
        self.dictionary[i]
    }

    /// A mutation of `seed` whose first four bytes are a Windows IOCTL code
    /// (`device << 16 | access << 14 | function << 2 | method`): one time in
    /// four the code changes, to a compared value or a neighbouring
    /// function or another method; the payload after it is mutated as by
    /// `mutate_preserving_prefix`.
    pub fn mutate_ioctl(&mut self, seed: &[u8]) -> Vec<u8> {
        let mut out = self.mutate_preserving_prefix(seed, 4);
        if out.len() >= 4 && self.next() % 4 == 0 {
            let code = u32::from_le_bytes(out[..4].try_into().expect("four bytes"));
            let code = match self.next() % 4 {
                0 | 1 if !self.dictionary.is_empty() => self.word(),
                2 => {
                    let step = 1 + (self.next() % 4) as u32;
                    let function = (code >> 2) & 0xfff;
                    let function = if self.next() & 1 == 0 { function.wrapping_add(step) } else { function.wrapping_sub(step) } & 0xfff;
                    (code & !(0xfff << 2)) | (function << 2)
                }
                _ => (code & !3) | (self.next() % 4) as u32,
            };
            out[..4].copy_from_slice(&code.to_le_bytes());
        }
        out
    }
    /// A mutation of the call sequence `seed` (see `Harness::sequence`):
    /// a call's payload or code changes, or calls are inserted (from the
    /// seed or `donor`), dropped, repeated, swapped, or the tail replaced
    /// by `donor`'s. Whole calls past `max_len` are dropped.
    pub fn mutate_sequence(&mut self, seed: &[u8], donor: &[u8]) -> Vec<u8> {
        let mut calls = calls(seed);
        if calls.is_empty() {
            calls.push((0, Vec::new()));
        }
        let donors = calls_or(donor, &calls);
        for _ in 0..1 + self.next() % 3 {
            let i = self.choose(calls.len());
            match self.next() % 9 {
                0..=2 => calls[i].1 = self.mutate(&calls[i].1),
                3 => {
                    let code = calls[i].0;
                    calls[i].0 = match self.next() % 4 {
                        0 if !self.dictionary.is_empty() => self.word(),
                        1 => {
                            let other = &donors[self.choose(donors.len())];
                            other.0
                        }
                        2 => code ^ (1 << (self.next() % 32)),
                        _ => code.wrapping_add(1 + (self.next() % 4) as u32).wrapping_sub(if self.next() & 1 == 0 { 0 } else { 2 + 2 * (self.next() % 4) as u32 }),
                    };
                }
                4 => {
                    let call = donors[self.choose(donors.len())].clone();
                    let at = self.choose(calls.len() + 1);
                    calls.insert(at, call);
                }
                5 if calls.len() > 1 => {
                    calls.remove(i);
                }
                6 => {
                    let call = calls[i].clone();
                    calls.insert(i, call);
                }
                7 => {
                    let j = self.choose(calls.len());
                    calls.swap(i, j);
                }
                _ => {
                    let j = self.choose(donors.len());
                    calls.truncate(i);
                    calls.extend(donors[j..].iter().cloned());
                }
            }
        }
        let mut out = sequence(&calls);
        while out.len() > self.max_len && calls.len() > 1 {
            calls.pop();
            out = sequence(&calls);
        }
        out.truncate(self.max_len);
        out
    }

    fn next(&mut self) -> u64 {
        self.state ^= self.state << 7;
        self.state ^= self.state >> 9;
        self.state ^= self.state << 8;
        self.state
    }
    pub fn mutate(&mut self, seed: &[u8]) -> Vec<u8> {
        self.mutate_preserving_prefix(seed, 0)
    }
    pub fn mutate_preserving_prefix(&mut self, seed: &[u8], prefix_len: usize) -> Vec<u8> {
        let mut out = seed.to_vec();
        if out.len() < prefix_len {
            out.resize(prefix_len, 0);
        } else if out.is_empty() {
            out.push(0);
        }
        let operations = 1 + (self.next() % 8) as usize;
        for _ in 0..operations {
            let editable = out.len().saturating_sub(prefix_len);
            match self.next() % 9 {
                0 if editable != 0 => {
                    let i = prefix_len + (self.next() as usize) % editable;
                    out[i] ^= 1 << (self.next() % 8);
                }
                1 if editable != 0 => {
                    let i = prefix_len + (self.next() as usize) % editable;
                    out[i] = self.next() as u8;
                }
                2 if out.len() < self.max_len => {
                    let i = prefix_len + (self.next() as usize) % (editable + 1);
                    out.insert(i, self.next() as u8);
                }
                3 if editable > 1 => {
                    let i = prefix_len + (self.next() as usize) % editable;
                    out.remove(i);
                }
                4 if editable != 0 => {
                    let i = prefix_len + (self.next() as usize) % editable;
                    out[i] = out[i].wrapping_add((self.next() & 0xff) as u8);
                }
                5 if editable != 0 => {
                    const INTERESTING: [u8; 14] =
                        [0, 1, 2, 3, 4, 7, 8, 16, 32, 64, 0x7f, 0x80, 0xfe, 0xff];
                    let i = prefix_len + (self.next() as usize) % editable;
                    out[i] = INTERESTING[(self.next() as usize) % INTERESTING.len()];
                }
                6 if editable >= 4 => {
                    const INTERESTING: [u32; 12] =
                        [0, 1, 2, 4, 8, 16, 32, 64, 127, 128, 0x7fff_ffff, u32::MAX];
                    let i = prefix_len + (self.next() as usize) % (editable - 3);
                    out[i..i + 4].copy_from_slice(
                        &INTERESTING[(self.next() as usize) % INTERESTING.len()].to_le_bytes(),
                    );
                }
                7 if editable != 0 => {
                    let i = prefix_len + (self.next() as usize) % editable;
                    let n = (1 + self.next() as usize % 8).min(out.len() - i);
                    out[i..i + n].fill(if self.next() & 1 == 0 { 0 } else { 0xff });
                }
                8 if editable >= 4 && !self.dictionary.is_empty() => {
                    let i = prefix_len + (self.next() as usize) % (editable - 3);
                    let word = self.word();
                    out[i..i + 4].copy_from_slice(&word.to_le_bytes());
                }
                _ => {}
            }
        }
        out.truncate(self.max_len);
        if out.len() < prefix_len {
            out.resize(prefix_len.min(self.max_len), 0);
        }
        out
    }

    pub fn choose(&mut self, count: usize) -> usize {
        if count == 0 {
            0
        } else {
            (self.next() as usize) % count
        }
    }

    pub fn choose_weighted(&mut self, weights: &[u32]) -> usize {
        let total: u64 = weights.iter().map(|weight| u64::from(*weight)).sum();
        if total == 0 {
            return self.choose(weights.len());
        }
        let mut pick = self.next() % total;
        for (index, weight) in weights.iter().enumerate() {
            let weight = u64::from(*weight);
            if pick < weight {
                return index;
            }
            pick -= weight;
        }
        weights.len().saturating_sub(1)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorpusEntry {
    pub hash: String,
    pub length: usize,
    pub new_blocks: usize,
    pub path: PathBuf,
}

#[derive(Debug)]
pub struct CorpusStore {
    root: PathBuf,
}

impl CorpusStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("inputs"))?;
        fs::create_dir_all(root.join("crashes"))?;
        Ok(Self { root })
    }
    pub fn hash(data: &[u8]) -> String {
        hex::encode(Sha256::digest(data))
    }
    pub fn save_input(&self, data: &[u8], new_blocks: usize) -> Result<CorpusEntry> {
        let hash = Self::hash(data);
        let path = self.root.join("inputs").join(&hash);
        if !path.exists() {
            fs::write(&path, data)?;
        }
        Ok(CorpusEntry {
            hash,
            length: data.len(),
            new_blocks,
            path,
        })
    }
    pub fn load_inputs(&self) -> Result<Vec<Vec<u8>>> {
        let mut paths = Vec::new();
        for entry in fs::read_dir(self.root.join("inputs"))? {
            let path = entry?.path();
            if path.is_file() {
                paths.push(path);
            }
        }
        paths.sort();
        paths
            .into_iter()
            .map(|path| fs::read(path).map_err(Error::from))
            .collect()
    }
    pub fn save_finding(&self, finding: &Finding, data: &[u8]) -> Result<PathBuf> {
        let hash = Self::hash(data);
        let path = self
            .root
            .join("crashes")
            .join(format!("{}-{:?}.bin", hash, finding.class));
        if !path.exists() {
            fs::write(&path, data)?;
        }
        fs::write(
            path.with_extension("json"),
            serde_json::to_vec_pretty(finding)?,
        )?;
        Ok(path)
    }
}

/// Optional VMM-backed runner. It is deliberately kept in this leaf crate so
/// the normal `vroom` binaries never depend on fuzzing policy or corpus I/O.
#[derive(Debug)]
pub struct Engine {
    pub vm: vroom_vmm::Vmm,
    pub harness: Harness,
    pub coverage: CoverageMap,
    block_by_pc: HashMap<u64, usize>,
    /// One function-entry breakpoint is rotated across runs to catch handlers
    /// reached through unresolved indirect dispatch branches.
    watch_targets: Vec<u64>,
    watch_cursor: usize,
    snapshot_fresh: bool,
    cow_enabled: bool,
    input_buffer: Vec<u8>,
    /// `blocks` mode: block index -> the instruction its BRK replaced.
    planted: HashMap<usize, [u8; 4]>,
    /// `blocks` mode: blocks whose code page isn't mapped at the snapshot
    /// (pageable code in transition), by 4 KiB VA page. Each is planted once
    /// an input has faulted its page in: resets bring back the same
    /// physical page every time.
    pending: BTreeMap<u64, Vec<usize>>,
    /// Names for findings' stacks: made at the first one (a PDB load).
    symbols: Option<vroom_kernel::symbolize::Symbolizer>,
    /// The target's image (`entry_pc`'s), found by the first compare log.
    target_image: Option<(u64, u64)>,
    /// `nt!KiDispatchException`, when the harness detects exceptions: every
    /// exception passes there first, the ones an `__except` swallows too.
    exception_hook: Option<u64>,
    /// Blocks the last comparison log reached first (blocks mode): the
    /// input it logged covers them.
    pub cmplog_blocks: Vec<u64>,
}

/// One input, in place and ready to run (`Engine::load_case`).
struct Case {
    /// The first input after the snapshot: arm the in-place reset now.
    arm_after_patch: bool,
    reset_us: u64,
    /// The harness's watchpoints, at this input's addresses.
    watchpoints: Vec<MemoryWatch>,
}

/// The 4-byte fields comparison logging shifts by the operands'
/// difference: the IOCTL code (the prefix) and the payload's first
/// aligned words, where structure headers, sizes and magic values sit;
/// in a sequence, each call's code and its payload's first two words.
pub fn cmp_fields(harness: &Harness, input: &[u8]) -> Vec<usize> {
    let len = input.len();
    if harness.sequence {
        let mut fields = Vec::new();
        let mut at = 0;
        for (_, payload) in calls(input) {
            fields.push(at);
            fields.extend([0, 4].into_iter().filter(|w| w + 4 <= payload.len()).map(|w| at + 6 + w));
            at += 6 + payload.len();
        }
        return fields;
    }
    let prefix = if harness.ioctl_prefix { 4 } else { 0 };
    let mut fields: Vec<usize> = if harness.ioctl_prefix { vec![0] } else { Vec::new() };
    fields.extend((prefix..len.min(prefix + 32)).step_by(4).filter(|f| f + 4 <= len));
    fields
}

/// The PDB cache the lab fills for the harness's VM (`<state>/symbols`).
pub fn symbol_cache(harness: &Harness) -> PathBuf {
    vroom_config::VmConfig::load(&harness.config)
        .map(|c| c.default_state_dir(&harness.config))
        .unwrap_or_else(|_| harness.state_dir.clone())
        .join("symbols")
}

/// Function start addresses from block lists' `function` names: a function
/// starts at its lowest block.
pub fn function_names(blocks: &[CoverageBlock]) -> Vec<(u64, String)> {
    let mut starts = BTreeMap::<String, u64>::new();
    for b in blocks {
        if let (Some(va), false) = (b.va, b.function.is_empty()) {
            let at = starts.entry(b.function.clone()).or_insert(va);
            *at = (*at).min(va);
        }
    }
    starts.into_iter().map(|(name, at)| (at, name)).collect()
}

/// The same exception (code, PC and faulting address) this often in one
/// input is an exception storm.
const MAX_EXCEPTIONS: usize = 8;
/// The same for an exception outside the target (the system's own).
const MAX_SYSTEM_EXCEPTIONS: usize = 256;

/// What every debug configuration of an input keeps.
#[derive(Debug, Clone, Copy)]
struct Always {
    bugcheck: Option<u64>,
    brk: Option<u16>,
}

/// A fuzzing VM is disposable: no host port forwards (they would clash
/// between workers, and with the lab), its own copy of the EFI variables,
/// since restoring a snapshot writes them back, and no flushes to the
/// host's disk (every reset drops what the guest wrote).
fn disposable(config: &mut vroom_config::VmConfig, state_dir: &Path) -> Result<()> {
    for net in &mut config.nets {
        net.port_forwards.clear();
    }
    for disk in &mut config.disks {
        disk.sync = vroom_config::DiskSync::None;
    }
    if let vroom_config::Boot::Uefi { vars, .. } = &mut config.boot {
        let copy = state_dir.join("fuzz-efivars.fd");
        if !copy.exists() {
            fs::create_dir_all(state_dir)?;
            if vars.exists() {
                fs::copy(&*vars, &copy)?;
            }
        }
        *vars = copy;
    }
    Ok(())
}

impl Engine {
    pub fn open(harness: Harness) -> Result<Self> {
        let input_capacity = harness.input_capacity;
        let mut config = vroom_config::VmConfig::load(&harness.config)
            .map_err(|e| Error::Config(e.to_string()))?;
        disposable(&mut config, &harness.state_dir)?;
        if harness.cpu >= config.cpus as usize {
            return Err(Error::Invalid(format!(
                "harness CPU {} is outside VM CPU count {}",
                harness.cpu, config.cpus
            )));
        }
        let hv = vroom_hv::default_hypervisor()?;
        // COW resets restore the VM in place (`Vmm::research_reset`).
        // `reopen_after: 1` opts out and restores the snapshot each time.
        let cow_enabled = harness.reopen_after != 1;
        let mut vm = vroom_vmm::Vmm::start(
            config,
            hv,
            vroom_vmm::VmmOptions {
                serial_out: Box::new(std::io::sink()),
                start: vroom_vmm::StartMode::RestorePaused(harness.snapshot.clone()),
                clipboard: None,
                ui: None,
                state_dir: harness.state_dir.clone(),
                // The harness doesn't sandbox the VMM it drives.
                sandbox: false,
            },
        )?;
        // Each reopen restores the snapshot onto a new disk overlay.
        vm.research_discard_disk_layers();
        Ok(Self {
            vm,
            harness,
            coverage: CoverageMap::default(),
            block_by_pc: HashMap::new(),
            watch_targets: Vec::new(),
            watch_cursor: 0,
            snapshot_fresh: true,
            cow_enabled,
            input_buffer: vec![0; input_capacity],
            planted: HashMap::new(),
            pending: BTreeMap::new(),
            symbols: None,
            target_image: None,
            exception_hook: None,
            cmplog_blocks: Vec::new(),
        })
    }

    /// Resolve declared blocks and prepare the coverage map for hardware
    /// breakpoints on each reached block's successors.
    pub fn instrument(&mut self) -> Result<()> {
        let state = self.vm.debug_registers(self.harness.cpu)?;
        let registers = Registers::from_state(&state)
            .map_err(|error| Error::Invalid(format!("ARM64 register state: {error}")))?;
        if self.harness.detectors.iter().any(|d| d == "exceptions" || d == "access-violation") && self.exception_hook.is_none() {
            self.load_symbols(&registers)?;
            let hook = self
                .symbols
                .as_ref()
                .and_then(|s| s.resolve("KiDispatchException"))
                .ok_or_else(|| Error::Invalid("exceptions: nt!KiDispatchException not in the kernel PDB".into()))?;
            if !self.harness.exception_sites.contains(&hook) {
                self.harness.exception_sites.push(hook);
            }
            self.exception_hook = Some(hook);
            // Whose an exception is, from the first one on.
            self.target_range(&registers)?;
        }
        let address_space = AddressSpace {
            memory: &self.vm,
            registers: &registers,
        };
        for index in 0..self.harness.coverage.len() {
            let block = self.harness.coverage[index].clone();
            let gpa = match (block.gpa, block.va) {
                (Some(gpa), _) => Some(gpa),
                (None, Some(va)) => match address_space.translate(va) {
                    Ok(translated) => Some(translated.physical),
                    // Driver code can be pageable. Keep its VA breakpoint in
                    // the manifest even when that code page is not resident
                    // at the snapshot; it may be loaded before execution.
                    Err(_) => None,
                },
                (None, None) => {
                    return Err(Error::Invalid(format!(
                        "coverage block {} needs either gpa or va",
                        block.id
                    )))
                }
            };
            if let Some(gpa) = gpa {
                self.block_by_pc.insert(gpa, index);
            }
            if let Some(va) = block.va {
                self.block_by_pc.insert(va, index);
            }
            self.harness.coverage[index].gpa = gpa;
        }
        let entry_pc = self.harness.entry_pc.unwrap_or(registers.pc);
        let entry_function = self
            .block_by_pc
            .get(&entry_pc)
            .and_then(|index| self.harness.coverage.get(*index))
            .map(|block| block.function.clone());
        let mut function_entries = BTreeMap::<String, u64>::new();
        for block in &self.harness.coverage {
            if block.function.is_empty() {
                continue;
            }
            if let Some(va) = block.va {
                function_entries
                    .entry(block.function.clone())
                    .and_modify(|entry| *entry = (*entry).min(va))
                    .or_insert(va);
            }
        }
        self.watch_targets = function_entries
            .into_iter()
            .filter_map(|(function, entry)| {
                (Some(function.as_str()) != entry_function.as_deref()
                    && entry != self.harness.stop_pc.unwrap_or(0))
                .then_some(entry)
            })
            .collect();
        if std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
            eprintln!(
                "function-entry watches: {}",
                self.watch_targets
                    .iter()
                    .map(|va| format!("{va:#x}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if self.harness.coverage_mode == CoverageMode::Blocks {
            self.plant(entry_pc)?;
        }
        if let Some(bugcheck) = self.harness.bugcheck_pc {
            if self.load_symbols(&registers).is_ok() {
                let symbols = self.symbols.as_ref().expect("loaded");
                if let (Some(ex), Some(two)) = (symbols.resolve("KeBugCheckEx"), symbols.resolve("KeBugCheck2")) {
                    if ex == bugcheck {
                        self.harness.bugcheck_pc = Some(two);
                    }
                }
            }
        }
        if let Some(bugcheck) = self.harness.bugcheck_pc {
            // The bugcheck breakpoint sits beyond the four coverage slots.
            // Check the backend has it before the first input needs it.
            let fits = self.vm.debug_configure(
                self.harness.cpu,
                vroom_hv::debug::DebugConfig {
                    enabled: true,
                    breakpoints: vec![bugcheck; 5],
                    ..Default::default()
                },
            );
            self.vm
                .debug_configure(self.harness.cpu, vroom_hv::debug::DebugConfig::default())?;
            fits.map_err(|error| {
                Error::Invalid(format!(
                    "bugcheck_pc needs a fifth hardware breakpoint: {error}"
                ))
            })?;
        }
        if self.cow_enabled {
            self.vm.research_arm()?;
        }
        Ok(())
    }

    /// Replace a VM that timed out or reset with a fresh restore of the same
    /// snapshot. COW reset is fast for normal cases, but cannot be trusted
    /// after a guest bugcheck or a wedged kernel path.
    pub fn reopen(self) -> Result<Self> {
        let Engine {
            vm,
            harness,
            coverage,
            watch_cursor,
            symbols,
            target_image,
            ..
        } = self;
        drop(vm);
        let mut fresh = Self::open(harness)?;
        fresh.coverage = coverage;
        // The same snapshot: the same modules, PDBs already loaded.
        fresh.symbols = symbols;
        fresh.target_image = target_image;
        fresh.instrument()?;
        fresh.watch_cursor = watch_cursor;
        Ok(fresh)
    }

    /// `blocks` mode: a BRK on every resolved block not covered yet, except
    /// the entry block (the snapshot stands on it) and the hardware
    /// breakpoint sites. Covered blocks stay as they are after a reopen.
    fn plant(&mut self, entry_pc: u64) -> Result<()> {
        let brk = brk_instruction(COVERAGE_BRK);
        let hardware: Vec<u64> = self
            .harness
            .stop_pc
            .into_iter()
            .chain(self.harness.bugcheck_pc)
            .chain(self.harness.exception_sites.iter().copied())
            .collect();
        self.pending.clear();
        for index in 0..self.harness.coverage.len() {
            let block = &self.harness.coverage[index];
            let covered = self.coverage.blocks.get(&block.id).is_some_and(|row| row.hits > 0);
            let entry = block.va == Some(entry_pc) || block.gpa == Some(entry_pc);
            if covered || entry || block.va.is_some_and(|va| hardware.contains(&va)) {
                continue;
            }
            match (block.gpa, block.va) {
                (Some(gpa), _) => self.plant_at(index, gpa, brk)?,
                (None, Some(va)) => self.pending.entry(va & !0xfff).or_default().push(index),
                (None, None) => {}
            }
        }
        if std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
            eprintln!(
                "planted {} coverage breakpoints; {} blocks on {} pages not mapped yet",
                self.planted.len(),
                self.pending.values().map(Vec::len).sum::<usize>(),
                self.pending.len()
            );
        }
        Ok(())
    }

    fn plant_at(&mut self, index: usize, gpa: u64, brk: [u8; 4]) -> Result<()> {
        let mut original = [0u8; 4];
        self.vm.debug_read_physical(gpa, &mut original)?;
        if original == brk {
            // Two manifest rows for one address.
            return Ok(());
        }
        self.vm.research_patch(gpa, &brk)?;
        self.planted.insert(index, original);
        Ok(())
    }

    /// `blocks` mode, after an input: plant the blocks on pages that input
    /// mapped. VM paused.
    fn plant_pending(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let state = self.vm.debug_registers(self.harness.cpu)?;
        let registers = Registers::from_state(&state)
            .map_err(|error| Error::Invalid(format!("ARM64 register state: {error}")))?;
        let mut mapped = Vec::new();
        {
            let address_space = AddressSpace {
                memory: &self.vm,
                registers: &registers,
            };
            for &page in self.pending.keys() {
                if let Ok(translated) = address_space.translate(page) {
                    mapped.push((page, translated.physical));
                }
            }
        }
        let brk = brk_instruction(COVERAGE_BRK);
        for (page, physical) in mapped {
            // A page the snapshot already held (in transition) comes back
            // at this frame every input. One the input filled in has other
            // contents in the baseline: a BRK there would corrupt them.
            if !self.vm.research_matches_baseline(physical, 0x1000)? {
                continue;
            }
            for index in self.pending.remove(&page).unwrap_or_default() {
                let va = self.harness.coverage[index].va.expect("pending blocks have a VA");
                let gpa = physical + (va - page);
                self.harness.coverage[index].gpa = Some(gpa);
                self.block_by_pc.insert(gpa, index);
                let covered = self
                    .coverage
                    .blocks
                    .get(&self.harness.coverage[index].id)
                    .is_some_and(|row| row.hits > 0);
                if !covered {
                    self.plant_at(index, gpa, brk)?;
                }
            }
            if std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
                eprintln!("page {page:#x} mapped at {physical:#x}: planted its blocks");
            }
        }
        Ok(())
    }

    /// `blocks` mode: blocks still waiting for their page to be mapped.
    pub fn unmapped_blocks(&self) -> usize {
        self.pending.values().map(Vec::len).sum()
    }

    /// `blocks` mode: put back block `index`'s instruction for good.
    fn unplant(&mut self, index: usize) -> Result<bool> {
        let Some(original) = self.planted.remove(&index) else {
            return Ok(false);
        };
        let gpa = self.harness.coverage[index].gpa.expect("planted blocks have a GPA");
        self.vm.research_patch(gpa, &original)?;
        Ok(true)
    }

    /// Coverage another instance found: those blocks need no breakpoint
    /// here any more. The VM must be paused (between inputs).
    pub fn take_coverage(&mut self, block_ids: &[u64]) -> Result<()> {
        if self.planted.is_empty() {
            return Ok(());
        }
        let indices: Vec<usize> = self
            .harness
            .coverage
            .iter()
            .enumerate()
            .filter(|(_, block)| block_ids.contains(&block.id))
            .map(|(index, _)| index)
            .collect();
        for index in indices {
            self.unplant(index)?;
        }
        Ok(())
    }

    fn always(&self) -> Always {
        Always {
            bugcheck: self.harness.bugcheck_pc,
            brk: (self.harness.coverage_mode == CoverageMode::Blocks).then_some(COVERAGE_BRK),
        }
    }

    pub fn watch_cursor(&self) -> usize {
        self.watch_cursor
    }

    pub fn set_watch_cursor(&mut self, cursor: usize) {
        self.watch_cursor = cursor;
    }

    fn next_watch_target(&mut self) -> Option<u64> {
        if self.watch_targets.is_empty() {
            return None;
        }
        let target = self.watch_targets[self.watch_cursor % self.watch_targets.len()];
        self.watch_cursor = self.watch_cursor.wrapping_add(1);
        Some(target)
    }

    /// The symbolized call stack of a stopped thread (the VM paused). A
    /// stack that can't be had is one line saying why: a finding stays a
    /// finding.
    fn stack_of(&mut self, registers: &Registers) -> Vec<String> {
        if let Err(e) = self.load_symbols(registers) {
            return vec![format!("(no symbols: {e})")];
        }
        let space = AddressSpace { memory: &self.vm, registers };
        self.symbols.as_mut().map(|s| s.stack(&space, registers)).unwrap_or_default()
    }

    /// The symbolizer, made once (a PDB load; the VM paused).
    fn load_symbols(&mut self, registers: &Registers) -> Result<()> {
        if self.symbols.is_none() {
            let space = AddressSpace { memory: &self.vm, registers };
            let mut s = vroom_kernel::symbolize::Symbolizer::new(&space, registers, &symbol_cache(&self.harness))
                .map_err(|e| Error::Invalid(format!("symbols: {e}")))?;
            s.add_names(function_names(&self.harness.coverage));
            self.symbols = Some(s);
        }
        Ok(())
    }

    /// The target's code: the image `entry_pc` is in, or the module the
    /// harness names (`target`) when the entry is elsewhere (a user-mode
    /// client calling into a driver).
    fn target_range(&mut self, registers: &Registers) -> Result<(u64, u64)> {
        if let Some(range) = self.target_image {
            return Ok(range);
        }
        let range = match self.harness.target.clone() {
            Some(name) => {
                self.load_symbols(registers)?;
                self.symbols
                    .as_ref()
                    .and_then(|s| s.module_range(&name))
                    .ok_or_else(|| Error::Invalid(format!("target module {name} isn't loaded")))?
            }
            None => {
                let entry = self.harness.entry_pc.unwrap_or(registers.pc);
                let space = AddressSpace { memory: &self.vm, registers };
                let image = vroom_kernel::windows::image_containing(&space, entry)
                    .map_err(|e| Error::Invalid(format!("target image: {e}")))?;
                (image.base, image.base + u64::from(image.size))
            }
        };
        self.target_image = Some(range);
        Ok(range)
    }

    /// At `nt!KiDispatchException` (x0 the exception record, x2 the trap
    /// frame, x3 the previous mode, x4 first chance): a finding if this is
    /// a first-chance kernel-mode fault a bug makes (an access to a kernel
    /// address, a stack overrun, an illegal or misaligned instruction) in
    /// the target's code or below a call from it. Accesses to user
    /// addresses are probes of user buffers, which fault by design.
    /// The stack pointer of the code an exception came from (the trap
    /// frame KiDispatchException's x2 points to).
    fn trap_sp(&mut self, registers: &Registers) -> Option<u64> {
        self.load_symbols(registers).ok()?;
        let space = AddressSpace { memory: &self.vm, registers };
        let symbols = self.symbols.as_ref()?;
        symbols.trap_registers(&space, registers.x[2], registers).ok().map(|r| r.sp)
    }

    fn exception_finding(&mut self, registers: &Registers, input: &str) -> Result<Option<Finding>> {
        if registers.x[3] & 0xff != 0 || registers.x[4] & 0xff == 0 {
            return Ok(None);
        }
        let record = registers.x[0];
        let (code, address, kind, fault) = {
            let space = AddressSpace { memory: &self.vm, registers };
            let mut word = [0u8; 4];
            if vroom_kernel::windows::VirtualMemory::read(&space, record, &mut word).is_err() {
                return Ok(None);
            }
            let code = u32::from_le_bytes(word);
            let u64_at = |at: u64| vroom_kernel::windows::VirtualMemory::u64(&space, record + at).ok();
            (code, u64_at(0x10).unwrap_or(0), u64_at(0x20).unwrap_or(0), u64_at(0x28).unwrap_or(0))
        };
        if std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
            eprintln!("exception {code:#x} at {address:#x} info {kind:#x} {fault:#x} mode {:#x} first {:#x}", registers.x[3], registers.x[4]);
        }
        // The trap handlers pass the kernel's internal codes
        // (KI_EXCEPTION_*), translated after this.
        let code = match code {
            0x1000_0004 => 0xc000_0005,
            0x1000_0002 => 0xc000_001d,
            0x1000_0003 => 0xc000_0094,
            0x1000_0001 => 0xc000_0096,
            c => c,
        };
        let internal = code & 0xffff_ff00 == 0x1000_0000;
        let class = match code {
            // A kernel address, or the NULL page (a NULL dereference, user
            // half or not).
            0xc000_0005 if fault >> 63 == 1 || fault < 0x1_0000 => BugClass::AccessViolation,
            // Stack overrun (fast fail), illegal and privileged instruction,
            // data and instruction misalignment, stack overflow, divide.
            0xc000_0409 | 0xc000_001d | 0xc000_0096 | 0x8000_0002 | 0xc000_00aa | 0xc000_00fd | 0xc000_0094 => {
                BugClass::ExceptionDispatch
            }
            // Other faults the trap handlers raise (an address size fault
            // on a non-canonical pointer: 0x10000011).
            _ if internal => BugClass::ExceptionDispatch,
            _ => return Ok(None),
        };
        self.load_symbols(registers)?;
        let (lo, hi) = self.target_range(registers)?;
        let space = AddressSpace { memory: &self.vm, registers };
        let symbols = self.symbols.as_mut().expect("loaded above");
        let faulting = symbols.trap_registers(&space, registers.x[2], registers).ok();
        // Whose it is from the PCs alone: naming loads PDBs (seconds for
        // tcpip's), and most exceptions are the system's own.
        let (pcs, stop) = match &faulting {
            Some(r) => symbols.pcs(&AddressSpace { memory: &self.vm, registers: r }, r),
            None => (vec![address], None),
        };
        // The target's, or a jump out of all code (a smashed return
        // address, a corrupt function pointer).
        let wild = !symbols.in_module(address);
        let theirs = !wild && !(lo..hi).contains(&address) && !pcs.iter().any(|pc| (lo..hi).contains(pc));
        if theirs {
            if std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
                eprintln!("exception {code:#x} at {address:#x} (not the target's)");
            }
            return Ok(None);
        }
        let frames: Vec<(u64, String)> = {
            let r = faulting.as_ref().unwrap_or(registers);
            let space = AddressSpace { memory: &self.vm, registers: r };
            pcs.iter().map(|&pc| (pc, symbols.name(&space, pc))).collect()
        };
        let what = match (code, kind) {
            (0xc000_0005, 0) => format!("reading {fault:#x}"),
            (0xc000_0005, 1) => format!("writing {fault:#x}"),
            (0xc000_0005, _) => format!("executing {fault:#x}"),
            (_, 0) if internal => format!("reading {fault:#x}"),
            (_, 1) if internal => format!("writing {fault:#x}"),
            _ => String::new(),
        };
        // Nothing to unwind from a wild PC, and where it went depends on
        // the input: one frame that says so.
        let mut stack: Vec<String> = if wild {
            vec![format!("<wild jump to {address:#x}>")]
        } else {
            frames.into_iter().map(|(_, n)| n).collect()
        };
        stack.extend(stop.filter(|_| !wild).map(|s| format!("({s})")));
        let at = stack.first().cloned().unwrap_or_else(|| format!("{address:#x}"));
        Ok(Some(Finding {
            class,
            detail: format!("exception {code:#x} {what} at {at} (first chance: an __except may swallow it)").replace("  ", " "),
            pc: Some(address),
            input: input.to_string(),
            stack,
        }))
    }

    /// The stack a bugcheck is about. KERNEL_SECURITY_CHECK_FAILURE (0x139,
    /// a fast fail) passes the trap frame of the failing code (third
    /// parameter): unwind from there. Else from `KeBugCheckEx`.
    fn bugcheck_stack(&mut self, registers: &Registers) -> Vec<String> {
        if registers.x[0] == 0x139 && self.load_symbols(registers).is_ok() {
            let space = AddressSpace { memory: &self.vm, registers };
            let symbols = self.symbols.as_mut().expect("loaded");
            if let Ok(r) = symbols.trap_registers(&space, registers.x[2], registers) {
                let space = AddressSpace { memory: &self.vm, registers: &r };
                return symbols.stack(&space, &r);
            }
        }
        self.stack_of(registers)
    }

    /// Put `input` in place for one execution, as every run does: reset
    /// to the snapshot (not before its first), the payload at
    /// `input_gpa`, then the IOCTL code, pointers, lengths and poisoned
    /// ranges the harness declares.
    fn load_case(&mut self, input: &[u8]) -> Result<Case> {
        if input.len() > self.harness.max_input {
            return Err(Error::Invalid("input exceeds max_input".into()));
        }
        let arm_after_patch = !self.cow_enabled && self.snapshot_fresh;
        let reset_start = Instant::now();
        if self.snapshot_fresh {
            // RestorePaused already supplied the exact snapshot state. Do
            // not COW-reset before its first testcase: HVF can retain stage-2
            // host mappings across that remap and execute against stale RAM.
            self.snapshot_fresh = false;
        } else if self.cow_enabled {
            self.vm.research_reset()?;
        } else {
            return Err(Error::Invalid(
                "reopen_after must be 1 when COW resets are disabled".into(),
            ));
        }
        let reset_us = reset_start.elapsed().as_micros() as u64;
        let (ioctl, payload) = if self.harness.ioctl_prefix {
            if input.len() < 4 {
                return Err(Error::Invalid(
                    "IOCTL-prefixed input is shorter than four bytes".into(),
                ));
            }
            (
                Some(u32::from_le_bytes(
                    input[..4].try_into().expect("four bytes"),
                )),
                &input[4..],
            )
        } else {
            (None, input)
        };
        if payload.len() > self.harness.input_capacity {
            return Err(Error::Invalid("payload exceeds input_capacity".into()));
        }
        self.input_buffer.fill(0);
        self.input_buffer[..payload.len()].copy_from_slice(payload);
        self.vm
            .debug_write_physical(&[(self.harness.input_gpa, &self.input_buffer)])?;
        let mut case_watchpoints = self.harness.watchpoints.clone();
        if !case_watchpoints.is_empty() {
            let state = self.vm.debug_registers(self.harness.cpu)?;
            let registers = Registers::from_state(&state)
                .map_err(|error| Error::Invalid(format!("ARM64 watchpoint state: {error}")))?;
            let address_space = AddressSpace {
                memory: &self.vm,
                registers: &registers,
            };
            for watch in &mut case_watchpoints {
                if let Some(offset) = watch.input_end_offset {
                    let base = self
                        .harness
                        .input_va
                        .ok_or_else(|| Error::Invalid("input_end_offset needs input_va".into()))?;
                    watch.address = if offset < 0 {
                        base.checked_add(payload.len() as u64)
                            .and_then(|end| end.checked_sub(offset.unsigned_abs() as u64))
                    } else {
                        base.checked_add(payload.len() as u64)
                            .and_then(|end| end.checked_add(offset as u64))
                    }
                    .ok_or_else(|| Error::Invalid("input-relative watchpoint overflow".into()))?;
                }
                if let Some(value) = watch.poison_value {
                    let physical = address_space
                        .translate(watch.address)
                        .map_err(|error| {
                            Error::Invalid(format!("translate poisoned range: {error}"))
                        })?
                        .physical;
                    let poison = vec![value; watch.size as usize];
                    self.vm.debug_write_physical(&[(physical, &poison)])?;
                    trace_field(
                        &self.vm,
                        "poison marker",
                        watch.address,
                        physical,
                        poison.len(),
                    )?;
                }
            }
        }
        if let Some(value) = ioctl {
            if let Some(address) = self.harness.ioctl_va {
                let state = self.vm.debug_registers(self.harness.cpu)?;
                let registers = Registers::from_state(&state)
                    .map_err(|error| Error::Invalid(format!("ARM64 register state: {error}")))?;
                let address_space = AddressSpace {
                    memory: &self.vm,
                    registers: &registers,
                };
                let physical = address_space
                    .translate(address)
                    .map_err(|error| Error::Invalid(format!("translate IOCTL field: {error}")))?
                    .physical;
                self.vm
                    .debug_write_physical(&[(physical, &value.to_le_bytes())])?;
                trace_field(&self.vm, "IOCTL", address, physical, 4)?;
            } else {
                let register = self.harness.ioctl_register.as_deref().ok_or_else(|| {
                    Error::Invalid("IOCTL testcase needs ioctl_register or ioctl_va".into())
                })?;
                self.vm.debug_write_register(
                    self.harness.cpu,
                    vroom_hv::debug::Register::parse(register)?,
                    value.into(),
                )?;
            }
        }
        if let Some(value) = self.harness.input_va {
            let state = self.vm.debug_registers(self.harness.cpu)?;
            let registers = Registers::from_state(&state)
                .map_err(|error| Error::Invalid(format!("ARM64 register state: {error}")))?;
            let address_space = AddressSpace {
                memory: &self.vm,
                registers: &registers,
            };
            let physical = address_space
                .translate(value)
                .map_err(|error| Error::Invalid(format!("translate input buffer: {error}")))?
                .physical;
            if physical != self.harness.input_gpa {
                return Err(Error::Invalid(format!(
                    "input_va {value:#x} resolves to {physical:#x}, not input_gpa {:#x}",
                    self.harness.input_gpa
                )));
            }
            if let Some(register) = self.harness.input_register.as_deref() {
                self.vm.debug_write_register(
                    self.harness.cpu,
                    vroom_hv::debug::Register::parse(register)?,
                    value,
                )?;
            } else if let Some(address) = self.harness.input_pointer_va {
                let state = self.vm.debug_registers(self.harness.cpu)?;
                let registers = Registers::from_state(&state)
                    .map_err(|error| Error::Invalid(format!("ARM64 register state: {error}")))?;
                let address_space = AddressSpace {
                    memory: &self.vm,
                    registers: &registers,
                };
                let physical = address_space
                    .translate(address)
                    .map_err(|error| {
                        Error::Invalid(format!("translate input pointer field: {error}"))
                    })?
                    .physical;
                self.vm
                    .debug_write_physical(&[(physical, &value.to_le_bytes())])?;
                trace_field(&self.vm, "input pointer", address, physical, 8)?;
            }
        }
        if let Some(address) = self.harness.input_length_va {
            let state = self.vm.debug_registers(self.harness.cpu)?;
            let registers = Registers::from_state(&state)
                .map_err(|error| Error::Invalid(format!("ARM64 register state: {error}")))?;
            let address_space = AddressSpace {
                memory: &self.vm,
                registers: &registers,
            };
            let physical = address_space
                .translate(address)
                .map_err(|error| Error::Invalid(format!("translate input length field: {error}")))?
                .physical;
            let length = u32::try_from(payload.len())
                .map_err(|_| Error::Invalid("input length does not fit in 32 bits".into()))?;
            self.vm
                .debug_write_physical(&[(physical, &length.to_le_bytes())])?;
            trace_field(&self.vm, "input length", address, physical, 4)?;
        }
        if let Some(address) = self.harness.output_length_va {
            let state = self.vm.debug_registers(self.harness.cpu)?;
            let registers = Registers::from_state(&state)
                .map_err(|error| Error::Invalid(format!("ARM64 register state: {error}")))?;
            let address_space = AddressSpace {
                memory: &self.vm,
                registers: &registers,
            };
            let physical = address_space
                .translate(address)
                .map_err(|error| Error::Invalid(format!("translate output length field: {error}")))?
                .physical;
            let length = u32::try_from(payload.len())
                .map_err(|_| Error::Invalid("input length does not fit in 32 bits".into()))?;
            self.vm
                .debug_write_physical(&[(physical, &length.to_le_bytes())])?;
            trace_field(&self.vm, "output length", address, physical, 4)?;
        }
        if let Some(register) = self.harness.input_length_register.as_deref() {
            self.vm.debug_write_register(
                self.harness.cpu,
                vroom_hv::debug::Register::parse(register)?,
                payload.len() as u64,
            )?;
        }
        Ok(Case { arm_after_patch, reset_us, watchpoints: case_watchpoints })
    }

    pub fn run_input(&mut self, input: &[u8]) -> Result<RunResult> {
        let Case { arm_after_patch, reset_us, watchpoints: case_watchpoints } = self.load_case(input)?;
        if arm_after_patch {
            self.vm.research_arm()?;
        }
        let state = self.vm.debug_registers(self.harness.cpu)?;
        let registers = Registers::from_state(&state)
            .map_err(|error| Error::Invalid(format!("ARM64 register state: {error}")))?;
        if !self.harness.client() {
            trace_dispatch_state(&self.vm, &registers, self.harness.ioctl_va)?;
        }
        let entry_pc = self.harness.entry_pc.unwrap_or(registers.pc);
        let entry_sp = registers.sp;
        let entry_ttbr0 = registers.ttbr0;
        // The one CPU that runs alone, single-stepping a foreign thread past
        // a shared site or the testcase's thread across a call site.
        let mut foreign_step = None;
        let mut own_step = None;
        // Where the testcase's thread resumes after an exception taken
        // during one of its steps.
        let mut exception_return = None;
        let mut foreign_threads = 0u64;
        let mut exceptions = BTreeMap::<(u32, u64, u64), usize>::new();
        // Past stop_pc, running until then (settle_ms).
        let mut settle: Option<Instant> = None;
        let slow_trace = std::env::var_os("VROOM_FUZZ_TRACE").is_some_and(|v| v == "slow");
        let mut iteration = Instant::now();
        let mut last_event = String::new();
        if std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
            eprintln!("start pc={:#x} entry={entry_pc:#x}", registers.pc);
        }
        let client = self.harness.client();
        if !client && !self.block_by_pc.contains_key(&entry_pc) {
            return Err(Error::Invalid(format!(
                "entry_pc {entry_pc:#x} is not in the coverage block list"
            )));
        }
        // The snapshot is already stopped at the first block. Do not plant
        // a software trap there: resume directly and use hardware execution
        // breakpoints on its CFG successors to observe the next block. A
        // client stands outside the blocks.
        let entry_index = self.block_by_pc.get(&entry_pc).copied();
        let always = self.always();
        let blocks_mode = always.brk.is_some();
        // Blocks mode needs no path following: BRKs mark every new block.
        let entry_successors = match entry_index {
            Some(index) if !blocks_mode => self.harness.coverage[index].successors.clone(),
            _ => Vec::new(),
        };
        let mut active_exception_sites = self.harness.exception_sites.clone();
        let mut watch_target = if blocks_mode { None } else { self.next_watch_target() };
        if std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
            eprintln!("watch target={watch_target:?}");
        }
        let initial_breakpoints = breakpoint_set(
            &entry_successors,
            watch_target,
            self.harness.stop_pc,
            &active_exception_sites,
        );
        if std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
            eprintln!("initial breakpoints={initial_breakpoints:?}");
        }
        let mut armed = initial_breakpoints;
        configure_all(&self.vm, &armed, always, &case_watchpoints)?;
        let isolate = self.harness.isolate_cpu.then_some(self.harness.cpu);
        if isolate.is_some() {
            // PSTATE.I and F: no interrupt can preempt the target thread.
            self.vm.debug_write_register(
                self.harness.cpu,
                vroom_hv::debug::Register::Pstate,
                isolated_pstate(registers.pstate),
            )?;
        }
        resume_vm(&mut self.vm, isolate)?;
        let start = Instant::now();
        let token = CorpusStore::hash(input);
        let mut new_blocks = 0;
        let mut new_edges = 0;
        let mut block_hits = BTreeMap::<u64, u64>::new();
        let mut edges_hit = BTreeSet::<(u64, u64)>::new();
        let mut finding = None;
        let mut findings = Vec::<Finding>::new();
        // The faulting stack of this input's exception, for a bugcheck it
        // ends in.
        let mut fault_stack: Option<Vec<String>> = None;
        let mut watch_hits = BTreeMap::<usize, u64>::new();
        // The VM left the snapshot's machine (a timeout, a guest reset, a
        // VMM failure): restore the snapshot rather than reset in place.
        let mut reopen = false;
        let mut previous_pc = None;
        let mut call_step_targets = Vec::new();
        let mut call_step_remaining = 0usize;
        let mut step_to_call_target = false;
        let mut step_successors_once = false;
        if let Some(index) = entry_index {
            let entry_block = &self.harness.coverage[index];
            previous_pc = Some(entry_block.id);
            *block_hits.entry(entry_block.id).or_default() += 1;
            if self.coverage.observe(entry_block, &token) {
                new_blocks += 1;
            }
        }
        loop {
            if slow_trace {
                let took = iteration.elapsed();
                if took > Duration::from_millis(200) {
                    eprintln!("slow iteration {} ms after {last_event}", took.as_millis());
                }
                iteration = Instant::now();
            }
            if settle.is_some_and(|deadline| Instant::now() >= deadline) {
                self.vm.pause()?;
                break;
            }
            if start.elapsed() > Duration::from_millis(self.harness.timeout_ms) {
                self.vm.pause()?;
                if std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
                    let state = self.vm.debug_registers(self.harness.cpu)?;
                    let registers = Registers::from_state(&state).map_err(|error| {
                        Error::Invalid(format!("ARM64 register state: {error}"))
                    })?;
                    let stack = self.stack_of(&registers);
                    eprintln!("timeout at pc={:#x} {stack:?}", registers.pc);
                }
                reopen = true;
                finding = detect("execution timeout", None, &token, true);
                if let Some(found) = &finding {
                    findings.push(found.clone());
                }
                break;
            }
            let event = self.vm.next_event(Duration::from_millis(5));
            if slow_trace {
                last_event = match &event {
                    Some(vroom_vmm::VmEvent::Debug(stop)) => format!("stop cpu {} pc {:#x} {:?}", stop.cpu, stop.pc, stop.reason),
                    Some(other) => format!("{other:?}").chars().take(60).collect(),
                    None => "no event".into(),
                };
            }
            let Some(event) = event else {
                continue;
            };
            match event {
                vroom_vmm::VmEvent::Debug(stop) => {
                    if std::env::var_os("VROOM_FUZZ_TRACE").is_some_and(|v| v == "2") {
                        eprintln!("debug stop cpu={} pc={:#x} {:?}", stop.cpu, stop.pc, stop.reason);
                    }
                    if own_step.or(foreign_step).is_some_and(|cpu| cpu != stop.cpu) {
                        // A stop queued before the step began: that CPU
                        // stays parked on a still-armed breakpoint and stops
                        // there again once every CPU resumes.
                        continue;
                    }
                    self.vm.pause()?;
                    if self.harness.bugcheck_pc == Some(stop.pc) {
                        // Any thread: the kernel is going down. Stop before
                        // it does, so an in-place reset recovers the VM.
                        let state = self.vm.debug_registers(stop.cpu as usize)?;
                        let registers = Registers::from_state(&state).map_err(|error| {
                            Error::Invalid(format!("ARM64 bugcheck registers: {error}"))
                        })?;
                        let x = &registers.x;
                        let bugcheck = Finding {
                            class: BugClass::BugCheck,
                            detail: format!(
                                "bugcheck {:#x} ({:#x}, {:#x}, {:#x}, {:#x}) from {:#x} on cpu {}",
                                x[0], x[1], x[2], x[3], x[4], x[30], stop.cpu
                            ),
                            pc: Some(stop.pc),
                            input: token.clone(),
                            stack: match fault_stack.clone() {
                                Some(stack) => stack,
                                None => self.bugcheck_stack(&registers),
                            },
                        };
                        if std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
                            eprintln!("{}", bugcheck.detail);
                        }
                        if finding.is_none() {
                            finding = Some(bugcheck.clone());
                        }
                        findings.push(bugcheck);
                        break;
                    }
                    if stop.reason == vroom_hv::debug::DebugReason::SoftwareBreakpoint {
                        // A coverage BRK (blocks mode): the only BRK that stops.
                        let index = self.block_by_pc.get(&stop.pc).copied().ok_or_else(|| {
                            Error::Invalid(format!("coverage breakpoint at unknown pc {:#x}", stop.pc))
                        })?;
                        // Another CPU may have taken this one out already; its
                        // instruction runs now either way.
                        if self.unplant(index)? {
                            let block = &self.harness.coverage[index];
                            *block_hits.entry(block.id).or_default() += 1;
                            if self.coverage.observe(block, &token) {
                                new_blocks += 1;
                            }
                            if std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
                                eprintln!("new block {} at {:#x} on cpu {}", block.id, stop.pc, stop.cpu);
                            }
                        }
                        // A step this stop interrupted is over too.
                        own_step = None;
                        foreign_step = None;
                        configure_all(&self.vm, &armed, always, &case_watchpoints)?;
                        resume_vm(&mut self.vm, isolate)?;
                        continue;
                    }
                    let stepped_own = own_step.take().is_some();
                    if foreign_step.take().is_some()
                        || (stop.reason == vroom_hv::debug::DebugReason::SingleStep && !stepped_own)
                    {
                        // A foreign thread is past the shared site (or a
                        // step a re-arm already cancelled): re-arm, resume.
                        configure_all(&self.vm, &armed, always, &case_watchpoints)?;
                        resume_vm(&mut self.vm, isolate)?;
                        continue;
                    }
                    if stepped_own && stop.reason == vroom_hv::debug::DebugReason::SingleStep {
                        let state = self.vm.debug_registers(stop.cpu as usize)?;
                        let registers = Registers::from_state(&state).map_err(|error| {
                            Error::Invalid(format!("ARM64 step registers: {error}"))
                        })?;
                        if is_exception_vector(stop.pc, registers.vbar) {
                            // An interrupt (or fault) was taken before the
                            // stepped instruction ran; ELR_EL1 holds it. Let
                            // the handler and any preemption run, then step
                            // that instruction again when the thread is back.
                            let elr = elr_el1(&state)
                                .ok_or_else(|| Error::Invalid("ELR_EL1 missing".into()))?;
                            if std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
                                eprintln!("step took an exception at {:#x}; resume at {elr:#x}", stop.pc);
                            }
                            exception_return = Some(elr);
                            armed = breakpoint_set(
                                &[elr],
                                watch_target,
                                self.harness.stop_pc,
                                &active_exception_sites,
                            );
                            configure_all(&self.vm, &armed, always, &case_watchpoints)?;
                            resume_vm(&mut self.vm, isolate)?;
                            continue;
                        }
                    }
                    if exception_return == Some(stop.pc)
                        && stop.reason == vroom_hv::debug::DebugReason::Breakpoint
                    {
                        exception_return = None;
                        self.vm.debug_step(stop.cpu as usize, step_debug_config(&case_watchpoints, always))?;
                        own_step = Some(stop.cpu);
                        continue;
                    }
                    // stop_pc and exception sites sit in shared kernel code:
                    // any thread scheduled after a preemption can reach them.
                    // Only the testcase's thread (its stack, its address
                    // space) ends the run or reports a finding.
                    if self.exception_hook == Some(stop.pc) {
                        let state = self.vm.debug_registers(stop.cpu as usize)?;
                        let record = Registers::from_state(&state).map(|r| r.x[0]).unwrap_or(0);
                        let mut word = [0u8; 4];
                        let mut address = [0u8; 8];
                        let mut fault = [0u8; 8];
                        let space = AddressSpace { memory: &self.vm, registers: &Registers::from_state(&state).map_err(|e| Error::Invalid(format!("ARM64 registers: {e}")))? };
                        let _ = vroom_kernel::windows::VirtualMemory::read(&space, record, &mut word);
                        let _ = vroom_kernel::windows::VirtualMemory::read(&space, record + 0x10, &mut address);
                        let _ = vroom_kernel::windows::VirtualMemory::read(&space, record + 0x28, &mut fault);
                        // The identical fault: the memory manager's handled
                        // page-table faults (another process starting) repeat
                        // the PC, not the faulting address.
                        let key = (u32::from_le_bytes(word), u64::from_le_bytes(address), u64::from_le_bytes(fault));
                        let seen = exceptions.entry(key).or_default();
                        *seen += 1;
                        // The system's own handled faults repeat too (the
                        // memory manager's while a process starts): only
                        // many more of those make a storm.
                        // User-mode exceptions (x3, PreviousMode) are the
                        // processes' own business.
                        let kernel = Registers::from_state(&state).is_ok_and(|r| r.x[3] & 0xff == 0);
                        let pc = u64::from_le_bytes(address);
                        let ours_pc = self.target_image.is_none_or(|(lo, hi)| (lo..hi).contains(&pc))
                            || (pc >> 63 == 1 && self.symbols.as_ref().is_some_and(|s| !s.in_module(pc)));
                        if kernel && *seen > if ours_pc { MAX_EXCEPTIONS } else { MAX_SYSTEM_EXCEPTIONS } {
                            // The same fault over and over (a smashed frame
                            // faulting in its own dispatch): it won't end,
                            // and what started it is reported already.
                            if std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
                                eprintln!("exception storm: {key:x?} {} times", *seen);
                            }
                            break;
                        }
                    }
                    let shared_site = self.harness.stop_pc == Some(stop.pc)
                        || self.harness.exception_sites.contains(&stop.pc);
                    if shared_site {
                        let state = self.vm.debug_registers(stop.cpu as usize)?;
                        let registers = Registers::from_state(&state).map_err(|error| {
                            Error::Invalid(format!("ARM64 stop registers: {error}"))
                        })?;
                        let returned = self.harness.stop_pc == Some(stop.pc);
                        // KiDispatchException runs on the exception stack:
                        // whose exception it is, the trap frame says.
                        let sp = if self.exception_hook == Some(stop.pc) {
                            self.trap_sp(&registers).unwrap_or(registers.sp)
                        } else {
                            registers.sp
                        };
                        // A client's stop_pc is in its own process; its
                        // input's exceptions may be any thread's (the
                        // driver's workers): the target range decides.
                        let ours = if client {
                            returned.then_some(registers.ttbr0 == entry_ttbr0).unwrap_or(true)
                        } else {
                            registers.ttbr0 == entry_ttbr0
                            && if returned {
                                registers.sp > entry_sp && registers.sp - entry_sp <= RETURN_FRAMES
                            } else {
                                sp.abs_diff(entry_sp) <= KERNEL_STACK
                            }
                        };
                        if std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
                            eprintln!(
                                "shared site cpu={} pc={:#x} sp={sp:#x} entry_sp={entry_sp:#x} ours={ours}",
                                stop.cpu, stop.pc
                            );
                        }
                        if !ours {
                            foreign_threads += 1;
                            foreign_step = Some(stop.cpu);
                            self.vm.debug_step(stop.cpu as usize, step_debug_config(&[], always))?;
                            continue;
                        }
                    }
                    if self.harness.stop_pc == Some(stop.pc) {
                        if client && std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
                            // What the client says (its first argument).
                            if let Ok(Ok(r)) = self.vm.debug_registers(stop.cpu as usize).map(|s| Registers::from_state(&s)) {
                                let mut words = [0u8; 64];
                                let space = AddressSpace { memory: &self.vm, registers: &r };
                                let _ = vroom_kernel::windows::VirtualMemory::read(&space, r.x[0], &mut words);
                                eprintln!("stop_pc x0={:#x}: {}", r.x[0], words.chunks(4).map(|w| format!("{:08x}", u32::from_le_bytes(w.try_into().expect("four")))).collect::<Vec<_>>().join(" "));
                            }
                        }
                        if client && self.harness.settle_ms > 0 && settle.is_none() {
                            // Past the breakpoint; the client parks again.
                            settle = Some(Instant::now() + Duration::from_millis(self.harness.settle_ms));
                            foreign_step = Some(stop.cpu);
                            self.vm.debug_step(stop.cpu as usize, step_debug_config(&[], always))?;
                            continue;
                        }
                        break;
                    }
                    if stop.reason == vroom_hv::debug::DebugReason::Watchpoint {
                        let watch_index = (case_watchpoints.len() == 1).then_some(0);
                        if let Some(index) = watch_index {
                            let watch = &case_watchpoints[index];
                            let count = watch_hits.entry(index).or_default();
                            *count += 1;
                            let poisoned = watch.poisoned && *count == 1;
                            let double = watch.double_fetch && watch.read && *count == 2;
                            let address = watch.address;
                            let stack = if poisoned || double {
                                match self.vm.debug_registers(stop.cpu as usize).map(|s| Registers::from_state(&s)) {
                                    Ok(Ok(registers)) => self.stack_of(&registers),
                                    _ => Vec::new(),
                                }
                            } else {
                                Vec::new()
                            };
                            if poisoned {
                                findings.push(Finding {
                                    class: BugClass::PoisonedAccess,
                                    detail: format!("watched memory access at {address:#x}"),
                                    pc: Some(stop.pc),
                                    input: token.clone(),
                                    stack: stack.clone(),
                                });
                            }
                            if double {
                                findings.push(Finding {
                                    class: BugClass::DoubleFetch,
                                    detail: format!("second read from watched address {address:#x}"),
                                    pc: Some(stop.pc),
                                    input: token.clone(),
                                    stack,
                                });
                            }
                            if finding.is_none() {
                                finding = findings.first().cloned();
                            }
                        }
                        let successors = previous_pc
                            .and_then(|id| {
                                self.harness.coverage.iter().find(|block| block.id == id)
                            })
                            .map(|block| block.successors.as_slice())
                            .unwrap_or(&[]);
                        armed = breakpoint_set(
                            successors,
                            watch_target,
                            self.harness.stop_pc,
                            &active_exception_sites,
                        );
                        configure_all(&self.vm, &armed, always, &case_watchpoints)?;
                        resume_vm(&mut self.vm, isolate)?;
                        continue;
                    }
                    if self.exception_hook == Some(stop.pc) {
                        let state = self.vm.debug_registers(stop.cpu as usize)?;
                        let registers = Registers::from_state(&state).map_err(|error| {
                            Error::Invalid(format!("ARM64 exception registers: {error}"))
                        })?;
                        if let Some(found) = self.exception_finding(&registers, &token)? {
                            if !findings.iter().any(|f| f.class == found.class && f.pc == found.pc) {
                                if std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
                                    eprintln!("{}", found.detail);
                                }
                                fault_stack = Some(found.stack.clone());
                                if finding.is_none() {
                                    finding = Some(found.clone());
                                }
                                findings.push(found);
                            }
                        }
                        // Step past the hook and keep it armed: the next
                        // exception may be the one that matters.
                        foreign_step = Some(stop.cpu);
                        self.vm.debug_step(stop.cpu as usize, step_debug_config(&[], always))?;
                        continue;
                    }
                    if self.harness.exception_sites.contains(&stop.pc) {
                        active_exception_sites.retain(|site| *site != stop.pc);
                        let stack = match self.vm.debug_registers(stop.cpu as usize).map(|s| Registers::from_state(&s)) {
                            Ok(Ok(registers)) => self.stack_of(&registers),
                            _ => Vec::new(),
                        };
                        let exception_finding = Finding {
                            class: BugClass::ExceptionDispatch,
                            detail: "configured exception-dispatch site reached".into(),
                            pc: Some(stop.pc),
                            input: token.clone(),
                            stack,
                        };
                        if finding.is_none() {
                            finding = Some(exception_finding.clone());
                        }
                        findings.push(exception_finding);
                        let successors = previous_pc
                            .and_then(|id| {
                                self.harness.coverage.iter().find(|block| block.id == id)
                            })
                            .map(|block| block.successors.as_slice())
                            .unwrap_or(&[]);
                        armed = breakpoint_set(
                            successors,
                            watch_target,
                            self.harness.stop_pc,
                            &active_exception_sites,
                        );
                        configure_all(&self.vm, &armed, always, &case_watchpoints)?;
                        resume_vm(&mut self.vm, isolate)?;
                        continue;
                    }
                    if watch_target == Some(stop.pc) {
                        watch_target = self.next_watch_target();
                    }
                    let block = self
                        .block_by_pc
                        .get(&stop.pc)
                        .and_then(|index| self.harness.coverage.get(*index));
                    if std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
                        eprintln!(
                            "stop cpu={} pc={:#x} reason={:?} block={:?}",
                            stop.cpu,
                            stop.pc,
                            stop.reason,
                            block.map(|row| row.id)
                        );
                        let state = self.vm.debug_registers(stop.cpu as usize)?;
                        let registers = Registers::from_state(&state).map_err(|error| {
                            Error::Invalid(format!("ARM64 stop registers: {error}"))
                        })?;
                        let address_space = AddressSpace {
                            memory: &self.vm,
                            registers: &registers,
                        };
                        if let Ok(translated) = address_space.translate(registers.pc) {
                            trace_field(
                                &self.vm,
                                "stopped code",
                                registers.pc,
                                translated.physical,
                                16,
                            )?;
                        }
                        eprintln!(
                            "stop registers: pc={:#x} x1={:#x} x3={:#x} x19={:#x} x21={:#x}",
                            registers.pc,
                            registers.x[1],
                            registers.x[3],
                            registers.x[19],
                            registers.x[21]
                        );
                    }
                    if let Some(block) = block {
                        if let Some(previous) = previous_pc {
                            edges_hit.insert((previous, block.id));
                            if self.coverage.observe_edge(previous, block.id) {
                                new_edges += 1;
                            }
                        }
                        previous_pc = Some(block.id);
                        *block_hits.entry(block.id).or_default() += 1;
                        if self.coverage.observe(block, &token) {
                            new_blocks += 1;
                        }
                    }
                    if stop.reason == vroom_hv::debug::DebugReason::SingleStep {
                        if !call_step_targets.is_empty() {
                            if call_step_targets.contains(&stop.pc) {
                                call_step_targets.clear();
                                call_step_remaining = 0;
                                if step_to_call_target {
                                    step_to_call_target = false;
                                    step_successors_once = true;
                                }
                            } else if call_step_remaining > 0 {
                                call_step_remaining -= 1;
                                if call_step_remaining > 0 {
                                    self.vm.debug_step(
                                        stop.cpu as usize,
                                        step_debug_config(&case_watchpoints, always),
                                    )?;
                                    own_step = Some(stop.cpu);
                                    continue;
                                }
                                call_step_targets.clear();
                                step_to_call_target = false;
                            } else {
                                call_step_targets.clear();
                                step_to_call_target = false;
                            }
                        }
                    }
                    if stop.reason == vroom_hv::debug::DebugReason::Breakpoint {
                        if let Some(block) = block {
                            // Hardware execution breakpoints can miss a direct
                            // call target on some hypervisor/page combinations.
                            // Step only across the short call-site block, then
                            // resume ordinary successor breakpoints in the callee.
                            let call_target = block.successors.iter().copied().find(|target| {
                                self.block_by_pc
                                    .get(target)
                                    .and_then(|index| self.harness.coverage.get(*index))
                                    .is_some_and(|callee| callee.function != block.function)
                            });
                            if let Some(target) = call_target {
                                let steps = (block.size as usize / 4).clamp(1, 64);
                                call_step_targets = vec![target];
                                call_step_remaining = steps;
                                step_to_call_target = true;
                                self.vm.debug_step(
                                    stop.cpu as usize,
                                    step_debug_config(&case_watchpoints, always),
                                )?;
                                own_step = Some(stop.cpu);
                                continue;
                            }
                            let breakpoints = breakpoint_set(
                                &block.successors,
                                watch_target,
                                self.harness.stop_pc,
                                &active_exception_sites,
                            );
                            if std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
                                eprintln!("arm block {} successors {:?}", block.id, breakpoints);
                            }
                            armed = breakpoints;
                            configure_all(&self.vm, &armed, always, &case_watchpoints)?;
                            resume_vm(&mut self.vm, isolate)?;
                        }
                    } else if stop.reason == vroom_hv::debug::DebugReason::SingleStep {
                        if let Some(block) = block {
                            if step_successors_once
                                && block.gpa.is_some()
                                && !block.successors.is_empty()
                            {
                                step_successors_once = false;
                                call_step_targets.clone_from(&block.successors);
                                call_step_remaining = (block.size as usize / 4).clamp(1, 64);
                                self.vm.debug_step(
                                    stop.cpu as usize,
                                    step_debug_config(&case_watchpoints, always),
                                )?;
                                own_step = Some(stop.cpu);
                                continue;
                            }
                            step_successors_once = false;
                            let breakpoints = breakpoint_set(
                                &block.successors,
                                watch_target,
                                self.harness.stop_pc,
                                &active_exception_sites,
                            );
                            armed = breakpoints;
                            configure_all(&self.vm, &armed, always, &case_watchpoints)?;
                            resume_vm(&mut self.vm, isolate)?;
                        } else {
                            armed = breakpoint_set(
                                &[],
                                watch_target,
                                self.harness.stop_pc,
                                &active_exception_sites,
                            );
                            configure_all(&self.vm, &armed, always, &case_watchpoints)?;
                            resume_vm(&mut self.vm, isolate)?;
                        }
                    }
                }
                vroom_vmm::VmEvent::Crashed(detail) => {
                    self.vm.pause()?;
                    reopen = true;
                    finding = detect(&detail, None, &token, false);
                    if let Some(found) = &finding {
                        findings.push(found.clone());
                    }
                    break;
                }
                vroom_vmm::VmEvent::Shutdown => {
                    self.vm.pause()?;
                    break;
                }
                vroom_vmm::VmEvent::Reset => {
                    self.vm.pause()?;
                    reopen = true;
                    if self.harness.reset_is_bugcheck {
                        finding = detect("guest reset after bugcheck", None, &token, false);
                        if let Some(found) = &finding {
                            findings.push(found.clone());
                        }
                    }
                    break;
                }
                _ => {}
            }
        }
        self.coverage.finish_execution();
        if blocks_mode && !reopen {
            self.plant_pending()?;
        }
        Ok(RunResult {
            input_hash: token,
            new_blocks,
            new_edges,
            block_hits,
            edges_hit,
            finding,
            findings,
            elapsed_us: start.elapsed().as_micros() as u64,
            foreign_threads,
            reset_us,
            reopen,
        })
    }
}

fn trace_field(
    vm: &vroom_vmm::Vmm,
    label: &str,
    virtual_address: u64,
    physical_address: u64,
    size: usize,
) -> Result<()> {
    if std::env::var_os("VROOM_FUZZ_TRACE").is_none() {
        return Ok(());
    }
    let mut bytes = vec![0; size];
    vm.debug_read_physical(physical_address, &mut bytes)?;
    eprintln!(
        "patched {label}: VA={virtual_address:#x} PA={physical_address:#x} bytes={}",
        hex::encode(bytes)
    );
    Ok(())
}

fn trace_dispatch_state(
    vm: &vroom_vmm::Vmm,
    registers: &Registers,
    ioctl_va: Option<u64>,
) -> Result<()> {
    if std::env::var_os("VROOM_FUZZ_TRACE").is_none() {
        return Ok(());
    }
    let irp = registers.x[1];
    let current_stack_va = irp + 0xb8;
    let address_space = AddressSpace {
        memory: vm,
        registers,
    };
    let current_stack_pa = address_space
        .translate(current_stack_va)
        .map_err(|error| Error::Invalid(format!("trace current stack pointer: {error}")))?
        .physical;
    let mut pointer_bytes = [0; 8];
    vm.debug_read_physical(current_stack_pa, &mut pointer_bytes)?;
    let current_stack = u64::from_le_bytes(pointer_bytes);
    let current_ioctl_va = current_stack + 0x18;
    let current_ioctl_pa = address_space
        .translate(current_ioctl_va)
        .map_err(|error| Error::Invalid(format!("trace dispatch IOCTL field: {error}")))?
        .physical;
    let mut ioctl_bytes = [0; 4];
    vm.debug_read_physical(current_ioctl_pa, &mut ioctl_bytes)?;
    eprintln!(
        "dispatch args: IRP={irp:#x} stack_va={current_stack:#x} code_va={current_ioctl_va:#x} code={} harness_code_va={ioctl_va:?}",
        hex::encode(ioctl_bytes)
    );
    Ok(())
}

fn breakpoint_set(
    successors: &[u64],
    watch_target: Option<u64>,
    stop_pc: Option<u64>,
    exception_sites: &[u64],
) -> Vec<u64> {
    let mut breakpoints = Vec::with_capacity(4);
    for address in stop_pc
        .into_iter()
        .chain(exception_sites.iter().copied())
        .chain(successors.iter().copied())
        .chain(watch_target)
    {
        if !breakpoints.contains(&address) && breakpoints.len() < 4 {
            breakpoints.push(address);
        }
    }
    breakpoints
}

/// How far above the snapshot's SP the caller's frame at `stop_pc` may be
/// (the frames between the target call and the entry block).
const RETURN_FRAMES: u64 = 0x4000;
/// How far an exception-dispatch site can be from the entry SP on the same
/// kernel stack.
const KERNEL_STACK: u64 = 0x8000;

/// Resume every CPU, or only the isolated one.
fn resume_vm(vm: &mut vroom_vmm::Vmm, isolate: Option<usize>) -> Result<()> {
    match isolate {
        Some(cpu) => vm.debug_resume_cpu(cpu)?,
        None => vm.resume()?,
    }
    Ok(())
}

/// PSTATE for the testcase's thread running alone: IRQs and FIQs masked
/// (I, F), and no software step pending (SS: a comparison log or trace
/// stepped this CPU before the reset).
pub(crate) fn isolated_pstate(pstate: u64) -> u64 {
    (pstate | 0xc0) & !(1 << 21)
}

/// The first instruction of an AArch64 exception vector entry.
fn is_exception_vector(pc: u64, vbar: u64) -> bool {
    pc.checked_sub(vbar)
        .is_some_and(|offset| offset < 0x800 && offset % 0x80 == 0)
}

fn elr_el1(state: &vroom_hv::VcpuState) -> Option<u64> {
    if state.backend == "kvm" {
        let id = 0x6030_0000_0013_0000u64 | 0xc201;
        let bytes = &state.raw.iter().find(|(r, _)| *r == id)?.1;
        return Some(u64::from_le_bytes(bytes.as_slice().try_into().ok()?));
    }
    state.regs.iter().find(|r| r.0 == 0x1c201).map(|r| r.1)
}

/// Arm the same breakpoints on every vCPU: the testcase's thread can be
/// preempted and resumed on another CPU. The bugcheck entry takes a slot of
/// its own after the (at most four) coverage breakpoints.
fn configure_all(
    vm: &vroom_vmm::Vmm,
    breakpoints: &[u64],
    always: Always,
    watches: &[MemoryWatch],
) -> Result<()> {
    let mut breakpoints = breakpoints.to_vec();
    if let Some(pc) = always.bugcheck.filter(|pc| !breakpoints.contains(pc)) {
        breakpoints.push(pc);
    }
    for cpu in 0..vm.config().cpus as usize {
        vm.debug_configure(
            cpu,
            vroom_hv::debug::DebugConfig {
                enabled: true,
                breakpoints: breakpoints.clone(),
                watchpoints: hv_watchpoints(watches),
                trap_brk_immediate: always.brk,
                ..Default::default()
            },
        )?;
    }
    Ok(())
}

fn hv_watchpoints(watches: &[MemoryWatch]) -> Vec<vroom_hv::debug::Watchpoint> {
    watches
        .iter()
        .map(|watch| vroom_hv::debug::Watchpoint {
            address: watch.address,
            size: watch.size,
            read: watch.read,
            write: watch.write,
        })
        .collect()
}

fn step_debug_config(watches: &[MemoryWatch], always: Always) -> vroom_hv::debug::DebugConfig {
    vroom_hv::debug::DebugConfig {
        enabled: true,
        single_step: true,
        watchpoints: hv_watchpoints(watches),
        // A stepped thread can land on a coverage BRK too.
        trap_brk_immediate: always.brk,
        ..Default::default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub input_hash: String,
    pub new_blocks: usize,
    pub new_edges: usize,
    /// Per-input hit counts and edges, independent of earlier campaign inputs.
    pub block_hits: BTreeMap<u64, u64>,
    pub edges_hit: BTreeSet<(u64, u64)>,
    pub finding: Option<Finding>,
    /// All independent signals observed for this input. `finding` remains the
    /// first one for older result consumers.
    #[serde(default)]
    pub findings: Vec<Finding>,
    /// Wall-clock iteration time, including reset, instrumentation and guest
    /// execution. Keeping it beside coverage makes target comparisons
    /// reproducible instead of relying on console timing.
    pub elapsed_us: u64,
    /// Times another thread reached `stop_pc` or an exception site during
    /// this input and was stepped past.
    #[serde(default)]
    pub foreign_threads: u64,
    /// Time spent restoring the COW baseline before this input.
    #[serde(default)]
    pub reset_us: u64,
    /// The input ended in a timeout, a guest reset or a VMM failure, so the
    /// in-place reset can't be trusted: restore the snapshot before the
    /// next input. A bugcheck stopped at `bugcheck_pc` doesn't need this.
    #[serde(default)]
    pub reopen: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn coverage_summary_explains_function_progress() {
        let blocks = vec![
            CoverageBlock {
                id: 1,
                gpa: Some(0x1000),
                va: Some(0x8000),
                size: 4,
                successors: vec![0x8004],
                module: "driver".into(),
                function: "dispatch".into(),
                source: None,
            },
            CoverageBlock {
                id: 2,
                gpa: Some(0x1004),
                va: Some(0x8004),
                size: 4,
                successors: vec![],
                module: "driver".into(),
                function: "dispatch".into(),
                source: None,
            },
        ];
        let mut map = CoverageMap::default();
        assert!(map.observe(&blocks[0], "a"));
        assert!(!map.observe(&blocks[0], "b"));
        let s = map.summary(&blocks);
        assert_eq!((s.total_blocks, s.covered_blocks), (2, 1));
        assert_eq!(s.functions["dispatch"], (2, 1, 2));
    }
    #[test]
    fn detectors_keep_bug_classes_distinct() {
        assert_eq!(
            detect("POOL_CORRUPTION", None, "x", false).unwrap().class,
            BugClass::PoolCorruption
        );
        assert_eq!(
            detect("double-free in allocator", Some(4), "x", false)
                .unwrap()
                .class,
            BugClass::DoubleFree
        );
        assert_eq!(
            detect("", None, "x", true).unwrap().class,
            BugClass::Timeout
        );
    }
    #[test]
    fn sequences_mutate_by_calls() {
        let seed = sequence(&[(0, vec![0, 0, 3, 0]), (1, vec![0, 0, 0, 8, 0, 0]), (4, b"hello".to_vec())]);
        assert_eq!(calls(&seed).len(), 3);
        assert_eq!(sequence(&calls(&seed)), seed);
        let mut m = Mutator::new(7, 256);
        let mut lengths = std::collections::BTreeSet::new();
        for _ in 0..500 {
            let out = m.mutate_sequence(&seed, &seed);
            assert!(out.len() <= 256);
            let c = calls(&out);
            assert!(!c.is_empty());
            assert_eq!(sequence(&c), out, "every mutation is whole calls");
            lengths.insert(c.len());
        }
        assert!(lengths.len() > 3, "calls are inserted and dropped: {lengths:?}");
        // A cut-short last call keeps its bytes.
        assert_eq!(calls(&seed[..seed.len() - 2]).last().unwrap().1, b"hel");
    }

    #[test]
    fn a_sequence_compares_by_call_codes_and_first_words() {
        let h: Harness = serde_json::from_value(serde_json::json!({
            "name": "s", "config": "c", "snapshot": "s", "state_dir": "d",
            "input_gpa": 0, "input_capacity": 4096, "sequence": true,
        }))
        .unwrap();
        let input = sequence(&[(0, vec![0, 0, 3]), (10, vec![0; 9]), (4, vec![0; 5])]);
        // Codes at 0, 9, 24; words at 15, 19 (9 bytes) and 30 (5 bytes).
        assert_eq!(cmp_fields(&h, &input), vec![0, 9, 15, 19, 24, 30]);
    }

    #[test]
    fn signatures_start_at_the_faulting_code() {
        let f = |stack: &[&str]| Finding {
            class: BugClass::BugCheck,
            detail: String::new(),
            pc: Some(1),
            input: String::new(),
            stack: stack.iter().map(|s| s.to_string()).collect(),
        };
        let a = f(&["nt!KeBugCheckEx", "nt!MiSystemFault+0x8b0", "nt!KiKernelSp0ExceptionHandler+0x5c", "HEVD!sub_a+0x10", "HEVD!sub_b+0x4", "nt!IofCallDriver+0x78"]);
        assert_eq!(signature(&a), "BugCheck:HEVD!sub_a<HEVD!sub_b<nt!IofCallDriver");
        let b = f(&["nt!KeBugCheckEx", "nt!MiSystemFault+0x8b0", "nt!KiKernelSp0ExceptionHandler+0x5c", "HEVD!sub_a+0x14", "HEVD!sub_b+0x4", "nt!IofCallDriver+0x78"]);
        assert_eq!(signature(&a), signature(&b), "same function, another offset: the same bug");
        let c = f(&["clfs!X+0x8", "clfs!Y+0x4"]);
        assert_eq!(signature(&c), "BugCheck:clfs!X<clfs!Y");
        assert_eq!(signature(&f(&[])), "BugCheck:Some(1)");
        assert_eq!(signature(&f(&["<wild jump to 0x4141414141414141>"])), "BugCheck:<wild jump>");
    }

    #[test]
    fn ioctl_mutations_keep_device_and_learn_codes() {
        let mut m = Mutator::new(7, 64);
        let seed = [0x03, 0x20, 0x22, 0x00, b'A', b'A'];
        let codes: std::collections::BTreeSet<u32> =
            (0..2000).map(|_| u32::from_le_bytes(m.mutate_ioctl(&seed)[..4].try_into().unwrap())).collect();
        assert!(codes.len() > 4, "the code changes");
        assert!(codes.iter().all(|c| c >> 14 == 0x222003 >> 14), "only function and method move: {codes:x?}");
        m.learn(0x0022_206f);
        m.learn(3);
        assert_eq!(m.dictionary(), [0x0022_206f], "tiny values aren't kept");
        assert!((0..2000).any(|_| m.mutate_ioctl(&seed)[..4] == 0x0022_206fu32.to_le_bytes()));
    }

    #[test]
    fn mutator_is_bounded_and_reproducible() {
        let a = Mutator::new(7, 32).mutate(b"seed");
        let b = Mutator::new(7, 32).mutate(b"seed");
        assert_eq!(a, b);
        assert!(a.len() <= 32);
    }

    #[test]
    fn mutator_preserves_ioctl_prefix() {
        let seed = [0x22, 0x33, 0x44, 0x55, 1, 2, 3, 4];
        let mut mutator = Mutator::new(9, 32);
        for _ in 0..64 {
            let input = mutator.mutate_preserving_prefix(&seed, 4);
            assert_eq!(&input[..4], &seed[..4]);
            assert!(input.len() <= 32);
        }
    }

    #[test]
    fn weighted_parent_choice_uses_positive_weights() {
        let mut mutator = Mutator::new(13, 32);
        for _ in 0..32 {
            assert_eq!(mutator.choose_weighted(&[0, 0, 7]), 2);
        }
    }

    #[test]
    fn breakpoint_set_reserves_a_slot_for_unresolved_targets() {
        assert_eq!(
            breakpoint_set(&[10, 11, 12], Some(20), Some(30), &[]),
            vec![30, 10, 11, 12]
        );
    }

    #[test]
    fn coverage_breakpoints_are_brk_with_the_fuzzer_immediate() {
        assert_eq!(brk_instruction(0), 0xd420_0000u32.to_le_bytes());
        assert_eq!(brk_instruction(0xf000), 0xd43e_0000u32.to_le_bytes());
        let word = u32::from_le_bytes(brk_instruction(COVERAGE_BRK));
        assert_eq!((word >> 5) & 0xffff, u32::from(COVERAGE_BRK));
    }

    #[test]
    fn coverage_mode_defaults_to_edges() {
        #[derive(serde::Deserialize)]
        struct Only {
            #[serde(default)]
            coverage_mode: CoverageMode,
        }
        let parse = |text: &str| serde_json::from_str::<Only>(text).unwrap().coverage_mode;
        assert_eq!(parse("{}"), CoverageMode::Edges);
        assert_eq!(parse(r#"{"coverage_mode":"blocks"}"#), CoverageMode::Blocks);
    }

    #[test]
    fn breakpoint_set_keeps_exception_sites_armed() {
        assert_eq!(
            breakpoint_set(&[10, 11, 12], None, None, &[40]),
            vec![40, 10, 11, 12]
        );
    }
}

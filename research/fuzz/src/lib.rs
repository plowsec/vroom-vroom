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

/// A basic block known to the coverage database. `gpa` is used by the first
/// runner because it is unambiguous before a guest page-table resolver is
/// selected; `va` and symbol fields preserve the analysis provenance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CoverageBlock {
    pub id: u64,
    pub gpa: u64,
    pub va: Option<u64>,
    pub size: u32,
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
    pub checkpoint: PathBuf,
    pub state_dir: PathBuf,
    pub input_gpa: u64,
    pub input_capacity: usize,
    /// Optional guest virtual address passed to the target call. The runner
    /// writes it to `input_register` when a testcase is inserted.
    #[serde(default)]
    pub input_va: Option<u64>,
    /// Treat the first four testcase bytes as a little-endian IOCTL code.
    #[serde(default)]
    pub ioctl_prefix: bool,
    /// ARM64 register names used by a call-site snapshot (for example HEVD's
    /// x1/x2/x3 DeviceIoControl arguments).
    #[serde(default)]
    pub ioctl_register: Option<String>,
    #[serde(default)]
    pub input_register: Option<String>,
    #[serde(default)]
    pub input_length_register: Option<String>,
    /// Address immediately after the target call. A breakpoint here ends a
    /// successful iteration instead of waiting for the timeout.
    #[serde(default)]
    pub stop_pc: Option<u64>,
    /// Treat a guest reset as a kernel bugcheck. This is useful for drivers
    /// such as HEVD whose deliberate faults reset the lab guest.
    #[serde(default)]
    pub reset_is_bugcheck: bool,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub max_input: usize,
    #[serde(default)]
    pub coverage: Vec<CoverageBlock>,
    #[serde(default)]
    pub detectors: Vec<String>,
}

fn default_timeout() -> u64 {
    250
}

impl Harness {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path)?;
        let mut h: Self = serde_json::from_slice(&bytes)?;
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
        if h.ioctl_prefix && h.ioctl_register.is_none() {
            return Err(Error::Invalid("ioctl_prefix needs ioctl_register".into()));
        }
        if h.input_va.is_some() != h.input_register.is_some() {
            return Err(Error::Invalid(
                "input_va and input_register must be paired".into(),
            ));
        }
        if h.stop_pc.is_some_and(|pc| pc & 3 != 0) {
            return Err(Error::Invalid("stop_pc must be instruction aligned".into()));
        }
        if h.coverage.iter().any(|b| b.size == 0 || b.gpa & 3 != 0) {
            return Err(Error::Invalid(
                "coverage blocks must be aligned and non-empty".into(),
            ));
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
            if !gpas.insert(block.gpa) {
                return Err(Error::Invalid(format!(
                    "duplicate coverage GPA {:#x}",
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
    Timeout,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub class: BugClass,
    pub detail: String,
    pub pc: Option<u64>,
    pub input: String,
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
    })
}

#[derive(Debug, Clone)]
pub struct Mutator {
    state: u64,
    max_len: usize,
}

impl Mutator {
    pub fn new(seed: u64, max_len: usize) -> Self {
        Self {
            state: seed | 1,
            max_len,
        }
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
            match self.next() % 5 {
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
                _ if editable != 0 => {
                    let i = prefix_len + (self.next() as usize) % editable;
                    out[i] = out[i].wrapping_add((self.next() & 0xff) as u8);
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
    originals: HashMap<u64, u32>,
    block_by_pc: HashMap<u64, usize>,
    input_buffer: Vec<u8>,
}

impl Engine {
    pub fn open(harness: Harness) -> Result<Self> {
        let input_capacity = harness.input_capacity;
        let config = vroom_config::VmConfig::load(&harness.config)
            .map_err(|e| Error::Config(e.to_string()))?;
        let hv = vroom_hv::default_hypervisor()?;
        let vm = vroom_vmm::Vmm::start(
            config,
            hv,
            vroom_vmm::VmmOptions {
                serial_out: Box::new(std::io::sink()),
                start: vroom_vmm::StartMode::RestorePaused(harness.checkpoint.clone()),
                clipboard: None,
                ui: None,
                state_dir: harness.state_dir.clone(),
            },
        )?;
        Ok(Self {
            vm,
            harness,
            coverage: CoverageMap::default(),
            originals: HashMap::new(),
            block_by_pc: HashMap::new(),
            input_buffer: vec![0; input_capacity],
        })
    }

    /// Patch each declared block with an ARM64 BRK before arming the COW
    /// baseline. The original words are kept for precise single stepping.
    pub fn instrument(&mut self) -> Result<()> {
        for (index, block) in self.harness.coverage.iter().enumerate() {
            let mut bytes = [0u8; 4];
            self.vm.debug_read_physical(block.gpa, &mut bytes)?;
            let original = u32::from_le_bytes(bytes);
            self.originals.insert(block.gpa, original);
            self.block_by_pc.insert(block.gpa, index);
            if let Some(va) = block.va {
                self.block_by_pc.insert(va, index);
            }
            let trap = 0xd420_0000u32 | ((block.id as u32 & 0xffff) << 5);
            self.vm
                .debug_write_physical(&[(block.gpa, &trap.to_le_bytes())])?;
        }
        self.vm.research_arm()?;
        Ok(())
    }

    pub fn run_input(&mut self, input: &[u8]) -> Result<RunResult> {
        if input.len() > self.harness.max_input {
            return Err(Error::Invalid("input exceeds max_input".into()));
        }
        self.vm.research_reset()?;
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
        if let Some(value) = ioctl {
            let register = self
                .harness
                .ioctl_register
                .as_deref()
                .ok_or_else(|| Error::Invalid("ioctl_prefix needs ioctl_register".into()))?;
            self.vm.debug_write_register(
                0,
                vroom_hv::debug::Register::parse(register)?,
                value.into(),
            )?;
        }
        if let Some(value) = self.harness.input_va {
            let register = self
                .harness
                .input_register
                .as_deref()
                .ok_or_else(|| Error::Invalid("input_va needs input_register".into()))?;
            self.vm
                .debug_write_register(0, vroom_hv::debug::Register::parse(register)?, value)?;
        }
        if let Some(register) = self.harness.input_length_register.as_deref() {
            self.vm.debug_write_register(
                0,
                vroom_hv::debug::Register::parse(register)?,
                payload.len() as u64,
            )?;
        }
        self.vm.debug_configure(
            0,
            vroom_hv::debug::DebugConfig {
                enabled: true,
                trap_software_breakpoints: true,
                breakpoints: self.harness.stop_pc.into_iter().collect(),
                ..Default::default()
            },
        )?;
        self.vm.resume()?;
        let start = Instant::now();
        let token = CorpusStore::hash(input);
        let mut new_blocks = 0;
        let mut new_edges = 0;
        let mut finding = None;
        let mut previous_pc = None;
        loop {
            if start.elapsed() > Duration::from_millis(self.harness.timeout_ms) {
                self.vm.pause()?;
                finding = detect("execution timeout", None, &token, true);
                break;
            }
            let Some(event) = self.vm.next_event(Duration::from_millis(5)) else {
                continue;
            };
            match event {
                vroom_vmm::VmEvent::Debug(stop) => {
                    self.vm.pause()?;
                    if self.harness.stop_pc == Some(stop.pc) {
                        break;
                    }
                    let block = self
                        .block_by_pc
                        .get(&stop.pc)
                        .and_then(|index| self.harness.coverage.get(*index));
                    if let Some(block) = block {
                        if let Some(previous) = previous_pc {
                            if self.coverage.observe_edge(previous, block.id) {
                                new_edges += 1;
                            }
                        }
                        previous_pc = Some(block.id);
                        if self.coverage.observe(block, &token) {
                            new_blocks += 1;
                        }
                    }
                    if stop.reason == vroom_hv::debug::DebugReason::SoftwareBreakpoint {
                        if let Some(block) = block {
                            let gpa = block.gpa;
                            if let Some(original) = self.originals.get(&gpa) {
                                self.vm
                                    .debug_write_physical(&[(gpa, &original.to_le_bytes())])?;
                                self.vm.debug_step(
                                    0,
                                    vroom_hv::debug::DebugConfig {
                                        enabled: true,
                                        trap_software_breakpoints: true,
                                        breakpoints: self.harness.stop_pc.into_iter().collect(),
                                        ..Default::default()
                                    },
                                )?;
                                while let Some(next) = self.vm.next_event(Duration::from_millis(20))
                                {
                                    if matches!(next, vroom_vmm::VmEvent::Debug(_)) {
                                        break;
                                    }
                                }
                                self.vm.pause()?;
                                self.vm.debug_configure(
                                    0,
                                    vroom_hv::debug::DebugConfig {
                                        enabled: true,
                                        trap_software_breakpoints: true,
                                        breakpoints: self.harness.stop_pc.into_iter().collect(),
                                        ..Default::default()
                                    },
                                )?;
                                self.vm.debug_write_physical(&[(
                                    gpa,
                                    &(0xd420_0000u32 | ((block.id as u32 & 0xffff) << 5))
                                        .to_le_bytes(),
                                )])?;
                                self.vm.resume()?;
                            }
                        }
                    }
                }
                vroom_vmm::VmEvent::Crashed(detail) => {
                    self.vm.pause()?;
                    finding = detect(&detail, None, &token, false);
                    break;
                }
                vroom_vmm::VmEvent::Shutdown => {
                    self.vm.pause()?;
                    break;
                }
                vroom_vmm::VmEvent::Reset => {
                    self.vm.pause()?;
                    if self.harness.reset_is_bugcheck {
                        finding = detect("guest reset after bugcheck", None, &token, false);
                    }
                    break;
                }
                _ => {}
            }
        }
        self.coverage.finish_execution();
        Ok(RunResult {
            input_hash: token,
            new_blocks,
            new_edges,
            finding,
            elapsed_us: start.elapsed().as_micros() as u64,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub input_hash: String,
    pub new_blocks: usize,
    pub new_edges: usize,
    pub finding: Option<Finding>,
    /// Wall-clock iteration time, including reset, instrumentation and guest
    /// execution. Keeping it beside coverage makes target comparisons
    /// reproducible instead of relying on console timing.
    pub elapsed_us: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn coverage_summary_explains_function_progress() {
        let blocks = vec![
            CoverageBlock {
                id: 1,
                gpa: 0x1000,
                va: Some(0x8000),
                size: 4,
                module: "driver".into(),
                function: "dispatch".into(),
                source: None,
            },
            CoverageBlock {
                id: 2,
                gpa: 0x1004,
                va: Some(0x8004),
                size: 4,
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
}

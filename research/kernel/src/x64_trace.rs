//! AMD64 DR tracing. No guest code patching or EPT execute overlays.
use super::*;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::time::Instant;
use vroom_hv::debug::DebugStop;

const MAX_CAPACITY: u64 = 16384;
const FAULT_CAPACITY: usize = 1024;
type Layout = (Vec<u64>, Vec<Watchpoint>);

#[derive(Clone, Copy, Debug, Default)]
struct Decision {
    owned: bool,
    halt: bool,
    execution: bool,
}
fn batch_continues(decisions: &[Decision], lifecycle: bool, blocked: bool) -> bool {
    !lifecycle && !blocked && decisions.iter().all(|d| d.owned && !d.halt)
}
fn classify_causes(
    state: &State,
    layout: &Layout,
    user_points: &[u64],
    user_watches: &[Watchpoint],
    stop: &DebugStop,
) -> Decision {
    let (points, watches) = layout;
    let mut d = Decision {
        owned: false,
        halt: stop.syndrome & (1 << 14) != 0,
        execution: false,
    };
    // BD (debug register access) and BT are not tracing-owned exceptions.
    d.halt |= stop.syndrome & ((1 << 13) | (1 << 15)) != 0;
    for bit in 0..4 {
        if stop.syndrome & (1 << bit) == 0 {
            continue;
        }
        if let Some(address) = points.get(bit) {
            if *address != stop.pc {
                d.halt = true;
                continue;
            }
            d.halt |= user_points.contains(address);
            // Membership, not armed(), determines ownership of OLD queued hits.
            let traced = state.points.iter().any(|p| p.address == *address)
                || state.fault.as_ref().is_some_and(|f| f.address == *address);
            d.owned |= traced;
            d.execution |= traced;
            d.halt |= !traced;
        } else if let Some(w) = watches.get(bit - points.len()) {
            d.halt |= user_watches.contains(w);
            let traced = state.watches.iter().any(|h| h.slot == *w);
            d.owned |= traced;
            d.halt |= !traced;
        } else {
            d.halt = true;
        }
    }
    d
}

#[derive(Debug, PartialEq, Eq)]
struct DispatchArgs {
    exception_record: u64,
    exception_frame: u64,
    trap_frame: u64,
    user: bool,
    first_chance_address: Option<u64>,
}
fn dispatch_arguments(g: &vroom_kernel::x86_64::General) -> DispatchArgs {
    DispatchArgs {
        exception_record: g.rcx,
        exception_frame: g.rdx,
        trap_frame: g.r8,
        user: g.r9 & 0xff != 0,
        first_chance_address: g.rsp.checked_add(0x28),
    }
}

fn fault_code(bytes: &[u8]) -> Option<(u32, u32)> {
    let raw = u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?);
    // KiDispatchException receives internal KI_EXCEPTION_* codes before NT
    // normalizes them. Filters and access decoding must use the public code.
    Some((raw, super::super::exception_code(raw)))
}

#[derive(Clone)]
struct Capture {
    name: String,
    register: String,
    offset: i64,
    length: usize,
}

#[derive(Clone)]
struct Point {
    address: u64,
    captures: Vec<Capture>,
    condition: Option<(String, u64)>,
    process: Option<(u64, u64)>,
    max_hits: Option<u64>,
    stop: bool,
    hits: u64,
    filtered: u64,
}
impl Point {
    fn armed(&self) -> bool {
        self.max_hits.is_none_or(|n| self.hits < n)
    }
    fn json(&self) -> Value {
        json!({"address":self.address,"kind":"hardware","hits":self.hits,
            "filtered":self.filtered,"armed":self.armed(),"stop":self.stop,
            "pid":self.process.map(|p|p.0),"max_hits":self.max_hits})
    }
}

#[derive(Clone)]
struct Hwatch {
    slot: Watchpoint,
    name: Option<String>,
    on: String,
    process: Option<(u64, u64)>,
    stop: bool,
    hits: u64,
}
impl Hwatch {
    fn json(&self) -> Value {
        json!({"address":self.slot.address,"length":self.slot.size,"name":self.name,
            "on":self.on,"kind":"hardware","stop":self.stop,"hits":self.hits,
            "pid":self.process.map(|p|p.0)})
    }
}

#[derive(Clone)]
struct FaultCatch {
    address: u64,
    mode: String,
    codes: Vec<u32>,
    process: Option<(u64, u64)>,
    stop: bool,
    hits: u64,
    filtered: u64,
}

pub(super) struct State {
    points: Vec<Point>,
    watches: Vec<Hwatch>,
    fault: Option<FaultCatch>,
    records: VecDeque<Value>,
    faults: VecDeque<Value>,
    capacity: usize,
    seq: u64,
    fault_seq: u64,
    dropped: u64,
    fault_dropped: u64,
    started: Instant,
    pub(super) installed: RefCell<BTreeMap<usize, Layout>>,
    deferred: VecDeque<VmEvent>,
    queued_layouts: BTreeMap<usize, VecDeque<Layout>>,
    blocked: bool,
    resume_rf: BTreeSet<usize>,
    preprocessed: BTreeMap<usize, usize>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            points: vec![],
            watches: vec![],
            fault: None,
            records: VecDeque::new(),
            faults: VecDeque::new(),
            capacity: 4096,
            seq: 0,
            fault_seq: 0,
            dropped: 0,
            fault_dropped: 0,
            started: Instant::now(),
            installed: RefCell::new(BTreeMap::new()),
            deferred: VecDeque::new(),
            queued_layouts: BTreeMap::new(),
            blocked: false,
            resume_rf: BTreeSet::new(),
            preprocessed: BTreeMap::new(),
        }
    }
}

fn boolean(q: &Value, key: &str) -> Result<bool, String> {
    q.get(key)
        .filter(|v| !v.is_null())
        .map(|v| {
            v.as_bool()
                .ok_or_else(|| format!("{key}: expected boolean"))
        })
        .unwrap_or(Ok(false))
}
fn hardware(q: &Value) -> Result<(), String> {
    match q.get("kind").filter(|v| !v.is_null()).and_then(Value::as_str) {
        None | Some("hardware") if q.get("kind").is_none_or(|v| v.is_null() || v.is_string()) => Ok(()),
        _ => Err("AMD64 tracing supports kind hardware only; execute-only software/EPT traps and software memory watches are not implemented".into()),
    }
}
fn scalar(r: &Registers, name: &str) -> Result<u64, String> {
    let g = &r.regs;
    Ok(match name.to_ascii_lowercase().as_str() {
        "rax" => g.rax,
        "rbx" => g.rbx,
        "rcx" => g.rcx,
        "rdx" => g.rdx,
        "rsi" => g.rsi,
        "rdi" => g.rdi,
        "rbp" => g.rbp,
        "rsp" | "sp" => g.rsp,
        "r8" => g.r8,
        "r9" => g.r9,
        "r10" => g.r10,
        "r11" => g.r11,
        "r12" => g.r12,
        "r13" => g.r13,
        "r14" => g.r14,
        "r15" => g.r15,
        "rip" | "pc" => g.rip,
        "rflags" => g.rflags,
        "cr3" => r.sregs.cr3,
        _ => return Err(format!("unknown AMD64 capture register {name}")),
    })
}
fn register(v: &Value) -> Result<String, String> {
    let n = v
        .as_str()
        .ok_or("missing AMD64 register")?
        .to_ascii_lowercase();
    // Keep the accepted names in one place, without constructing fake CPU state.
    if !matches!(
        n.as_str(),
        "rax"
            | "rbx"
            | "rcx"
            | "rdx"
            | "rsi"
            | "rdi"
            | "rbp"
            | "rsp"
            | "r8"
            | "r9"
            | "r10"
            | "r11"
            | "r12"
            | "r13"
            | "r14"
            | "r15"
            | "rip"
            | "rflags"
            | "cr3"
            | "pc"
            | "sp"
    ) {
        return Err(format!("unknown AMD64 register {n}"));
    }
    Ok(n)
}
fn capture(q: &Value, default_name: &str) -> Result<Capture, String> {
    let length = q.get("length").map(number).transpose()?.unwrap_or(0);
    if length > 4096 {
        return Err("capture length: 0..4096 bytes".into());
    }
    let offset = match q.get("offset") {
        None | Some(Value::Null) => 0,
        Some(v) => v.as_i64().ok_or("offset: expected signed integer")?,
    };
    Ok(Capture {
        name: q["name"].as_str().unwrap_or(default_name).into(),
        register: register(&q["register"])?,
        offset,
        length: length as usize,
    })
}
fn same_process(p: Option<(u64, u64)>, r: &Registers) -> bool {
    p.is_none_or(|(_, cr3)| cr3 & !0xfff == r.sregs.cr3 & !0xfff)
}
fn execution_cause(points: &[u64], dr6: u64, rip: u64) -> bool {
    points
        .iter()
        .enumerate()
        .any(|(i, p)| dr6 & (1 << i) != 0 && *p == rip)
}
fn push_bounded(ring: &mut VecDeque<Value>, record: Value, capacity: usize, dropped: &mut u64) {
    if ring.len() == capacity {
        ring.pop_front();
        *dropped += 1;
    }
    ring.push_back(record);
}

/// Merge in the exact DR slot order used by KVM. x64 has four TOTAL slots.
fn slots(
    points: &[u64],
    watches: &[Watchpoint],
    state: &State,
) -> Result<(Vec<u64>, Vec<Watchpoint>), String> {
    allocate_slots(
        points,
        watches,
        &state.points,
        &state.watches,
        state.fault.as_ref(),
    )
}
fn allocate_slots(
    points: &[u64],
    watches: &[Watchpoint],
    traces: &[Point],
    hwatches: &[Hwatch],
    fault: Option<&FaultCatch>,
) -> Result<Layout, String> {
    let mut p = points.to_vec();
    p.extend(traces.iter().filter(|t| t.armed()).map(|t| t.address));
    p.extend(fault.iter().map(|f| f.address));
    p.sort_unstable();
    p.dedup();
    let mut w = watches.to_vec();
    for h in hwatches {
        if !w.contains(&h.slot) {
            w.push(h.slot.clone());
        }
    }
    if p.len() + w.len() > 4 {
        return Err(format!(
            "AMD64 execution and data traps share 4 DR slots; requested {} execution + {} data",
            p.len(),
            w.len()
        ));
    }
    Ok((p, w))
}

impl Session {
    fn trace_validate(
        &self,
        traces: &[Point],
        hwatches: &[Hwatch],
        fault: Option<&FaultCatch>,
    ) -> Result<(), String> {
        let mut points = self.breakpoints.clone();
        if let Some(run) = &self.run_to {
            points.push(run.address);
        }
        allocate_slots(&points, &self.watches, traces, hwatches, fault)?;
        Ok(())
    }
    pub(super) fn trace_next_event(&mut self) -> Option<VmEvent> {
        self.tracing
            .deferred
            .pop_front()
            .or_else(|| self.vm.next_event(Duration::ZERO))
    }
    pub(super) fn trace_execution_owned(&self, address: u64) -> bool {
        self.tracing.points.iter().any(|p| p.address == address)
            || self
                .tracing
                .fault
                .as_ref()
                .is_some_and(|f| f.address == address)
    }
    pub(super) fn trace_pause_requested(&mut self) {
        self.tracing.blocked = true;
    }
    pub(super) fn trace_resume_requested(&mut self) -> Result<(), String> {
        self.trace_drain_events();
        if !self.tracing.deferred.is_empty() {
            return Err("pending VM events preserved; process them before resuming".into());
        }
        for &cpu in &self.tracing.resume_rf {
            let r = self.registers(cpu, false)?;
            let active = self
                .tracing
                .installed
                .borrow()
                .get(&cpu)
                .is_some_and(|(p, _)| p.contains(&r.regs.rip));
            if active {
                self.vm
                    .debug_write_register(
                        cpu,
                        Register::X86(X86Register::Rflags),
                        r.regs.rflags | (1 << 16),
                    )
                    .map_err(string)?;
            }
        }
        self.tracing.resume_rf.clear();
        self.tracing.blocked = false;
        Ok(())
    }
    fn trace_drain_events(&mut self) {
        while let Some(event) = self.vm.next_event(Duration::ZERO) {
            if let VmEvent::Debug(stop) = &event {
                if let Some(layout) = self
                    .tracing
                    .installed
                    .borrow()
                    .get(&(stop.cpu as usize))
                    .cloned()
                {
                    self.tracing
                        .queued_layouts
                        .entry(stop.cpu as usize)
                        .or_default()
                        .push_back(layout);
                }
            }
            self.tracing.deferred.push_back(event);
        }
    }
    /// Call only after global pause, before changing any CPU's DR layout.
    pub(super) fn trace_preserve_pending(&mut self) {
        self.trace_drain_events();
    }
    pub(super) fn trace_slots(
        &self,
        points: &[u64],
        watches: &[Watchpoint],
    ) -> Result<(Vec<u64>, Vec<Watchpoint>), String> {
        slots(points, watches, &self.tracing)
    }
    fn trace_process(
        &mut self,
        q: &Value,
        address: u64,
        cpu: usize,
    ) -> Result<Option<(u64, u64)>, String> {
        match q.get("pid").filter(|v| !v.is_null()) {
            Some(v) if v.as_str() == Some("any") => Ok(None),
            Some(v) => {
                let pid = number(v)?;
                let p = self
                    .processes(cpu)?
                    .into_iter()
                    .find(|p| p.pid == pid)
                    .ok_or("pid not found")?;
                Ok(Some((p.pid, p.directory_table)))
            }
            None => Ok(self
                .selected
                .as_ref()
                .filter(|_| address >> 63 == 0)
                .map(|p| (p.pid, p.directory_table))),
        }
    }
    fn trace_status(&self) -> Value {
        json!({"tracepoints":self.tracing.points.iter().map(Point::json).collect::<Vec<_>>(),
            "hwatch":self.tracing.watches.iter().map(Hwatch::json).collect::<Vec<_>>(),
            "records":self.tracing.records.len(),"recorded":self.tracing.seq,
            "dropped":self.tracing.dropped,"capacity":self.tracing.capacity,
            "limitations":"hardware only; four shared DR slots; pid filters compare kernel CR3 (KPTI user CR3 aliases are not matched); no guest clock counter"})
    }
    fn fault_status(&self) -> Value {
        json!({"catching":self.tracing.fault.as_ref().map(|f|json!({"address":f.address,"kind":"hardware",
            "mode":f.mode,"codes":f.codes,"stop":f.stop,"pid":f.process.map(|p|p.0),"hits":f.hits,"filtered":f.filtered})),
            "recorded":self.tracing.fault_seq,"dropped":self.tracing.fault_dropped,"capacity":FAULT_CAPACITY,
            "last":self.tracing.faults.back()})
    }
    pub(super) fn trace_command(&mut self, q: &Value, cpu: usize) -> Result<Value, String> {
        let op = q["op"].as_str().unwrap_or_default();
        if op == "traces" || (op == "faults" && q.get("enable").is_none_or(Value::is_null)) {
            if q.get("memory").is_some_and(|v| !v.is_null()) {
                return Err("AMD64 debug.traces memory rereads are not implemented; use captures at hit time".into());
            }
            let limit = q
                .get("limit")
                .filter(|v| !v.is_null())
                .map(number)
                .transpose()?
                .unwrap_or(u64::MAX);
            let clear = boolean(q, "clear")?;
            if clear && limit == 0 {
                return Err("limit 0 with clear would drain nothing".into());
            }
            let mut result = if op == "traces" {
                self.trace_status()
            } else {
                self.fault_status()
            };
            let ring = if op == "traces" {
                &mut self.tracing.records
            } else {
                &mut self.tracing.faults
            };
            let n = limit.min(ring.len() as u64) as usize;
            let records: Vec<Value> = if clear {
                ring.drain(..n).collect()
            } else {
                ring.iter().take(n).cloned().collect()
            };
            result["records"] = json!(records);
            result["remaining"] = json!(ring.len());
            return Ok(result);
        }
        if op == "tracepoints" && q.get("tracepoints").is_none_or(Value::is_null) {
            return Ok(self.trace_status());
        }
        if self.vm.state() != RunState::Paused {
            return Err("pause every CPU before changing tracing".into());
        }
        self.trace_drain_events();
        if !self.tracing.deferred.is_empty() {
            return Err(
                "pending VM events preserved; process them before replacing tracing".into(),
            );
        }
        let capacity = q
            .get("capacity")
            .filter(|v| !v.is_null())
            .map(number)
            .transpose()?;
        if capacity.is_some_and(|n| !(1..=MAX_CAPACITY).contains(&n)) {
            return Err(format!("capacity: 1..{MAX_CAPACITY}"));
        }
        match op {
            "tracepoints" => {
                let list = q["tracepoints"]
                    .as_array()
                    .ok_or("expected tracepoints array")?;
                if list.len() > 4 {
                    return Err("at most four hardware tracepoints".into());
                }
                let mut points = Vec::new();
                for t in list {
                    hardware(t)?;
                    if t.get("actions")
                        .is_some_and(|v| !v.is_null() && v.as_array().is_none_or(|a| !a.is_empty()))
                    {
                        return Err("AMD64 tracepoint actions are not implemented".into());
                    }
                    let address = number(&t["address"])?;
                    if !canonical(address) {
                        return Err("noncanonical tracepoint address".into());
                    }
                    if points.iter().any(|p: &Point| p.address == address)
                        || self
                            .tracing
                            .fault
                            .as_ref()
                            .is_some_and(|f| f.address == address)
                    {
                        return Err("duplicate tracepoint or fault-catch address".into());
                    }
                    let mut captures = Vec::new();
                    if t.get("register").is_some_and(|v| !v.is_null()) {
                        captures.push(capture(t, "pointer")?);
                    }
                    if let Some(c) = t.get("captures").filter(|v| !v.is_null()) {
                        let a = c.as_array().ok_or("captures: expected array")?;
                        if a.len() > 16 {
                            return Err("at most 16 captures".into());
                        }
                        for c in a {
                            captures.push(capture(c, "capture")?);
                        }
                    }
                    if captures.iter().map(|c| c.length).sum::<usize>() > 4096 {
                        return Err("total captured memory is limited to 4096 bytes per hit".into());
                    }
                    let condition = t
                        .get("record_if")
                        .filter(|v| !v.is_null())
                        .map(|v| {
                            Ok::<_, String>((register(&v["register"])?, number(&v["equals"])?))
                        })
                        .transpose()?;
                    let max_hits = t
                        .get("max_hits")
                        .filter(|v| !v.is_null())
                        .map(number)
                        .transpose()?;
                    if max_hits == Some(0) {
                        return Err("max_hits must be positive".into());
                    }
                    points.push(Point {
                        address,
                        captures,
                        condition,
                        max_hits,
                        process: self.trace_process(t, address, cpu)?,
                        stop: boolean(t, "stop")?,
                        hits: 0,
                        filtered: 0,
                    });
                }
                self.trace_validate(&points, &self.tracing.watches, self.tracing.fault.as_ref())?;
                let old = std::mem::replace(&mut self.tracing.points, points);
                if let Err(e) = self.debug(&self.breakpoints, &self.watches) {
                    self.tracing.points = old;
                    let rollback = self.debug(&self.breakpoints, &self.watches);
                    return Err(format!("{e}; rollback: {rollback:?}; VM remains paused"));
                }
            }
            "hwatch" => {
                let list = q["watches"].as_array().ok_or("expected watches array")?;
                if list.len() > 4 {
                    return Err("at most four hardware hwatches".into());
                }
                let mut watches = Vec::new();
                for w in list {
                    hardware(w)?;
                    let address = number(&w["address"])?;
                    let length = number(&w["length"])?;
                    if !canonical(address)
                        || !matches!(length, 1 | 2 | 4 | 8)
                        || address % length != 0
                    {
                        return Err("AMD64 hardware watches require canonical, naturally aligned 1/2/4/8-byte ranges".into());
                    }
                    let on = w["on"].as_str().unwrap_or("write");
                    if !matches!(on, "write" | "access" | "free") {
                        return Err("AMD64 supports on write, access, or free (poison heuristic); read-only traps are unavailable".into());
                    }
                    if on == "free" && length != 8 {
                        return Err("free poison heuristic requires length 8".into());
                    }
                    watches.push(Hwatch {
                        slot: Watchpoint {
                            address,
                            size: length as u8,
                            read: on == "access",
                            write: true,
                        },
                        name: w["name"].as_str().map(str::to_owned),
                        on: on.into(),
                        process: self.trace_process(w, address, cpu)?,
                        stop: boolean(w, "stop")?,
                        hits: 0,
                    });
                }
                self.trace_validate(&self.tracing.points, &watches, self.tracing.fault.as_ref())?;
                let old = std::mem::replace(&mut self.tracing.watches, watches);
                if let Err(e) = self.debug(&self.breakpoints, &self.watches) {
                    self.tracing.watches = old;
                    let rollback = self.debug(&self.breakpoints, &self.watches);
                    return Err(format!("{e}; rollback: {rollback:?}; VM remains paused"));
                }
            }
            "faults" => {
                let fault = if boolean(q, "enable")? {
                    hardware(q)?;
                    let mode = q["mode"].as_str().unwrap_or("any");
                    if !matches!(mode, "any" | "user" | "kernel") {
                        return Err("mode: user, kernel, or any".into());
                    }
                    let codes = q
                        .get("codes")
                        .filter(|v| !v.is_null())
                        .map(|v| v.as_array().ok_or("codes: expected array"))
                        .transpose()?
                        .map(|a| {
                            a.iter()
                                .map(|v| number(v).and_then(|n| u32::try_from(n).map_err(string)))
                                .collect::<Result<Vec<_>, _>>()
                        })
                        .transpose()?
                        .unwrap_or_default();
                    self.find_kernel(cpu)?;
                    if self.kernel_symbols().is_err() {
                        self.command(&json!({"op":"symbols","cpu":cpu}))?;
                    }
                    let symbols = self.kernel_symbols()?;
                    let address = symbols.resolve("KiDispatchException").map_err(string)?;
                    for field in ["Rip", "Rsp", "EFlags"] {
                        symbols.field("_KTRAP_FRAME", field).map_err(string)?;
                    }
                    if self.tracing.points.iter().any(|t| t.address == address) {
                        return Err("remove the tracepoint at KiDispatchException first".into());
                    }
                    Some(FaultCatch {
                        address,
                        mode: mode.into(),
                        codes,
                        process: self.trace_process(q, address, cpu)?,
                        stop: boolean(q, "stop")?,
                        hits: 0,
                        filtered: 0,
                    })
                } else {
                    None
                };
                self.trace_validate(&self.tracing.points, &self.tracing.watches, fault.as_ref())?;
                let old = std::mem::replace(&mut self.tracing.fault, fault);
                if let Err(e) = self.debug(&self.breakpoints, &self.watches) {
                    self.tracing.fault = old;
                    let rollback = self.debug(&self.breakpoints, &self.watches);
                    return Err(format!("{e}; rollback: {rollback:?}; VM remains paused"));
                }
                return Ok(self.fault_status());
            }
            _ => return Err("unknown tracing operation".into()),
        }
        if let Some(c) = capacity {
            self.tracing.capacity = c as usize;
        }
        while self.tracing.records.len() > self.tracing.capacity {
            self.tracing.records.pop_front();
            self.tracing.dropped += 1;
        }
        Ok(self.trace_status())
    }

    fn trace_read(&self, r: &Registers, address: u64, length: usize) -> Result<Vec<u8>, String> {
        let mut data = vec![0; length];
        AddressSpace {
            memory: &self.vm,
            registers: r,
        }
        .read(address, &mut data)
        .map_err(string)?;
        Ok(data)
    }
    fn trace_capture(&self, r: &Registers, c: &Capture) -> Value {
        let mut result = json!({"name":c.name,"register":c.register,"length":c.length});
        let read = (|| {
            let value = scalar(r, &c.register)?;
            result["value"] = json!(value);
            let address = value
                .checked_add_signed(c.offset)
                .ok_or("capture address overflow")?;
            result["address"] = json!(address);
            if c.length != 0 {
                result["hex"] = json!(hex(&self.trace_read(r, address, c.length)?));
            }
            Ok::<_, String>(())
        })();
        if let Err(e) = read {
            result["error"] = json!(e);
        }
        result
    }
    fn push_trace(&mut self, mut record: Value) {
        self.tracing.seq += 1;
        record["seq"] = json!(self.tracing.seq);
        record["time_ns"] = json!(self
            .tracing
            .started
            .elapsed()
            .as_nanos()
            .min(u64::MAX as u128) as u64);
        push_bounded(
            &mut self.tracing.records,
            record,
            self.tracing.capacity,
            &mut self.tracing.dropped,
        );
    }

    /// Called after the VMM's global pause acknowledgement, before normal stop handling.
    /// True means this stop was handled and execution has been safely continued.
    pub(super) fn trace_debug_stop(&mut self, stop: &DebugStop) -> Result<bool, String> {
        let cpu = stop.cpu as usize;
        if let Some(n) = self.tracing.preprocessed.get_mut(&cpu) {
            if *n != 0 {
                *n -= 1;
                return Ok(false);
            }
        }
        let layout = self.trace_stop_layout(cpu);
        self.trace_drain_events();
        // Hardware execution breakpoints are faults: RF alone crosses the
        // instruction while retaining every data trap. Never inject an internal
        // single-step into Windows just to rearm an unchanged DR breakpoint.
        let mut decisions = vec![self.trace_record_stop(stop, layout)?];
        // pause() acknowledged every CPU before this drain. Every pending stop
        // is decoded NOW, against its saved layout, before any reconfiguration.
        let events = std::mem::take(&mut self.tracing.deferred);
        let mut lifecycle = false;
        for event in &events {
            if let VmEvent::Debug(sibling) = event {
                let layout = self.trace_stop_layout(sibling.cpu as usize);
                let d = self.trace_record_stop(sibling, layout)?;
                decisions.push(d);
            } else {
                lifecycle = true;
            }
        }
        if !batch_continues(
            &decisions,
            lifecycle,
            self.tracing.blocked || self.run_to.is_some(),
        ) {
            // Records were already captured. Replay stops for ordinary reporting,
            // but never count/capture them twice or lose lifecycle event ordering.
            for event in &events {
                if let VmEvent::Debug(s) = event {
                    *self.tracing.preprocessed.entry(s.cpu as usize).or_default() += 1;
                }
            }
            self.tracing.deferred = events;
            self.tracing.blocked = true;
            return Ok(false);
        }
        // Only owned non-stopping debug events were consumed. Apply RF to all
        // execution-stop CPUs before releasing them together with the devices.
        self.debug(&self.breakpoints, &self.watches)?;
        self.trace_resume_requested()?;
        self.vm.resume().map_err(string)?;
        Ok(true)
    }

    fn trace_stop_layout(&mut self, cpu: usize) -> Layout {
        self.tracing
            .queued_layouts
            .get_mut(&cpu)
            .and_then(VecDeque::pop_front)
            .or_else(|| self.tracing.installed.borrow().get(&cpu).cloned())
            .unwrap_or_default()
    }

    fn trace_record_stop(&mut self, stop: &DebugStop, layout: Layout) -> Result<Decision, String> {
        let cpu = stop.cpu as usize;
        if !matches!(
            stop.reason,
            DebugReason::Breakpoint | DebugReason::Watchpoint | DebugReason::SingleStep
        ) {
            return Ok(Decision::default());
        }
        let r = self.registers(cpu, false)?;
        let decision = classify_causes(
            &self.tracing,
            &layout,
            &self.breakpoints,
            &self.watches,
            stop,
        );
        let (points, watches) = layout;
        let mut owned = decision.owned;
        let mut halt = decision.halt;
        let mut execution = decision.execution;
        // DR6 bits identify slots, not addresses; RIP on a data trap is AFTER the access.
        for (i, w) in watches.iter().enumerate() {
            if stop.syndrome & (1 << (points.len() + i)) == 0 {
                continue;
            }
            if self.watches.contains(w) {
                halt = true;
            }
            let matching: Vec<usize> = self
                .tracing
                .watches
                .iter()
                .enumerate()
                .filter(|(_, h)| h.slot == *w)
                .map(|(i, _)| i)
                .collect();
            for index in matching {
                owned = true;
                let h = self.tracing.watches[index].clone();
                if !same_process(h.process, &r) {
                    continue;
                }
                let after = self.trace_read(&r, h.slot.address, h.slot.size as usize);
                if h.on == "free"
                    && after.as_ref().is_ok_and(|b| {
                        b.as_slice() != [0xfa, 0xad, 0xdb, 0xba, 0xfa, 0xad, 0xdb, 0xba]
                    })
                {
                    continue;
                }
                self.tracing.watches[index].hits += 1;
                let mut record = json!({"kind":"hwatch","hwatch":index,"name":h.name,"hit":self.tracing.watches[index].hits,
                    "cpu":cpu,"pc":r.regs.rip,"registers":r,"dr6":stop.syndrome,"address":h.slot.address,"length":h.slot.size,"on":h.on,
                    "access":if h.slot.read {"read_or_write"} else {"write"},
                    "note":"x64 data #DB is post-instruction; before bytes and exact access PC/direction are unavailable"});
                match after {
                    Ok(b) => record["hex"] = json!(hex(&b)),
                    Err(e) => record["read_error"] = json!(e),
                }
                self.push_trace(record);
                halt |= h.stop;
            }
        }
        // Execution traps are faults (pre-instruction); do not mistake BS-only exits for hits.
        let exec_cause = execution_cause(&points, stop.syndrome, stop.pc);
        if exec_cause {
            self.tracing.resume_rf.insert(cpu);
            if self.breakpoints.contains(&stop.pc) {
                halt = true;
            }
            if let Some(index) = self
                .tracing
                .points
                .iter()
                .position(|p| p.address == stop.pc)
            {
                owned = true;
                execution = true;
                let t = self.tracing.points[index].clone();
                if t.armed()
                    && same_process(t.process, &r)
                    && t.condition
                        .as_ref()
                        .is_none_or(|(n, v)| scalar(&r, n).ok() == Some(*v))
                {
                    self.tracing.points[index].hits += 1;
                    let captures: Vec<Value> = t
                        .captures
                        .iter()
                        .map(|c| self.trace_capture(&r, c))
                        .collect();
                    let mut record = json!({"kind":"tracepoint","tracepoint":index,"hit":self.tracing.points[index].hits,
                        "cpu":cpu,"pc":stop.pc,"registers":r,"dr6":stop.syndrome,"captures":captures});
                    if let Some(c) = captures.iter().find(|c| c["name"] == "pointer") {
                        record["pointer"] = c["address"].clone();
                        record["hex"] = c["hex"].clone();
                    }
                    self.push_trace(record);
                    halt |= t.stop;
                } else if t.armed() {
                    self.tracing.points[index].filtered += 1;
                }
            }
            if self
                .tracing
                .fault
                .as_ref()
                .is_some_and(|f| f.address == stop.pc)
            {
                owned = true;
                execution = true;
                let seq = self.tracing.fault_seq;
                halt |= self.trace_fault(&r)?;
                if self.tracing.fault_seq != seq {
                    if let Some(record) = self.tracing.faults.back_mut() {
                        record["dr6"] = json!(stop.syndrome);
                    }
                }
            }
        }
        Ok(Decision {
            owned,
            halt,
            execution,
        })
    }

    fn trace_fault(&mut self, r: &Registers) -> Result<bool, String> {
        let f = self.tracing.fault.as_ref().expect("armed").clone();
        let args = dispatch_arguments(&r.regs);
        let user = args.user;
        let raw = self.trace_read(r, args.exception_record, 0x98);
        let codes = raw.as_ref().ok().and_then(|b| fault_code(b));
        let code = codes.map(|(_, normalized)| normalized);
        if !same_process(f.process, r)
            || (f.mode == "user" && !user)
            || (f.mode == "kernel" && user)
            || (!f.codes.is_empty() && code.is_some_and(|c| !f.codes.contains(&c)))
        {
            self.tracing.fault.as_mut().unwrap().filtered += 1;
            return Ok(false);
        }
        let mut errors = Vec::new();
        let mut record = json!({"kind":"fault","cpu":r.cpu,"user":user,"mode":if user {"user"} else {"kernel"},
            "dispatch_registers":r,"exception_record":args.exception_record,"exception_frame":args.exception_frame,"trap_frame":args.trap_frame,
            "note":"captured at x64 KiDispatchException entry, not a CONTEXT argument; trap fields are symbol-derived; no stack unwinding here"});
        match raw {
            Ok(b) => {
                let qword = |o| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
                let count = u32::from_le_bytes(b[0x18..0x1c].try_into().unwrap()).min(15) as usize;
                let info: Vec<u64> = (0..count).map(|i| qword(0x20 + i * 8)).collect();
                record["code"] = json!(code);
                record["raw_code"] = json!(codes.map(|(raw, _)| raw));
                record["name"] = json!(code.and_then(super::super::exception_name));
                record["flags"] = json!(u32::from_le_bytes(b[4..8].try_into().unwrap()));
                record["nested_record"] = json!(qword(8));
                record["address"] = json!(qword(0x10));
                record["information"] = json!(info);
                if matches!(code, Some(0xc0000005 | 0xc0000006)) && info.len() >= 2 {
                    record["access"] = json!(match info[0] {
                        0 => "read",
                        1 => "write",
                        8 => "execute",
                        _ => "unknown",
                    });
                    record["fault_address"] = json!(info[1]);
                }
            }
            Err(e) => errors.push(format!("exception record: {e}")),
        }
        match args
            .first_chance_address
            .ok_or("RSP overflow".into())
            .and_then(|a| self.trace_read(r, a, 1))
        {
            Ok(b) => record["first_chance"] = json!(b[0] != 0),
            Err(e) => errors.push(format!("fifth argument: {e}")),
        }
        // At the kernel dispatch entry CR3 is the process's kernel table,
        // unlike a KPTI-isolated user-mode tracepoint.
        match self.processes(r.cpu as usize) {
            Ok(all) => {
                record["process"] = json!(all
                    .into_iter()
                    .find(|p| p.directory_table & !0xfff == r.sregs.cr3 & !0xfff)
                    .map(|p| json!({"pid":p.pid,"name":p.name,"eprocess":p.eprocess})));
            }
            Err(e) => errors.push(format!("process identification: {e}")),
        }
        let symbols = self.kernel_symbols()?;
        let mut trap = serde_json::Map::new();
        // Volatile GPRs plus RIP/RSP live in KTRAP_FRAME. Nonvolatiles are
        // split between KTRAP_FRAME and KEXCEPTION_FRAME: never invent a full CONTEXT.
        for (ty, base, fields) in [
            (
                "_KTRAP_FRAME",
                r.regs.r8,
                &[
                    "Rax", "Rcx", "Rdx", "R8", "R9", "R10", "R11", "Rbp", "Rip", "Rsp", "EFlags",
                    "SegCs", "SegSs",
                ][..],
            ),
            (
                "_KEXCEPTION_FRAME",
                r.regs.rdx,
                &["Rbx", "Rdi", "Rsi", "R12", "R13", "R14", "R15"][..],
            ),
        ] {
            for &field in fields {
                let width = if field == "EFlags" {
                    4
                } else if field.starts_with("Seg") {
                    2
                } else {
                    8
                };
                let value = symbols
                    .field(ty, field)
                    .map_err(string)
                    .and_then(|off| base.checked_add(off).ok_or("frame address overflow".into()))
                    .and_then(|a| self.trace_read(r, a, width));
                match value {
                    Ok(b) => {
                        let mut raw = [0; 8];
                        raw[..width].copy_from_slice(&b);
                        trap.insert(field.to_ascii_lowercase(), json!(u64::from_le_bytes(raw)));
                    }
                    Err(e) => errors.push(format!("{ty}.{field}: {e}")),
                }
            }
        }
        record["pc"] = trap.get("rip").cloned().unwrap_or(Value::Null);
        record["registers"] = Value::Object(trap);
        record["errors"] = json!(errors);
        self.tracing.fault.as_mut().unwrap().hits += 1;
        self.tracing.fault_seq += 1;
        record["seq"] = json!(self.tracing.fault_seq);
        record["time_ns"] = json!(self
            .tracing
            .started
            .elapsed()
            .as_nanos()
            .min(u64::MAX as u128) as u64);
        push_bounded(
            &mut self.tracing.faults,
            record,
            FAULT_CAPACITY,
            &mut self.tracing.fault_dropped,
        );
        Ok(f.stop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn internal_dispatch_codes_are_normalized_before_filtering() {
        for (raw, public) in [
            (0x10000004u32, 0xc0000005u32),
            (0x10000002, 0xc000001d),
            (0x10000003, 0xc0000094),
            (0x10000001, 0xc0000096),
            (0x80000003, 0x80000003),
        ] {
            let decoded = fault_code(&raw.to_le_bytes()).unwrap();
            assert_eq!(decoded, (raw, public));
            assert!([public].contains(&decoded.1));
        }
        assert_eq!(fault_code(&[0; 3]), None);
    }
    fn traced_point(address: u64) -> Point {
        Point {
            address,
            captures: vec![],
            condition: None,
            process: None,
            max_hits: Some(1),
            stop: false,
            hits: 0,
            filtered: 0,
        }
    }
    fn synthetic_stop(cpu: u32, dr6: u64, pc: u64) -> DebugStop {
        DebugStop {
            cpu,
            pc,
            syndrome: dr6,
            reason: DebugReason::Breakpoint,
            fault_address: None,
        }
    }
    #[test]
    fn smp_trace_only_batch_continues_including_old_exhausted_hit() {
        let mut state = State::default();
        state.points.push(traced_point(0x1000));
        let layout = (vec![0x1000], vec![]);
        let first = classify_causes(&state, &layout, &[], &[], &synthetic_stop(0, 1, 0x1000));
        state.points[0].hits = 1; // CPU 0 exhausted max_hits; CPU 1 already queued.
        assert!(!state.points[0].armed());
        let sibling = classify_causes(&state, &layout, &[], &[], &synthetic_stop(1, 1, 0x1000));
        assert!(sibling.owned && sibling.execution && !sibling.halt);
        assert!(batch_continues(&[first, sibling], false, false));
        assert_eq!(state.points[0].hits, 1);
        assert!(!batch_continues(&[first, sibling], true, false));
        assert!(!batch_continues(&[first, sibling], false, true));
    }
    #[test]
    fn mixed_user_watch_or_unknown_dr_cause_never_auto_continues() {
        let mut state = State::default();
        state.points.push(traced_point(0x1000));
        let w = Watchpoint {
            address: 0x2000,
            size: 8,
            read: false,
            write: true,
        };
        state.watches.push(Hwatch {
            slot: w.clone(),
            name: None,
            on: "write".into(),
            process: None,
            stop: false,
            hits: 0,
        });
        let layout = (vec![0x1000], vec![w.clone()]);
        let trace = classify_causes(&state, &layout, &[], &[], &synthetic_stop(0, 1, 0x1000));
        let watch = classify_causes(&state, &layout, &[], &[w], &synthetic_stop(1, 2, 0x1234));
        assert!(watch.owned && watch.halt && !watch.execution);
        assert!(!batch_continues(&[trace, watch], false, false));
        let unknown = classify_causes(&state, &layout, &[], &[], &synthetic_stop(1, 8, 0x1000));
        assert!(unknown.halt);
        assert!(!batch_continues(&[trace, unknown], false, false));
    }
    #[test]
    fn single_step_causes_never_auto_continue_even_in_trace_only_batch() {
        let mut state = State::default();
        state.points.push(traced_point(0x1000));
        let layout = (vec![0x1000], vec![]);
        let stop = synthetic_stop(0, (1 << 14) | 1, 0x1000);
        let explicit_step = classify_causes(&state, &layout, &[], &[], &stop);
        assert!(!batch_continues(&[explicit_step], false, false));
        let user_break = classify_causes(&state, &layout, &[0x1000], &[], &stop);
        assert!(!batch_continues(&[user_break], false, false));
    }
    #[test]
    fn four_slots_are_shared_and_execution_addresses_deduplicated() {
        let mut state = State::default();
        let w = Watchpoint {
            address: 0x1000,
            size: 8,
            read: false,
            write: true,
        };
        state.watches.push(Hwatch {
            slot: w.clone(),
            name: None,
            on: "write".into(),
            process: None,
            stop: false,
            hits: 0,
        });
        assert_eq!(
            slots(&[1, 2, 3, 3], &[w.clone()], &state).unwrap().0.len(),
            3
        );
        assert!(slots(&[1, 2, 3, 4], &[], &state).is_err());
        assert_eq!(slots(&[], &[w], &state).unwrap().1.len(), 1);
    }
    #[test]
    fn arm_only_features_are_rejected() {
        assert!(hardware(&json!({"kind":"software"})).is_err());
        assert!(hardware(&json!({"kind":42})).is_err());
        assert!(hardware(&json!({})).is_ok());
        assert!(capture(&json!({"register":"x0"}), "x").is_err());
        assert!(capture(&json!({"register":"rax","length":4097}), "x").is_err());
        assert_eq!(
            capture(&json!({"register":"rcx","offset":-8,"length":8}), "x")
                .unwrap()
                .offset,
            -8
        );
    }
    #[test]
    fn poison_and_abi_layout() {
        let mut g = vroom_kernel::x86_64::General {
            rcx: 0x1110,
            rdx: 0x2220,
            r8: 0x3330,
            r9: 1,
            rsp: 0x8000,
            ..Default::default()
        };
        assert_eq!(
            dispatch_arguments(&g),
            DispatchArgs {
                exception_record: 0x1110,
                exception_frame: 0x2220,
                trap_frame: 0x3330,
                user: true,
                first_chance_address: Some(0x8028)
            }
        );
        g.r9 = 0;
        g.rsp = u64::MAX;
        assert_eq!(dispatch_arguments(&g).first_chance_address, None);
        assert_eq!(
            u64::from_le_bytes([0xfa, 0xad, 0xdb, 0xba, 0xfa, 0xad, 0xdb, 0xba]),
            0xbadbadfabadbadfa
        );
    }
    #[test]
    fn simultaneous_bs_and_execution_causes_are_not_collapsed() {
        let p = [0x1000, 0x2000];
        assert!(execution_cause(&p, (1 << 14) | 2, 0x2000));
        assert!(!execution_cause(&p, (1 << 14) | 2, 0x1000));
        assert!(!execution_cause(&p, 1 << 14, 0x2000));
    }
    #[test]
    fn rings_drop_oldest_and_count_overflow() {
        let mut ring = VecDeque::new();
        let mut dropped = 0;
        for n in 0..5 {
            push_bounded(&mut ring, json!(n), 2, &mut dropped);
        }
        assert_eq!(ring, VecDeque::from([json!(3), json!(4)]));
        assert_eq!(dropped, 3);
    }
}

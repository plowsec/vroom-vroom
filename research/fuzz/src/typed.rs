//! Typed mutation of sequence programs (syzkaller-like), for harnesses
//! with call descriptions: arguments generated and changed by type,
//! resource arguments naming slots earlier calls filled, inserted calls
//! preceded by the producers they need, splices that keep resources
//! consistent, and which call to insert learned from the corpus (a
//! choice table). A minority of mutations stay byte-level (parsers).
//! A call that failed when its program last ran fills no slot: what
//! follows it uses (mostly) slots that hold something, and calls that
//! failed on such a dead slot are pointed at live ones (most of the time),
//! a new producer first when there is none.

use crate::calls::{Call, Calls, Dir, Slots, Type, Value};
use crate::Mutator;
use std::collections::BTreeSet;

/// Share of mutations (percent) done on bytes instead of by type.
pub const BYTE_SHARE: u64 = 15;
/// Share of mutations (percent) of a program with failed calls that
/// repair them first (`repair`).
const REPAIR_SHARE: u64 = 90;
/// Producers a repair inserts at most.
const REPAIR_INSERTS: usize = 4;
/// Calls a program has before inserting gives way to removing.
const LONG: usize = 48;
/// Calls apart that still count as used together (choice table).
const WINDOW: usize = 8;
/// Producers prepended for a call at most deep (a read: an append, an
/// area, a log).
const DEPTH: usize = 3;

/// Which call to insert after a given one: syzkaller's static (shared
/// resources) times dynamic (calls used together in the corpus)
/// priorities, each normalized per row to 10..=1000 (1 when none).
#[derive(Debug, Clone)]
pub struct ChoiceTable {
    /// Per call: cumulative weights over the calls.
    runs: Vec<Vec<u64>>,
}

fn normalize(row: &mut [u64]) {
    let max = row.iter().copied().max().unwrap_or(0).max(1);
    for p in row {
        *p = if *p == 0 { 1 } else { (*p * 1000 / max).max(10) };
    }
}

impl ChoiceTable {
    pub fn new(calls: &Calls, corpus: &[Vec<u8>]) -> Self {
        let n = calls.descs.len();
        let mut fills = vec![BTreeSet::new(); n];
        let mut uses = vec![BTreeSet::new(); n];
        for (i, d) in calls.descs.iter().enumerate() {
            fills[i].extend(d.outputs.iter().map(|o| o.kind.as_str()));
            for arg in &d.args {
                let ty = match &arg.ty {
                    Type::Array { elem, .. } => &**elem,
                    ty => ty,
                };
                if let Type::Res { kind, dir, .. } = ty {
                    if *dir == Dir::Out { &mut fills[i] } else { &mut uses[i] }.insert(kind.as_str());
                }
            }
        }
        let mut stat = vec![vec![0u64; n]; n];
        for i in 0..n {
            for j in 0..n {
                stat[i][j] = 10 * fills[i].intersection(&uses[j]).count() as u64 + 4 * uses[i].intersection(&uses[j]).count() as u64;
            }
        }
        let mut dynamic = vec![vec![0u64; n]; n];
        for input in corpus {
            let p: Vec<usize> = calls.decode(input).iter().filter_map(|c| c.desc).collect();
            for (k, &a) in p.iter().enumerate() {
                for &b in &p[k + 1..p.len().min(k + 1 + WINDOW)] {
                    dynamic[a][b] += 1;
                }
            }
        }
        let runs = stat
            .iter_mut()
            .zip(&mut dynamic)
            .map(|(s, d)| {
                normalize(s);
                normalize(d);
                let mut total = 0;
                s.iter().zip(d.iter()).map(|(a, b)| {
                    total += a * b;
                    total
                }).collect()
            })
            .collect();
        Self { runs }
    }

    /// A call to follow `bias`, by weight; `pick` is random.
    pub fn choose(&self, bias: usize, pick: u64) -> usize {
        let run = &self.runs[bias];
        let pick = pick % run.last().copied().unwrap_or(1).max(1);
        run.partition_point(|&c| c <= pick).min(run.len() - 1)
    }
}

impl Mutator {
    /// A mutation of `seed` by type (one time in `BYTE_SHARE` per hundred
    /// on bytes): arguments, calls inserted (with their producers),
    /// removed, duplicated, swapped, replaced, or a splice with `donor`.
    /// Each with its calls' results when it last ran (empty: not known;
    /// all succeeded).
    pub(crate) fn mutate_program(&mut self, calls: &Calls, (seed, results): (&[u8], &[u32]), donor: (&[u8], &[u32])) -> Vec<u8> {
        let mut p = calls.decode_run(seed, results);
        if p.iter().any(|c| c.failed) && self.next() % 100 < REPAIR_SHARE {
            self.repair(calls, &mut p);
        }
        let mut out = seed.to_vec();
        // Until it differs (a mutation can be a no-op: a removal of the
        // only call, a value drawn again).
        for _ in 0..4 {
            for _ in 0..1 + self.next() % 3 {
                self.mutate_once(calls, &mut p, donor);
            }
            out = calls.encode(&p);
            while out.len() > self.max_len && p.len() > 1 {
                p.pop();
                out = calls.encode(&p);
            }
            out.truncate(self.max_len);
            if out != seed {
                break;
            }
        }
        out
    }

    fn mutate_once(&mut self, calls: &Calls, p: &mut Vec<Call>, donor: (&[u8], &[u32])) {
        if p.is_empty() {
            self.insert_call(calls, p);
            return;
        }
        let i = self.choose(p.len());
        match self.next() % 100 {
            0..=34 => self.mutate_args(calls, p),
            35..=64 if p.len() >= LONG && self.next() % 2 == 0 => {
                p.remove(i);
            }
            35..=64 => self.insert_call(calls, p),
            65..=76 if p.len() > 1 => {
                p.remove(i);
            }
            77..=84 => self.splice(calls, p, donor),
            85..=89 => {
                let call = p[i].clone();
                p.insert(i + 1, call);
            }
            90..=94 => {
                let j = self.choose(p.len());
                p.swap(i, j);
            }
            _ => {
                let bias = if i > 0 { p[i - 1].desc } else { None };
                let d = self.choose_call(calls, bias);
                let state = Slots::after(calls, &p[..i]);
                p[i] = self.gen_call(calls, d, &state);
            }
        }
    }

    /// The call to insert after `bias`: the choice table's pick, one time
    /// in ten any call.
    fn choose_call(&mut self, calls: &Calls, bias: Option<usize>) -> usize {
        match bias {
            Some(b) if self.table.is_some() && self.next() % 10 != 0 => {
                let pick = self.next();
                self.table.as_ref().expect("checked").choose(b, pick)
            }
            _ => self.choose(calls.descs.len()),
        }
    }

    /// Insert a call (at the end half the time), after the producers its
    /// resources need.
    pub(crate) fn insert_call(&mut self, calls: &Calls, p: &mut Vec<Call>) {
        let pos = if p.is_empty() || self.next() % 2 == 0 { p.len() } else { self.choose(p.len() + 1) };
        let bias = if pos > 0 { p[self.choose(pos)].desc } else { None };
        let d = self.choose_call(calls, bias);
        self.place(calls, p, pos, d, 0);
    }

    /// Put call `d` at `pos`, first a producer for each kind it uses that
    /// no slot holds there (kinds an argument may also give inline, half
    /// the time). Where the call after it is.
    pub(crate) fn place(&mut self, calls: &Calls, p: &mut Vec<Call>, mut pos: usize, d: usize, depth: usize) -> usize {
        if depth < DEPTH {
            let state = Slots::after(calls, &p[..pos]);
            let mut needs: Vec<(&str, bool)> = Vec::new();
            for arg in &calls.descs[d].args {
                let ty = match &arg.ty {
                    Type::Array { elem, .. } => &**elem,
                    ty => ty,
                };
                if let Type::Res { kind, dir: Dir::In | Dir::Close, inline, .. } = ty {
                    match needs.iter_mut().find(|(k, _)| k == kind) {
                        Some(n) => n.1 &= *inline != 0,
                        None => needs.push((kind, *inline != 0)),
                    }
                }
            }
            for (kind, inline) in needs {
                if !state.live(kind).is_empty() || (inline && self.next() % 2 == 0) {
                    continue;
                }
                let producers: Vec<usize> = calls.producers.get(kind).map_or(Vec::new(), |v| v.iter().copied().filter(|&q| q != d).collect());
                if !producers.is_empty() {
                    let q = producers[self.choose(producers.len())];
                    pos = self.place(calls, p, pos, q, depth + 1);
                }
            }
        }
        let state = Slots::after(calls, &p[..pos]);
        let call = self.gen_call(calls, d, &state);
        p.insert(pos, call);
        pos + 1
    }

    /// Call `d` with arguments drawn by type, its resources from `state`.
    pub(crate) fn gen_call(&mut self, calls: &Calls, d: usize, state: &Slots) -> Call {
        let desc = &calls.descs[d];
        let args = desc.args.iter().map(|a| self.gen_value(&a.ty, state)).collect();
        let mut call = Call { op: desc.op, desc: Some(d), args, extra: Vec::new(), failed: false };
        calls.fit_arrays(&mut call, |ty| self.gen_value(ty, state));
        calls.fix_lens(&mut call);
        call
    }

    /// One or two arguments of a call changed by type.
    fn mutate_args(&mut self, calls: &Calls, p: &mut [Call]) {
        let described: Vec<usize> = (0..p.len())
            .filter(|&i| p[i].desc.is_some_and(|d| calls.descs[d].args.iter().any(|a| !matches!(a.ty, Type::Const { .. } | Type::Len { .. }))))
            .collect();
        if described.is_empty() {
            return;
        }
        let i = described[self.choose(described.len())];
        let d = p[i].desc.expect("described");
        let desc = &calls.descs[d];
        let state = Slots::after(calls, &p[..i]);
        let mutable: Vec<usize> = (0..desc.args.len()).filter(|&j| !matches!(desc.args[j].ty, Type::Const { .. } | Type::Len { .. })).collect();
        for _ in 0..1 + usize::from(self.next() % 3 == 0) {
            let j = mutable[self.choose(mutable.len())];
            let ty = &desc.args[j].ty;
            if let Type::Array { len, elem } = ty {
                let k = desc.args.iter().position(|a| a.name == *len).expect("checked");
                let count = &desc.args[k].ty;
                let (min, max) = match count {
                    Type::Int { size, min, max, .. } => (*min, max.unwrap_or(u64::MAX >> (64 - 8 * u32::from(*size)))),
                    _ => (0, 0),
                };
                let Value::Array(items) = &mut p[i].args[j] else { continue };
                match self.next() % 3 {
                    0 if (items.len() as u64) < max.min(64) => items.push(self.gen_value(elem, &state)),
                    1 if items.len() as u64 > min => {
                        let at = self.choose(items.len());
                        items.remove(at);
                    }
                    _ if !items.is_empty() => {
                        let at = self.choose(items.len());
                        self.mutate_value(elem, &mut items[at], &state);
                    }
                    _ => {}
                }
                let n = items.len() as u64;
                p[i].args[k] = Value::Int(count.int_raw(n));
            } else {
                let mut v = p[i].args[j].clone();
                self.mutate_value(ty, &mut v, &state);
                p[i].args[j] = v;
            }
        }
        calls.fit_arrays(&mut p[i], |ty| self.gen_value(ty, &state));
        if self.next() % 10 != 0 {
            calls.fix_lens(&mut p[i]);
        }
        // Other arguments: it may not fail now.
        p[i].failed = false;
    }

    /// Replace the program from a point with `donor`'s calls from a
    /// point; their resources that name no live slot then name one (most
    /// of the time).
    fn splice(&mut self, calls: &Calls, p: &mut Vec<Call>, donor: (&[u8], &[u32])) {
        let tail = calls.decode_run(donor.0, donor.1);
        if tail.is_empty() {
            return;
        }
        let i = self.choose(p.len() + 1);
        let j = self.choose(tail.len());
        p.truncate(i);
        let mut state = Slots::after(calls, p);
        for mut call in tail.into_iter().skip(j) {
            self.fix_resources(calls, &mut call, &state);
            state.apply(calls, &call);
            p.push(call);
        }
    }

    /// Failed calls' resource arguments that name no live slot (a producer
    /// failed, a slot closed, none filled) point at a live one
    /// (`fix_resources`): the calls may not fail now. Before the first one
    /// whose first resource (its handle) is of a kind no slot holds, a
    /// producer of it (and its own). Corpus
    /// programs keep such calls otherwise: each child fails them again.
    pub(crate) fn repair(&mut self, calls: &Calls, p: &mut Vec<Call>) {
        let mut state = Slots::default();
        let mut inserted = 0;
        let mut i = 0;
        while i < p.len() {
            if p[i].failed {
                let first = calls.uses(&p[i]).into_iter().find(|u| u.dir != Dir::Out);
                let missing = first.filter(|u| state.live(u.kind).is_empty()).map(|u| u.kind.to_string());
                if let (true, Some(kind)) = (inserted < REPAIR_INSERTS, missing) {
                    let producers = calls.producers.get(&kind).map_or(&[][..], Vec::as_slice);
                    if !producers.is_empty() {
                        let q = producers[self.choose(producers.len())];
                        i = self.place(calls, p, i, q, 1);
                        state = Slots::after(calls, &p[..i]);
                        inserted += 1;
                    }
                }
                let args = p[i].args.clone();
                self.fix_resources(calls, &mut p[i], &state);
                p[i].failed = p[i].args == args;
            }
            state.apply(calls, &p[i]);
            i += 1;
        }
    }

    /// Point `call`'s resource arguments that name no live slot at a live
    /// one of their kind, 4 times in 5.
    pub(crate) fn fix_resources(&mut self, calls: &Calls, call: &mut Call, state: &Slots) {
        let Some(d) = call.desc else { return };
        for (arg, v) in calls.descs[d].args.iter().zip(call.args.iter_mut()) {
            match (&arg.ty, v) {
                (Type::Array { elem, .. }, Value::Array(items)) => {
                    for item in items {
                        self.fix_resource(elem, item, state);
                    }
                }
                (ty, v) => self.fix_resource(ty, v, state),
            }
        }
    }

    fn fix_resource(&mut self, ty: &Type, v: &mut Value, state: &Slots) {
        if let (Type::Res { kind, dir: Dir::In | Dir::Close, slots, tag, .. }, Value::Res(raw, None)) = (ty, v) {
            let live = state.live(kind);
            if !live.is_empty() && !live.contains(&(*raw % slots)) && self.next() % 5 != 0 {
                *raw = live[self.choose(live.len())] | tag;
            }
        }
    }

    pub(crate) fn gen_value(&mut self, ty: &Type, state: &Slots) -> Value {
        match ty {
            Type::Int { .. } => {
                let v = self.gen_int(ty);
                Value::Int(ty.int_raw(v))
            }
            Type::Flags { size, values } => {
                let raw = match self.next() % 10 {
                    0 => 0,
                    1 => self.next(),
                    _ => values.iter().filter(|_| self.next() % 2 == 0).fold(0, |a, v| a | v),
                };
                Value::Int(raw & mask_of(*size))
            }
            Type::Const { value, .. } => Value::Int(*value),
            Type::Res { .. } => self.gen_res(ty, state),
            Type::Len { .. } => Value::Int(0),
            Type::Array { .. } => Value::Array(Vec::new()),
            Type::Bytes { min, max } => Value::Bytes(self.gen_bytes(*min, *max)),
        }
    }

    /// An int's value (not raw): one worth trying, a boundary, a compared
    /// word, or any in range (small ones likelier).
    fn gen_int(&mut self, ty: &Type) -> u64 {
        let Type::Int { size, min, max, values, .. } = ty else { return 0 };
        let max = max.unwrap_or(mask_of(*size));
        let width = max - min;
        match self.next() % 100 {
            0..=39 if !values.is_empty() => values[self.choose(values.len())],
            40..=54 => [*min, max, min.saturating_add(1).min(max), max.saturating_sub(1).max(*min), min + width / 2][self.choose(5)],
            55..=64 if *size >= 4 && !self.dictionary.is_empty() => u64::from(self.word()),
            _ => {
                let bits = u64::from(64 - width.leading_zeros()).max(1);
                let b = 1 + self.next() % bits;
                let r = self.next() & (u64::MAX >> (64 - b));
                min + if width == u64::MAX { r } else { r % (width + 1) }
            }
        }
    }

    /// A slot of the resource's kind. Used: most often a live one (the
    /// latest half the time), sometimes a dead one (closed, or its
    /// producer failed) or any, or a value inline. Filled: mostly a free
    /// slot.
    fn gen_res(&mut self, ty: &Type, state: &Slots) -> Value {
        let Type::Res { kind, dir, slots, tag, inline, .. } = ty else { return Value::Res(0, None) };
        let slots = *slots;
        let slot = match dir {
            Dir::In | Dir::Close => {
                if *inline != 0 && self.next() % 100 < 15 {
                    let v = self.gen_inline();
                    return Value::Res(0x80 | (self.next() as u8 & 0x7f), Some(v & mask_of(*inline)));
                }
                let (live, closed) = (state.live(kind), state.closed(kind));
                match self.next() % 100 {
                    r if r < 85 && !live.is_empty() => {
                        if r < 45 { live[live.len() - 1] } else { live[self.choose(live.len())] }
                    }
                    85..=92 if !closed.is_empty() => closed[self.choose(closed.len())],
                    _ => self.choose(usize::from(slots)) as u8,
                }
            }
            Dir::Out => {
                let live = state.live(kind);
                let free: Vec<u8> = (0..slots).filter(|s| !live.contains(s)).collect();
                if !free.is_empty() && self.next() % 10 < 7 {
                    free[self.choose(free.len())]
                } else {
                    self.choose(usize::from(slots)) as u8
                }
            }
        };
        Value::Res(slot | tag, None)
    }

    /// A value given inline for a resource (an LSN: container << 32 |
    /// block offset).
    fn gen_inline(&mut self) -> u64 {
        match self.next() % 6 {
            0 => 0,
            1 => u64::MAX,
            2 => (self.next() % 4) << 32 | (self.next() % 0x400) << 9,
            3 if !self.dictionary.is_empty() => u64::from(self.word()),
            4 => self.next() % 0x1000,
            _ => self.next(),
        }
    }

    fn gen_bytes(&mut self, min: usize, max: usize) -> Vec<u8> {
        let cap = match self.next() % 10 {
            0..=5 => max.min(64),
            6..=8 => max.min(256),
            _ => max,
        }
        .max(min);
        let len = min + self.choose(cap - min + 1);
        match self.next() % 4 {
            0 => vec![0; len],
            1 => (0..len).map(|_| self.next() as u8).collect(),
            2 => vec![self.next() as u8; len],
            _ => {
                let mut out = Vec::with_capacity(len + 4);
                while out.len() < len {
                    let w = if self.dictionary.is_empty() { self.next() as u32 % 0x100 } else { self.word() };
                    out.extend_from_slice(&w.to_le_bytes());
                }
                out.truncate(len);
                out
            }
        }
    }

    pub(crate) fn mutate_value(&mut self, ty: &Type, v: &mut Value, state: &Slots) {
        if let Type::Res { .. } = ty {
            *v = self.gen_res(ty, state);
            return;
        }
        match (ty, v) {
            (Type::Int { size, .. }, Value::Int(raw)) => {
                *raw = match self.next() % 10 {
                    0..=4 => ty.int_raw(self.gen_int(ty)),
                    5..=7 => {
                        let delta = 1 + self.next() % 16;
                        let value = ty.int_value(*raw);
                        let value = if self.next() % 2 == 0 { value.wrapping_add(delta) } else { value.wrapping_sub(delta) };
                        ty.int_raw(value)
                    }
                    _ => *raw ^ (1 << (self.next() % (8 * u64::from(*size)))),
                };
            }
            (Type::Flags { size, values }, Value::Int(raw)) => {
                *raw = match self.next() % 10 {
                    0..=5 if !values.is_empty() => *raw ^ values[self.choose(values.len())],
                    6 => 0,
                    7 => *raw ^ (1 << (self.next() % (8 * u64::from(*size)))),
                    _ => match self.gen_value(ty, state) {
                        Value::Int(r) => r,
                        _ => *raw,
                    },
                };
            }
            (Type::Bytes { min, max }, Value::Bytes(bytes)) => match self.next() % 10 {
                0..=5 => {
                    let mut b = self.mutate(bytes);
                    b.truncate(*max);
                    b.resize(b.len().max(*min), 0);
                    *bytes = b;
                }
                6..=7 => *bytes = self.gen_bytes(*min, *max),
                _ => {
                    let cap = (*max).min(bytes.len().saturating_mul(2).max(16));
                    let len = *min + self.choose(cap.saturating_sub(*min) + 1);
                    bytes.resize(len, 0);
                }
            },
            _ => {}
        }
    }
}

fn mask_of(size: u8) -> u64 {
    if size >= 8 {
        u64::MAX
    } else {
        (1u64 << (8 * u32::from(size))) - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calls::tests::sample;
    use crate::sequence;

    fn live_share(c: &Calls, input: &[u8]) -> (usize, usize) {
        let p = c.decode(input);
        let mut state = Slots::default();
        let (mut live, mut all) = (0, 0);
        for call in &p {
            for u in c.uses(call) {
                if u.dir != Dir::Out && !state.live(u.kind).is_empty() {
                    all += 1;
                    live += usize::from(state.live(u.kind).contains(&u.slot));
                }
            }
            state.apply(c, call);
        }
        (live, all)
    }

    #[test]
    fn inserted_calls_come_after_their_producers() {
        let c = sample();
        let mut m = Mutator::new(3, 4096);
        let read = c.descs.iter().position(|d| d.name == "read").unwrap();
        let mut p = Vec::new();
        m.place(&c, &mut p, 0, read, 0);
        let names: Vec<&str> = p.iter().map(|call| c.descs[call.desc.unwrap()].name.as_str()).collect();
        assert_eq!(&names[..2], ["open", "area"], "{names:?}");
        assert_eq!(names.last(), Some(&"read"));
        // The area's log and the read's area are the ones filled before.
        for i in [1, p.len() - 1] {
            let state = Slots::after(&c, &p[..i]);
            let u = c.uses(&p[i])[0];
            assert!(state.live(u.kind).contains(&u.slot), "{:?}", c.show(&p));
        }
    }

    #[test]
    fn typed_mutations_use_live_slots_mostly_not_always() {
        let c = sample();
        let seed = sequence(&[(0, vec![1, 0]), (3, vec![1, 2, 0, 16, 0, 0]), (4, vec![2, 0, 3, 0, b'x']), (6, vec![2, 3])]);
        let mut m = Mutator::new(11, 4096);
        m.describe(c.clone(), std::slice::from_ref(&seed));
        let (mut live, mut all, mut differ) = (0, 0, 0);
        for _ in 0..2000 {
            let out = m.mutate_program(&c, (&seed, &[]), (&seed, &[]));
            assert_eq!(sequence(&crate::calls(&out)), out, "whole calls");
            differ += usize::from(out != seed);
            let (l, a) = live_share(&c, &out);
            live += l;
            all += a;
        }
        assert!(differ > 1900, "{differ}");
        let share = live as f64 / all as f64;
        assert!((0.75..0.99).contains(&share), "{share}");
    }

    /// Log 1, area 2 on it (failed), an append and a read on the area.
    fn failed_area() -> (Vec<u8>, Vec<u32>) {
        let seed = sequence(&[(0, vec![1, 0]), (3, vec![1, 2, 0, 16, 0, 0]), (4, vec![2, 0, 3, 0, b'x']), (6, vec![2, 3])]);
        (seed, vec![0, 3 << 24 | 87, 4 << 24 | 6, 6 << 24 | 6])
    }

    #[test]
    fn calls_inserted_after_a_failed_producer_get_their_own() {
        let c = sample();
        let (seed, results) = failed_area();
        let read = c.descs.iter().position(|d| d.name == "read").unwrap();
        let mut m = Mutator::new(3, 4096);
        let areas = |p: &[Call]| p.iter().filter(|call| c.descs[call.desc.unwrap()].name == "area").count();
        let mut live = 0;
        for _ in 0..100 {
            let mut p = c.decode_run(&seed, &results);
            m.place(&c, &mut p, 4, read, 0);
            assert_eq!(areas(&p), 2, "{:?}", c.show(&p));
            let state = Slots::after(&c, &p[..p.len() - 1]);
            let u = c.uses(p.last().unwrap())[0];
            live += usize::from(state.live(u.kind).contains(&u.slot));
            // Not known to fail: area 2 does.
            let mut p = c.decode(&seed);
            m.place(&c, &mut p, 4, read, 0);
            assert_eq!(areas(&p), 1, "{:?}", c.show(&p));
        }
        assert!(live > 75, "the read's area is the new one: {live}");
    }

    #[test]
    fn slots_of_failed_producers_are_used_now_and_then() {
        let c = sample();
        // Area 4, then area 2 (failed) and its uses.
        let seed = sequence(&[(0, vec![1, 0]), (3, vec![1, 4, 0, 16, 0, 0]), (3, vec![1, 2, 0, 16, 0, 0]), (4, vec![2, 0, 3, 0, b'x']), (6, vec![2, 3])]);
        let results = [0, 3 << 24, 3 << 24 | 87, 4 << 24 | 6, 6 << 24 | 6];
        let ty = &c.descs.iter().find(|d| d.name == "read").unwrap().args[0].ty;
        let mut m = Mutator::new(5, 4096);
        let mut share = |p: &[Call]| {
            let state = Slots::after(&c, p);
            (0..1000).filter(|_| m.gen_value(ty, &state) == Value::Res(2, None)).count()
        };
        // 8% dead slots, 7% any (one in 8 is area 2).
        let dead = share(&c.decode_run(&seed, &results));
        assert!((40..150).contains(&dead), "{dead}");
        // Live and the latest: about two times in three.
        let live = share(&c.decode(&seed));
        assert!(live > 550, "{live}");
    }

    #[test]
    fn failed_calls_on_dead_slots_are_repaired_mostly() {
        let c = sample();
        // Area 4, then area 2 (failed), an append and a read on area 2
        // (failed), a read on area 4.
        let seed = sequence(&[(0, vec![1, 0]), (3, vec![1, 4, 0, 16, 0, 0]), (3, vec![1, 2, 0, 16, 0, 0]), (4, vec![2, 0, 3, 0, b'x']), (6, vec![2, 3]), (6, vec![4, 3])]);
        let results = [0, 3 << 24, 3 << 24 | 87, 4 << 24 | 87, 6 << 24 | 87, 6 << 24];
        let mut m = Mutator::new(13, 4096);
        let mut repaired = 0;
        for _ in 0..200 {
            let mut p = c.decode_run(&seed, &results);
            m.repair(&c, &mut p);
            assert_eq!(c.show(&p[..3]), c.show(&c.decode(&seed)[..3]), "the failed area has no dead slot to fix");
            assert_eq!(p[5].args[0], Value::Res(4, None), "calls that succeeded stay");
            for call in &p[3..5] {
                let fixed = call.args[0] == Value::Res(4, None);
                assert_eq!(call.failed, !fixed);
                repaired += usize::from(fixed);
            }
        }
        assert!((280..360).contains(&repaired), "4 in 5: {repaired}");
        // No area at all: one comes first.
        let (seed, results) = failed_area();
        let mut repaired = 0;
        for _ in 0..200 {
            let mut p = c.decode_run(&seed, &results);
            m.repair(&c, &mut p);
            let names: Vec<&str> = p.iter().map(|call| c.descs[call.desc.unwrap()].name.as_str()).collect();
            assert_eq!(names, ["open", "area", "area", "append", "read"]);
            let state = Slots::after(&c, &p[..3]);
            repaired += usize::from(state.live("area").contains(&c.uses(&p[3])[0].slot));
        }
        assert!(repaired > 140, "{repaired}");
    }

    #[test]
    fn splices_point_resources_at_live_slots() {
        let c = sample();
        // The seed opens log 1, area 2; the donor log 6, area 5, used after.
        let seed = sequence(&[(0, vec![1, 0]), (3, vec![1, 2, 0, 16, 0, 0])]);
        let donor = sequence(&[(0, vec![6, 0]), (3, vec![6, 5, 0, 16, 0, 0]), (5, vec![]), (4, vec![5, 0, 3, 0]), (6, vec![5, 3]), (6, vec![5, 3])]);
        let mut m = Mutator::new(5, 4096);
        let (mut live, mut all) = (0, 0);
        for _ in 0..500 {
            let mut p = c.decode(&seed);
            m.splice(&c, &mut p, (&donor, &[]));
            let (l, a) = live_share(&c, &c.encode(&p));
            live += l;
            all += a;
        }
        assert!(live as f64 > 0.75 * all as f64, "{live} of {all}");
    }

    #[test]
    fn the_choice_table_learns_what_follows_what() {
        let c = sample();
        let open = c.descs.iter().position(|d| d.name == "open").unwrap();
        let tail = c.descs.iter().position(|d| d.name == "tail").unwrap();
        let corpus: Vec<Vec<u8>> = (0..20).map(|_| sequence(&[(0, vec![1, 0]), (8, vec![1, 0])])).collect();
        let t = ChoiceTable::new(&c, &corpus);
        let mut m = Mutator::new(9, 64);
        let picks = (0..1000).filter(|_| t.choose(open, m.next()) == tail).count();
        assert!(picks > 500, "{picks}");
        // Without a corpus: calls sharing a kind first.
        let t = ChoiceTable::new(&c, &[]);
        let near: Vec<usize> = ["area", "close", "tail", "names"].iter().map(|n| c.descs.iter().position(|d| d.name == *n).unwrap()).collect();
        let picks = (0..1000).filter(|_| near.contains(&t.choose(open, m.next()))).count();
        assert!(picks > 900, "{picks}");
    }

    #[test]
    fn generated_ints_stay_in_range() {
        let c = sample();
        let mut m = Mutator::new(1, 64);
        let area = c.descs.iter().position(|d| d.name == "area").unwrap();
        let ty = &c.descs[area].args[2].ty;
        let inside = (0..1000).filter(|_| matches!(m.gen_value(ty, &Slots::default()), Value::Int(r) if r < 1 << 20)).count();
        assert_eq!(inside, 1000);
        let open = &c.descs[0].args[1].ty;
        let values: BTreeSet<u64> = (0..300).map(|_| match m.gen_value(open, &Slots::default()) { Value::Int(r) => open.int_value(r), _ => 0 }).collect();
        assert_eq!(values, (1..=5).collect(), "mode is 1..=5");
    }
}

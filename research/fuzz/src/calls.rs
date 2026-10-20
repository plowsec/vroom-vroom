//! Call descriptions for sequence harnesses (syzkaller-like): each op
//! code's payload as typed arguments, in payload order, so programs can
//! be generated, mutated and minimized by type, with resources (handles,
//! slots) produced by one call and used by the next.
//!
//! An op code may have several descriptions (variants, like syzkaller's
//! `call$variant`): a payload is the first whose `const` arguments and
//! resource tags it matches. Past the payload's end the client reads
//! zeros; bytes past the described arguments are kept as they are.

use crate::{sequence, Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallDesc {
    pub op: u32,
    pub name: String,
    #[serde(default)]
    pub args: Vec<Arg>,
    /// Resources the call fills in fixed slots, without an argument.
    #[serde(default)]
    pub outputs: Vec<Output>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Output {
    pub kind: String,
    pub slot: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Arg {
    pub name: String,
    #[serde(flatten)]
    pub ty: Type,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dir {
    /// The call uses the slot.
    #[default]
    In,
    /// The call fills the slot.
    Out,
    /// The call uses the slot and closes it.
    Close,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Type {
    /// A little-endian integer of `size` bytes; values `min..=max`
    /// (default: the whole size). `modulo`: the client takes
    /// `min + raw % (max - min + 1)`, else the raw value. `values`: ones
    /// worth trying first, `names` theirs.
    Int {
        size: u8,
        #[serde(default)]
        min: u64,
        #[serde(default)]
        max: Option<u64>,
        #[serde(default)]
        modulo: bool,
        #[serde(default)]
        values: Vec<u64>,
        #[serde(default)]
        names: Vec<String>,
    },
    /// Any OR of `values`.
    Flags { size: u8, values: Vec<u64> },
    /// The value that selects this variant: `raw & mask`, `raw % modulo`
    /// or `raw` equals `value`.
    Const {
        size: u8,
        value: u64,
        #[serde(default)]
        mask: Option<u64>,
        #[serde(default)]
        modulo: Option<u64>,
    },
    /// A slot of a resource kind, one byte: `slot | tag` (`raw & tag_mask
    /// == tag` selects the variant; the client takes the byte modulo
    /// `slots`). `inline`: bit 7 set instead says the next `inline`
    /// bytes are the value itself.
    Res {
        kind: String,
        #[serde(default)]
        dir: Dir,
        slots: u8,
        #[serde(default)]
        tag: u8,
        #[serde(default)]
        tag_mask: u8,
        #[serde(default)]
        inline: u8,
    },
    /// The byte length of the arguments `of` (this one included if named).
    Len { size: u8, of: Vec<String> },
    /// Elements, as many as the value of the earlier int `len`.
    Array { len: String, elem: Box<Type> },
    /// The rest of the payload (the last argument).
    Bytes {
        #[serde(default)]
        min: usize,
        max: usize,
    },
}

/// A decoded argument. Ints keep the raw value as in the payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Int(u64),
    /// The slot byte; with an inline value (bit 7), the value.
    Res(u8, Option<u64>),
    Array(Vec<Value>),
    Bytes(Vec<u8>),
}

/// A call of a program: its description (`None`: no description fits;
/// the payload is all in `extra`), arguments, and bytes past them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub op: u32,
    pub desc: Option<usize>,
    pub args: Vec<Value>,
    pub extra: Vec<u8>,
}

/// A use of a resource slot by an argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Use<'a> {
    pub kind: &'a str,
    pub dir: Dir,
    pub slot: u8,
}

/// The descriptions of a harness's calls.
#[derive(Debug, Clone)]
pub struct Calls {
    pub descs: Vec<CallDesc>,
    by_op: BTreeMap<u32, Vec<usize>>,
    /// Slots per resource kind.
    pub kinds: BTreeMap<String, u8>,
    /// Calls that fill each kind (an `out` argument or a fixed output).
    pub producers: BTreeMap<String, Vec<usize>>,
}

pub fn type_size(size: u8) -> Result<()> {
    if matches!(size, 1 | 2 | 4 | 8) {
        Ok(())
    } else {
        Err(Error::Invalid(format!("call argument size {size}: 1, 2, 4 or 8")))
    }
}

fn mask(size: u8) -> u64 {
    if size >= 8 {
        u64::MAX
    } else {
        (1u64 << (8 * u32::from(size))) - 1
    }
}

impl Type {
    /// Int: its range's size (0: all 2^64 values).
    pub fn span(&self) -> u64 {
        match self {
            Type::Int { size, min, max, .. } => max.unwrap_or(mask(*size)).wrapping_sub(*min).wrapping_add(1),
            _ => 0,
        }
    }

    /// Int: the value the client sees for `raw`.
    pub fn int_value(&self, raw: u64) -> u64 {
        match self {
            Type::Int { min, modulo: true, .. } => {
                let span = self.span();
                if span == 0 { min.wrapping_add(raw) } else { min.wrapping_add(raw % span) }
            }
            _ => raw,
        }
    }

    /// Int: the raw value that gives `value`.
    pub fn int_raw(&self, value: u64) -> u64 {
        match self {
            Type::Int { size, min, modulo: true, .. } => value.wrapping_sub(*min) & mask(*size),
            Type::Int { size, .. } | Type::Flags { size, .. } | Type::Const { size, .. } | Type::Len { size, .. } => value & mask(*size),
            _ => value,
        }
    }

    fn check(&self, call: &CallDesc, index: usize, kinds: &mut BTreeMap<String, u8>) -> Result<()> {
        let bad = |what: &str| Err(Error::Invalid(format!("call {} ({}) argument {index}: {what}", call.op, call.name)));
        match self {
            Type::Int { size, min, max, values, names, .. } => {
                type_size(*size)?;
                if max.is_some_and(|m| m < *min || m > mask(*size)) {
                    return bad("max below min or past its size");
                }
                if !names.is_empty() && names.len() != values.len() {
                    return bad("a name for each of values");
                }
            }
            Type::Flags { size, .. } | Type::Const { size, .. } | Type::Len { size, .. } => type_size(*size)?,
            Type::Res { kind, slots, inline, tag, tag_mask, .. } => {
                if *slots == 0 || (*inline != 0 && (*slots > 128 || *inline > 8)) || tag & !tag_mask != 0 {
                    return bad("slots 1..=128 with inline (≤8 bytes), tag within tag_mask");
                }
                if *kinds.entry(kind.clone()).or_insert(*slots) != *slots {
                    return bad("one slot count per resource kind");
                }
            }
            Type::Array { len, elem } => {
                let count = call.args[..index].iter().find(|a| a.name == *len);
                if !matches!(count.map(|a| &a.ty), Some(Type::Int { .. })) {
                    return bad("an array's len names an earlier int");
                }
                if matches!(**elem, Type::Array { .. } | Type::Bytes { .. } | Type::Len { .. }) {
                    return bad("array elements are ints, flags, consts or resources");
                }
                elem.check(call, index, kinds)?;
            }
            Type::Bytes { min, max } => {
                if index + 1 != call.args.len() || min > max {
                    return bad("bytes come last, min ≤ max");
                }
            }
        }
        if let Type::Len { of, .. } = self {
            if of.iter().any(|n| !call.args.iter().any(|a| a.name == *n)) {
                return bad("len of an argument the call doesn't have");
            }
        }
        Ok(())
    }
}

/// Reads a payload as the client does: past its end, zeros.
struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn int(&mut self, size: u8) -> u64 {
        let mut v = 0u64;
        for i in 0..usize::from(size) {
            v |= u64::from(self.data.get(self.at + i).copied().unwrap_or(0)) << (8 * i);
        }
        self.at += usize::from(size);
        v
    }
}

impl Calls {
    pub fn new(descs: Vec<CallDesc>) -> Result<Self> {
        let mut by_op = BTreeMap::<u32, Vec<usize>>::new();
        let mut kinds = BTreeMap::new();
        let mut producers = BTreeMap::<String, Vec<usize>>::new();
        for (i, d) in descs.iter().enumerate() {
            let mut names = BTreeSet::new();
            for (index, arg) in d.args.iter().enumerate() {
                if !names.insert(arg.name.as_str()) {
                    return Err(Error::Invalid(format!("call {} ({}): two arguments named {}", d.op, d.name, arg.name)));
                }
                arg.ty.check(d, index, &mut kinds)?;
            }
            by_op.entry(d.op).or_default().push(i);
            let mut filled: BTreeSet<&str> = d.outputs.iter().map(|o| o.kind.as_str()).collect();
            for arg in &d.args {
                if let Type::Res { kind, dir: Dir::Out, .. } = &arg.ty {
                    filled.insert(kind);
                }
            }
            for kind in filled {
                producers.entry(kind.to_string()).or_default().push(i);
            }
        }
        for d in &descs {
            if let Some(o) = d.outputs.iter().find(|o| !kinds.get(&o.kind).is_some_and(|&n| o.slot < n)) {
                return Err(Error::Invalid(format!("call {} ({}): output {}{}: a kind an argument has, a slot it has", d.op, d.name, o.kind, o.slot)));
            }
        }
        Ok(Self { descs, by_op, kinds, producers })
    }

    /// The description `payload` of `op` matches, with its arguments and
    /// the bytes past them.
    pub fn decode_call(&self, op: u32, payload: &[u8]) -> Call {
        for &d in self.by_op.get(&op).map(Vec::as_slice).unwrap_or(&[]) {
            if let Some((args, extra)) = self.decode_with(d, payload) {
                return Call { op, desc: Some(d), args, extra };
            }
        }
        Call { op, desc: None, args: Vec::new(), extra: payload.to_vec() }
    }

    fn decode_with(&self, d: usize, payload: &[u8]) -> Option<(Vec<Value>, Vec<u8>)> {
        let desc = &self.descs[d];
        let mut r = Reader { data: payload, at: 0 };
        let mut args: Vec<Value> = Vec::with_capacity(desc.args.len());
        for arg in &desc.args {
            let v = match &arg.ty {
                Type::Array { len, elem } => {
                    let count = self.count(desc, &args, len);
                    let mut items = Vec::with_capacity(count);
                    for _ in 0..count {
                        items.push(Self::decode_one(elem, &mut r)?);
                    }
                    Value::Array(items)
                }
                Type::Bytes { .. } => {
                    let rest = payload.get(r.at..).unwrap_or(&[]).to_vec();
                    r.at = r.at.max(payload.len());
                    Value::Bytes(rest)
                }
                ty => Self::decode_one(ty, &mut r)?,
            };
            args.push(v);
        }
        let extra = payload.get(r.at..).unwrap_or(&[]).to_vec();
        Some((args, extra))
    }

    /// An array's length: the value of its `len` int among `args`.
    fn count(&self, desc: &CallDesc, args: &[Value], len: &str) -> usize {
        let i = desc.args.iter().position(|a| a.name == len).expect("checked");
        match args.get(i) {
            Some(Value::Int(raw)) => desc.args[i].ty.int_value(*raw).min(4096) as usize,
            _ => 0,
        }
    }

    fn decode_one(ty: &Type, r: &mut Reader) -> Option<Value> {
        Some(match ty {
            Type::Int { size, .. } | Type::Flags { size, .. } | Type::Len { size, .. } => Value::Int(r.int(*size)),
            Type::Const { size, value, mask, modulo } => {
                let raw = r.int(*size);
                let seen = match (mask, modulo) {
                    (Some(m), _) => raw & m,
                    (None, Some(n)) if *n != 0 => raw % n,
                    _ => raw,
                };
                if seen != *value {
                    return None;
                }
                Value::Int(raw)
            }
            Type::Res { inline, tag, tag_mask, .. } => {
                let raw = r.int(1) as u8;
                if raw & tag_mask != *tag {
                    return None;
                }
                let value = (*inline != 0 && raw & 0x80 != 0).then(|| r.int(*inline));
                Value::Res(raw, value)
            }
            Type::Array { .. } | Type::Bytes { .. } => return None,
        })
    }

    pub fn decode(&self, input: &[u8]) -> Vec<Call> {
        crate::calls(input).into_iter().map(|(op, payload)| self.decode_call(op, &payload)).collect()
    }

    pub fn encode_call(&self, call: &Call) -> (u32, Vec<u8>) {
        let mut out = Vec::new();
        if let Some(d) = call.desc {
            for (arg, v) in self.descs[d].args.iter().zip(&call.args) {
                encode_value(&arg.ty, v, &mut out);
            }
        }
        out.extend_from_slice(&call.extra);
        (call.op, out)
    }

    pub fn encode(&self, program: &[Call]) -> Vec<u8> {
        sequence(&program.iter().map(|c| self.encode_call(c)).collect::<Vec<_>>())
    }

    /// Set every `len` argument of `call` to what it measures.
    pub fn fix_lens(&self, call: &mut Call) {
        let Some(d) = call.desc else { return };
        let desc = &self.descs[d];
        for (i, arg) in desc.args.iter().enumerate() {
            if let Type::Len { size, of } = &arg.ty {
                let total: usize = of
                    .iter()
                    .filter_map(|n| desc.args.iter().position(|a| a.name == *n))
                    .map(|j| {
                        let mut bytes = Vec::new();
                        encode_value(&desc.args[j].ty, &call.args[j], &mut bytes);
                        bytes.len()
                    })
                    .sum();
                call.args[i] = Value::Int(total as u64 & mask(*size));
            }
        }
    }

    /// Resizes `call`'s arrays to their `len` ints (elements from `make`).
    pub fn fit_arrays(&self, call: &mut Call, mut make: impl FnMut(&Type) -> Value) {
        let Some(d) = call.desc else { return };
        let desc = &self.descs[d];
        for (i, arg) in desc.args.iter().enumerate() {
            if let Type::Array { len, elem } = &arg.ty {
                let count = self.count(desc, &call.args, len);
                if let Value::Array(items) = &mut call.args[i] {
                    items.truncate(count);
                    while items.len() < count {
                        items.push(make(elem));
                    }
                }
            }
        }
    }

    /// The resource slots `call` uses and fills, in argument order, then
    /// its fixed outputs.
    pub fn uses<'a>(&'a self, call: &Call) -> Vec<Use<'a>> {
        let mut out = Vec::new();
        let Some(d) = call.desc else { return out };
        let desc = &self.descs[d];
        for (arg, v) in desc.args.iter().zip(&call.args) {
            match (&arg.ty, v) {
                (Type::Res { kind, dir, slots, .. }, Value::Res(raw, None)) => out.push(Use { kind, dir: *dir, slot: raw % slots }),
                (Type::Array { elem, .. }, Value::Array(items)) => {
                    if let Type::Res { kind, dir, slots, .. } = &**elem {
                        for item in items {
                            if let Value::Res(raw, None) = item {
                                out.push(Use { kind, dir: *dir, slot: raw % slots });
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        out.extend(desc.outputs.iter().map(|o| Use { kind: &o.kind, dir: Dir::Out, slot: o.slot }));
        out
    }

    /// One line per call: its name and arguments.
    pub fn show(&self, program: &[Call]) -> Vec<String> {
        program.iter().map(|c| self.show_call(c)).collect()
    }

    pub fn show_call(&self, call: &Call) -> String {
        let Some(d) = call.desc else {
            return format!("op {} ({} bytes): {}", call.op, call.extra.len(), hex::encode(&call.extra));
        };
        let desc = &self.descs[d];
        let mut parts: Vec<String> = desc.args.iter().zip(&call.args).filter(|(a, _)| !matches!(a.ty, Type::Const { .. })).map(|(a, v)| format!("{}={}", a.name, show_value(&a.ty, v))).collect();
        if !call.extra.is_empty() {
            parts.push(format!("+{}", hex::encode(&call.extra)));
        }
        format!("{}({})", desc.name, parts.join(", "))
    }
}

fn encode_value(ty: &Type, v: &Value, out: &mut Vec<u8>) {
    match (ty, v) {
        (Type::Int { size, .. } | Type::Flags { size, .. } | Type::Const { size, .. } | Type::Len { size, .. }, Value::Int(raw)) => {
            out.extend_from_slice(&raw.to_le_bytes()[..usize::from(*size)]);
        }
        (Type::Res { inline, .. }, Value::Res(raw, value)) => {
            out.push(*raw);
            if let Some(value) = value {
                out.extend_from_slice(&value.to_le_bytes()[..usize::from(*inline)]);
            }
        }
        (Type::Array { elem, .. }, Value::Array(items)) => {
            for item in items {
                encode_value(elem, item, out);
            }
        }
        (Type::Bytes { .. }, Value::Bytes(bytes)) => out.extend_from_slice(bytes),
        _ => {}
    }
}

fn show_value(ty: &Type, v: &Value) -> String {
    match (ty, v) {
        (Type::Int { values, names, .. }, Value::Int(raw)) => {
            let value = ty.int_value(*raw);
            match values.iter().position(|&x| x == value).and_then(|i| names.get(i)) {
                Some(name) => name.clone(),
                None if value < 10 => value.to_string(),
                None => format!("{value:#x}"),
            }
        }
        (Type::Flags { .. } | Type::Len { .. } | Type::Const { .. }, Value::Int(raw)) => format!("{raw:#x}"),
        (Type::Res { kind, slots, .. }, Value::Res(raw, None)) => format!("{kind}{}", raw % slots),
        (Type::Res { .. }, Value::Res(_, Some(value))) => format!("{value:#x}"),
        (Type::Array { elem, .. }, Value::Array(items)) => format!("[{}]", items.iter().map(|i| show_value(elem, i)).collect::<Vec<_>>().join(", ")),
        (Type::Bytes { .. }, Value::Bytes(bytes)) if bytes.is_empty() => "\"\"".into(),
        (Type::Bytes { .. }, Value::Bytes(bytes)) if bytes.len() <= 32 => format!("{} ({})", hex::encode(bytes), bytes.len()),
        (Type::Bytes { .. }, Value::Bytes(bytes)) => format!("{}… ({})", hex::encode(&bytes[..32]), bytes.len()),
        _ => "?".into(),
    }
}

/// Which slots of each kind hold something at a point of a program:
/// filled by an earlier call (`live`), closed since, or neither.
#[derive(Debug, Clone, Default)]
pub struct Slots {
    /// Live slots per kind, most recently filled last.
    pub live: BTreeMap<String, Vec<u8>>,
    pub closed: BTreeMap<String, Vec<u8>>,
}

impl Slots {
    pub fn after(calls: &Calls, program: &[Call]) -> Self {
        let mut s = Self::default();
        for c in program {
            s.apply(calls, c);
        }
        s
    }

    pub fn apply(&mut self, calls: &Calls, call: &Call) {
        for u in calls.uses(call) {
            let live = self.live.entry(u.kind.to_string()).or_default();
            let closed = self.closed.entry(u.kind.to_string()).or_default();
            match u.dir {
                Dir::In => {}
                Dir::Out => {
                    live.retain(|&s| s != u.slot);
                    live.push(u.slot);
                    closed.retain(|&s| s != u.slot);
                }
                Dir::Close => {
                    live.retain(|&s| s != u.slot);
                    if !closed.contains(&u.slot) {
                        closed.push(u.slot);
                    }
                }
            }
        }
    }

    pub fn live(&self, kind: &str) -> &[u8] {
        self.live.get(kind).map_or(&[], Vec::as_slice)
    }

    pub fn closed(&self, kind: &str) -> &[u8] {
        self.closed.get(kind).map_or(&[], Vec::as_slice)
    }
}

/// A program as lines (`show`); without descriptions, each call's op and
/// payload.
pub fn show(calls: Option<&Calls>, input: &[u8]) -> Vec<String> {
    match calls {
        Some(c) => c.show(&c.decode(input)),
        None => crate::calls(input).into_iter().map(|(op, p)| format!("op {op} ({} bytes): {}", p.len(), hex::encode(&p))).collect(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A small client: open a log, make an area on it, append (an LSN
    /// out), read from an LSN, close a log or an area, set names.
    pub fn sample() -> Calls {
        let descs: Vec<CallDesc> = serde_json::from_value(serde_json::json!([
            {"op": 0, "name": "open", "args": [
                {"name": "slot", "type": "res", "kind": "log", "dir": "out", "slots": 8},
                {"name": "mode", "type": "int", "size": 1, "min": 1, "max": 5, "modulo": true, "values": [1, 3], "names": ["new", "existing"]}]},
            {"op": 3, "name": "area", "args": [
                {"name": "log", "type": "res", "kind": "log", "slots": 8},
                {"name": "slot", "type": "res", "kind": "area", "dir": "out", "slots": 8},
                {"name": "size", "type": "int", "size": 4, "max": 1048575, "modulo": true}]},
            {"op": 4, "name": "append", "args": [
                {"name": "area", "type": "res", "kind": "area", "slots": 8},
                {"name": "flags", "type": "flags", "size": 1, "values": [1, 2, 4]},
                {"name": "into", "type": "res", "kind": "lsn", "dir": "out", "slots": 16},
                {"name": "undo", "type": "res", "kind": "lsn", "slots": 16, "inline": 8},
                {"name": "data", "type": "bytes", "max": 64}]},
            {"op": 6, "name": "read", "outputs": [{"kind": "lsn", "slot": 15}], "args": [
                {"name": "area", "type": "res", "kind": "area", "slots": 8},
                {"name": "first", "type": "res", "kind": "lsn", "slots": 16, "inline": 8}]},
            {"op": 7, "name": "close", "args": [
                {"name": "log", "type": "res", "kind": "log", "dir": "close", "slots": 8, "tag_mask": 128}]},
            {"op": 7, "name": "delete", "args": [
                {"name": "area", "type": "res", "kind": "area", "dir": "close", "slots": 8, "tag": 128, "tag_mask": 128}]},
            {"op": 8, "name": "tail", "args": [
                {"name": "log", "type": "res", "kind": "log", "slots": 8},
                {"name": "which", "type": "const", "size": 1, "value": 0, "modulo": 3}]},
            {"op": 8, "name": "base", "args": [
                {"name": "area", "type": "res", "kind": "area", "slots": 8},
                {"name": "which", "type": "const", "size": 1, "value": 1, "modulo": 3}]},
            {"op": 18, "name": "names", "args": [
                {"name": "log", "type": "res", "kind": "log", "slots": 8},
                {"name": "count", "type": "int", "size": 1, "min": 1, "max": 4, "modulo": true},
                {"name": "names", "type": "array", "len": "count", "elem": {"type": "int", "size": 1, "max": 7, "modulo": true}},
                {"name": "length", "type": "len", "size": 2, "of": ["count", "names", "length"]}]}
        ]))
        .unwrap();
        Calls::new(descs).unwrap()
    }

    #[test]
    fn programs_decode_and_encode_back() {
        let c = sample();
        let input = sequence(&[
            (0, vec![2, 2]),
            (3, vec![2, 1, 0, 0, 1, 0]),
            (4, vec![1, 2, 5, 0x83, 1, 0, 0, 0, 2, 0, 0, 0, b'h', b'i']),
            (6, vec![1, 5]),
            (7, vec![0x81]),
            (7, vec![2]),
            (8, vec![2, 3]),
            (8, vec![1, 4]),
            (18, vec![2, 6, 1, 2, 3, 9, 9, 0xee]),
            (99, vec![1, 2, 3]),
        ]);
        let p = c.decode(&input);
        assert_eq!(c.encode(&p), input, "decoding loses nothing");
        let names: Vec<_> = p.iter().map(|call| call.desc.map(|d| c.descs[d].name.as_str())).collect();
        assert_eq!(names, [Some("open"), Some("area"), Some("append"), Some("read"), Some("delete"), Some("close"), Some("tail"), Some("base"), Some("names"), None]);
        // Inline LSN: bit 7, then eight bytes.
        assert_eq!(p[2].args[3], Value::Res(0x83, Some(0x2_0000_0001)));
        // 6 % 4 + 1 = 3 names; the length (2 bytes) then one extra byte.
        assert_eq!(p[8].args[2], Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]));
        assert_eq!(p[8].extra, vec![0xee]);
        let lines = c.show(&p);
        assert_eq!(lines[0], "open(slot=log2, mode=existing)");
        assert_eq!(lines[2], "append(area=area1, flags=0x2, into=lsn5, undo=0x200000001, data=6869 (2))");
        assert_eq!(lines[4], "delete(area=area1)");
        assert_eq!(lines[9], "op 99 (3 bytes): 010203");
    }

    #[test]
    fn short_payloads_read_zeros() {
        let c = sample();
        let call = c.decode_call(3, &[1]);
        assert_eq!(call.args, vec![Value::Res(1, None), Value::Res(0, None), Value::Int(0)]);
        assert_eq!(c.encode_call(&call).1, vec![1, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn lens_measure_their_arguments() {
        let c = sample();
        let mut call = c.decode_call(18, &[0, 1, 4, 5, 0, 0]);
        c.fix_lens(&mut call);
        // count (1) + two names + the length itself (2).
        assert_eq!(call.args[3], Value::Int(5));
        call.args[1] = Value::Int(3);
        c.fit_arrays(&mut call, |_| Value::Int(7));
        assert_eq!(call.args[2], Value::Array(vec![Value::Int(4), Value::Int(5), Value::Int(7), Value::Int(7)]));
    }

    #[test]
    fn slots_follow_producers_and_closes() {
        let c = sample();
        let p = c.decode(&sequence(&[(0, vec![2, 0]), (0, vec![5, 0]), (3, vec![2, 1]), (7, vec![2]), (6, vec![1, 0])]));
        let s = Slots::after(&c, &p);
        assert_eq!(s.live("log"), [5]);
        assert_eq!(s.closed("log"), [2]);
        assert_eq!(s.live("area"), [1]);
        assert_eq!(s.live("lsn"), [15], "a fixed output");
        assert_eq!(c.producers["lsn"].iter().map(|&d| c.descs[d].op).collect::<Vec<_>>(), [4, 6]);
    }

    #[test]
    fn bad_descriptions_are_refused() {
        let bad = |args: serde_json::Value| Calls::new(serde_json::from_value(serde_json::json!([{"op": 1, "name": "x", "args": args}])).unwrap()).is_err();
        assert!(bad(serde_json::json!([{"name": "a", "type": "int", "size": 3}])));
        assert!(bad(serde_json::json!([{"name": "a", "type": "bytes", "max": 4}, {"name": "b", "type": "int", "size": 1}])));
        assert!(bad(serde_json::json!([{"name": "a", "type": "array", "len": "n", "elem": {"type": "int", "size": 1}}])));
        assert!(bad(serde_json::json!([{"name": "a", "type": "len", "size": 1, "of": ["b"]}])));
        assert!(!bad(serde_json::json!([{"name": "n", "type": "int", "size": 1}, {"name": "a", "type": "array", "len": "n", "elem": {"type": "int", "size": 1}}])));
    }
}

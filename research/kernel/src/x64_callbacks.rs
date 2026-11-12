//! Layout evidence from connected AMD64 callback instructions, not OS versions.
//!
//! These are deliberately strict compiler templates, decoded from named entry
//! points and their direct branch/call targets. Never search arbitrary bytes for
//! a mask or a loop constant: immediates/data may contain those byte sequences.
use vroom_kernel::symbols::Symbols;
use vroom_kernel::windows::VirtualMemory;

pub(super) struct Layout {
    pub slots: u32,
    pub bits: u8,
    pub function: u64,
    pub context: Option<u64>,
}

struct Code<'a, M: VirtualMemory> {
    memory: &'a M,
    address: u64,
}
impl<'a, M: VirtualMemory> Code<'a, M> {
    fn at(memory: &'a M, address: u64) -> Self {
        Self { memory, address }
    }
    fn take(&mut self, n: usize) -> Result<Vec<u8>, String> {
        let next = self
            .address
            .checked_add(n as u64)
            .ok_or("callback instruction address overflow")?;
        let mut bytes = vec![0; n];
        self.memory
            .read(self.address, &mut bytes)
            .map_err(|e| format!("callback instruction at {:#x}: {e}", self.address))?;
        self.address = next;
        Ok(bytes)
    }
    fn expect(&mut self, expected: &[u8]) -> Result<(), String> {
        let at = self.address;
        if self.take(expected.len())? != expected {
            return Err(format!(
                "unrecognized AMD64 callback instruction template at {at:#x}; no layout inferred"
            ));
        }
        Ok(())
    }
    fn byte(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn relative(&mut self, n: usize) -> Result<u64, String> {
        let b = self.take(n)?;
        let displacement = if n == 1 {
            b[0] as i8 as i64
        } else {
            i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as i64
        };
        self.address
            .checked_add_signed(displacement)
            .ok_or("callback branch target overflow".into())
    }
    fn branch(&mut self, opcode: &[u8], n: usize) -> Result<u64, String> {
        self.expect(opcode)?;
        self.relative(n)
    }
}
fn equal(actual: u64, expected: u64, what: &str) -> Result<(), String> {
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "AMD64 callback {what} does not match instruction evidence"
        ))
    }
}

fn accessor(m: &impl VirtualMemory, s: &Symbols) -> Result<u64, String> {
    let mut c = Code::at(
        m,
        s.resolve("ExGetCallBackBlockRoutine")
            .map_err(|e| e.to_string())?,
    );
    // MOV RAX,[RCX+disp]; RET. This is the entire accessor, not an embedded load.
    c.expect(&[0x48, 0x8b])?;
    let offset = match c.byte()? {
        0x01 => 0,
        0x41 => c.byte()? as i8 as i64,
        0x81 => {
            let b = c.take(4)?;
            i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as i64
        }
        _ => return Err("unrecognized AMD64 callback routine accessor".into()),
    };
    c.expect(&[0xc3])?;
    if !(0..=64).contains(&offset) || offset & 7 != 0 {
        return Err("invalid callback accessor displacement".into());
    }
    Ok(offset as u64)
}

fn fast_reference(m: &impl VirtualMemory, s: &Symbols) -> Result<u8, String> {
    let base = s
        .resolve("ExReferenceCallBackBlock")
        .map_err(|e| e.to_string())?;
    let mut c = Code::at(m, base);
    // Preserve RBX/RDI; RDI=RCX; load the exact fast-ref at [RCX] into RBX.
    c.expect(&[
        0x48, 0x89, 0x5c, 0x24, 0x10, 0x57, 0x48, 0x83, 0xec, 0x20, 0x48, 0x8b, 0xf9, 0x0f, 0x0d,
        0x09, 0x48, 0x8b, 0x19, 0xf6, 0xc3,
    ])?;
    let mask = c.byte()?;
    let bits = mask.count_ones() as u8;
    if !(1..=4).contains(&bits) || mask as u16 != (1u16 << bits) - 1 {
        return Err("invalid callback reference-count mask".into());
    }
    let ready = c.branch(&[0x74], 1)?;
    let retry = c.address;
    c.expect(&[
        0x48, 0x8d, 0x53, 0xff, 0x48, 0x8b, 0xc3, 0xf0, 0x48, 0x0f, 0xb1, 0x11,
    ])?;
    equal(
        c.branch(&[0x74], 1)?,
        ready,
        "atomic-reference success branch",
    )?;
    c.expect(&[0x48, 0x8b, 0xd8, 0xa8])?;
    equal(c.byte()? as u64, mask as u64, "retry mask")?;
    equal(
        c.branch(&[0x75], 1)?,
        retry,
        "atomic-reference retry branch",
    )?;
    equal(c.address, ready, "reference-count zero path")?;
    c.expect(&[0x48, 0x85, 0xdb])?;
    let _null = c.branch(&[0x0f, 0x84], 4)?;
    c.expect(&[0x48, 0x8b, 0xc3, 0x83, 0xe0])?;
    equal(c.byte()? as u64, mask as u64, "reference-count extraction")?;
    let fast = c.branch(&[0x0f, 0x85], 4)?;
    if fast.checked_sub(base).map_or(true, |n| n > 2048) {
        return Err("callback reference fast path outside bounded helper".into());
    }
    let mut c = Code::at(m, fast);
    c.expect(&[0x48, 0x83, 0xe3])?;
    equal(c.byte()? as u64, (!mask) as u64, "pointer mask complement")?;
    // Strip the same low bits on the value actually returned in RAX.
    c.expect(&[0x48, 0x83, 0xf8, 0x01])?;
    let _one = c.branch(&[0x74], 1)?;
    c.expect(&[
        0x48, 0x8b, 0xc3, 0x48, 0x8b, 0x5c, 0x24, 0x38, 0x48, 0x83, 0xc4, 0x20, 0x5f, 0xc3,
    ])?;
    Ok(bits)
}

fn allocation(m: &impl VirtualMemory, address: u64, function: u64) -> Result<u64, String> {
    let mut c = Code::at(m, address);
    // The registration path passes (callback, flags) in RCX/RDX. Preserve both
    // through the allocation call; validate their stores into the returned block.
    c.expect(&[
        0x48, 0x89, 0x5c, 0x24, 0x08, 0x57, 0x48, 0x83, 0xec, 0x20, 0x48, 0x8b, 0xda, 0x48, 0x8b,
        0xf9, 0xba,
    ])?;
    let size = c.take(4)?;
    let size = u32::from_le_bytes([size[0], size[1], size[2], size[3]]);
    if !(16..=128).contains(&size) {
        return Err("unexpected callback allocation size".into());
    }
    c.expect(&[0x41, 0xb8])?;
    let _tag = c.take(4)?;
    c.expect(&[0x8d, 0x4a, 0x28])?;
    let _pool = c.branch(&[0xe8], 4)?;
    c.expect(&[0x48, 0x85, 0xc0])?;
    let done = c.branch(&[0x74], 1)?;
    c.expect(&[0x48, 0x83, 0x20, 0x00, 0x48, 0x89, 0x78])?;
    equal(c.byte()? as u64, function, "allocated function field")?;
    c.expect(&[0x48, 0x89, 0x58])?;
    let context = c.byte()? as u64;
    if context & 7 != 0
        || context == function
        || context < 8
        || context + 8 > size as u64
        || function + 8 > size as u64
    {
        return Err("invalid code-derived callback block fields".into());
    }
    equal(c.address, done, "allocation failure branch")?;
    c.expect(&[
        0x48, 0x8b, 0x5c, 0x24, 0x30, 0x48, 0x83, 0xc4, 0x20, 0x5f, 0xc3,
    ])?;
    Ok(context)
}

pub(super) fn derive(m: &impl VirtualMemory, s: &Symbols) -> Result<Layout, String> {
    let function = accessor(m, s)?;
    let bits = fast_reference(m, s)?;
    let table = s
        .resolve("PspCreateProcessNotifyRoutine")
        .map_err(|e| e.to_string())?;
    let reference = s
        .resolve("ExReferenceCallBackBlock")
        .map_err(|e| e.to_string())?;
    let entry = s
        .resolve("PspSetCreateProcessNotifyRoutine")
        .map_err(|e| e.to_string())?;
    let mut c = Code::at(m, entry);
    // Validate from the named entry, including where the original RCX callback
    // is saved. This establishes instruction boundaries for the entire loop.
    c.expect(&[
        0x48, 0x89, 0x5c, 0x24, 0x08, 0x48, 0x89, 0x6c, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18,
        0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x48, 0x83, 0xec, 0x20, 0x8b, 0xf2,
        0x8b, 0xda, 0x83, 0xe6, 0x02, 0x4c, 0x8b, 0xf9, 0xf6, 0xc2, 0x01,
    ])?;
    let registration = c.branch(&[0x0f, 0x84], 4)?;
    c.expect(&[0x65, 0x48, 0x8b, 0x2c, 0x25])?;
    let _thread_offset = c.take(4)?;
    equal(
        c.branch(&[0x4c, 0x8d, 0x2d], 4)?,
        table,
        "removal table RIP reference",
    )?;
    c.expect(&[0x83, 0xc8, 0xff, 0x66, 0x01, 0x85])?;
    let _critical_offset = c.take(4)?;
    c.expect(&[0x90, 0x45, 0x33, 0xf6])?; // counter starts at zero.
    let compare = c.address;
    c.expect(&[0x41, 0x83, 0xfe])?;
    let slots = c.byte()? as u32;
    if !(1..=127).contains(&slots) {
        return Err("unsupported callback array bound encoding".into());
    }
    let exhausted = c.branch(&[0x73], 1)?;
    // RCX = table + R14 * 8, not some unrelated loop with the same bound.
    c.expect(&[
        0x4e, 0x8d, 0x24, 0xf5, 0, 0, 0, 0, 0x4d, 0x03, 0xe5, 0x49, 0x8b, 0xcc,
    ])?;
    equal(c.branch(&[0xe8], 4)?, reference, "reference helper call")?;
    c.expect(&[0x48, 0x8b, 0xf8, 0x48, 0x85, 0xc0])?;
    let next = c.branch(&[0x74], 1)?;
    c.expect(&[0x83, 0xe3, 0xfe, 0x4c, 0x39, 0x78])?;
    equal(c.byte()? as u64, function, "removal function comparison")?;
    let _found = c.branch(&[0x0f, 0x84], 4)?;
    c.expect(&[0x48, 0x8b, 0xd7, 0x49, 0x8b, 0xcc])?;
    let dereference = c.branch(&[0xe8], 4)?;
    if let Ok(expected) = s.resolve("ExDereferenceCallBackBlock") {
        equal(dereference, expected, "dereference helper call")?;
    }
    equal(c.address, next, "null-block skip")?;
    c.expect(&[0x41, 0xff, 0xc6])?;
    equal(c.branch(&[0xeb], 1)?, compare, "removal loop back edge")?;
    equal(c.address, exhausted, "removal loop exit")?;

    // Follow the explicit registration path, including the allocation-result
    // test, to a second independent loop over the exact same table.
    if registration.checked_sub(entry).map_or(true, |n| n > 512) {
        return Err("registration path outside bounded entry".into());
    }
    let mut c = Code::at(m, registration);
    c.expect(&[0x85, 0xf6])?;
    let legacy = c.branch(&[0x74], 1)?;
    c.expect(&[0xba, 0x20, 0, 0, 0])?;
    let verify_at = c.address;
    let _verify = c.branch(&[0xe8], 4)?;
    c.expect(&[0x85, 0xc0])?;
    let allocate = c.branch(&[0x75], 1)?;
    c.expect(&[0xb8, 0x22, 0, 0, 0xc0])?;
    let _denied = c.branch(&[0xeb], 1)?;
    equal(c.address, legacy, "registration legacy path")?;
    c.expect(&[0x33, 0xd2])?;
    equal(
        c.branch(&[0xeb], 1)?,
        verify_at,
        "registration access-check path",
    )?;
    equal(c.address, allocate, "registration allocation path")?;
    c.expect(&[0x48, 0x8b, 0xd3, 0x49, 0x8b, 0xcf])?;
    let allocator = c.branch(&[0xe8], 4)?;
    if let Ok(expected) = s.resolve("ExAllocateCallBack") {
        equal(allocator, expected, "allocation helper call")?;
    }
    c.expect(&[0x48, 0x8b, 0xf8, 0x48, 0x85, 0xc0])?;
    let insert = c.branch(&[0x0f, 0x85], 4)?;
    if insert.abs_diff(entry) > 2 * 1024 * 1024 || allocator.abs_diff(entry) > 16 * 1024 * 1024 {
        return Err("callback registration target outside bounded kernel code".into());
    }
    let context = allocation(m, allocator, function)?;
    let mut c = Code::at(m, insert);
    c.expect(&[0x33, 0xdb])?;
    equal(
        c.branch(&[0x4c, 0x8d, 0x2d], 4)?,
        table,
        "insertion table RIP reference",
    )?;
    let compare = c.address;
    c.expect(&[0x83, 0xfb])?;
    equal(
        c.byte()? as u64,
        slots as u64,
        "independent insertion/removal bounds",
    )?;
    let exhausted = c.branch(&[0x73], 1)?;
    c.expect(&[
        0x48, 0x8d, 0x0c, 0xdd, 0, 0, 0, 0, 0x45, 0x33, 0xc0, 0x49, 0x03, 0xcd, 0x48, 0x8b, 0xd7,
    ])?;
    let exchange = c.branch(&[0xe8], 4)?;
    if let Ok(expected) = s.resolve("ExCompareExchangeCallBack") {
        equal(exchange, expected, "exchange helper call")?;
    }
    c.expect(&[0x84, 0xc0])?;
    let success = c.branch(&[0x75], 1)?;
    c.expect(&[0xff, 0xc3])?;
    equal(c.branch(&[0xeb], 1)?, compare, "insertion loop back edge")?;
    equal(c.address, success, "insertion success branch")?;
    if exhausted <= success || exhausted.abs_diff(insert) > 256 {
        return Err("invalid insertion loop exit".into());
    }
    Ok(Layout {
        slots,
        bits,
        function,
        context: Some(context),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    struct Memory(BTreeMap<u64, Vec<u8>>);
    impl VirtualMemory for Memory {
        fn read(&self, address: u64, bytes: &mut [u8]) -> vroom_kernel::Result<()> {
            let (&base, data) = self
                .0
                .range(..=address)
                .next_back()
                .ok_or_else(|| vroom_kernel::Error::Invalid("unmapped fixture".into()))?;
            let at = (address - base) as usize;
            bytes.copy_from_slice(
                data.get(at..at + bytes.len())
                    .ok_or_else(|| vroom_kernel::Error::Invalid("fixture boundary".into()))?,
            );
            Ok(())
        }
    }
    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
    fn rel(b: &mut [u8], base: u64, at: usize, length: usize, target: u64) {
        let d = target as i128 - (base + at as u64 + length as u64) as i128;
        b[at + length - 4..at + length].copy_from_slice(&(i32::try_from(d).unwrap()).to_le_bytes());
    }
    fn symbols(names: impl IntoIterator<Item = (String, u64)>) -> Symbols {
        Symbols {
            names: names.into_iter().collect(),
            functions: vec![],
            addresses: BTreeMap::new(),
            types: BTreeMap::new(),
            path: Default::default(),
            callback_slots: None,
            fast_ref_bits: None,
        }
    }
    fn fixture() -> (Memory, Symbols) {
        // Instruction templates are synthetic: all direct call/table addresses
        // are relocated to a sparse, in-memory fixture; no Windows binary needed.
        let mut entry=hex("48895c240848896c241048897424185741544155415641574883ec208bf28bda83e6024c8bf9f6c2010f848700000065488b2c25880100004c8d2d0000000083c8ff660185e4010000904533f64183fe4073384e8d24f5000000004d03e5498bcce800000000488bf84885c0741883e3fe4c3978080f849d280a00488bd7498bcce80000000041ffc6ebc2488bcde891797bffb87a0000c0488b5c2450488b6c2458488b7424604883c420415f415e415d415c5fc3cc85f67415ba20000000e8b0fca4ff85c0750bb8220000c0ebc933d2ebec488bd3498bcfe800000000488bf84885c00f8500000000b89a0000c0eba7");
        rel(&mut entry, 0x1000, 0x38, 7, 0x4000);
        rel(&mut entry, 0x1000, 0x61, 5, 0x2000);
        rel(&mut entry, 0x1000, 0xbf, 5, 0x7000);
        rel(&mut entry, 0x1000, 0xd9, 5, 0x5000);
        rel(&mut entry, 0x1000, 0xe4, 6, 0x6000);
        let mut reference=hex("48895c2410574883ec20488bf90f0d09488b19f6c30f7415488d53ff488bc3f0480fb1117407488bd8a80f75eb4885db0f84a2000000488bc383e00f0f85ec000000");
        rel(&mut reference, 0x2000, 0x3c, 6, 0x212e);
        let fast = hex("4883e3f04883f8017433488bc3488b5c24384883c4205fc3");
        let allocator=hex("48895c2408574883ec20488bda488bf9ba1800000041b8436272628d4a28e8ed050e004885c0740c488320004889780848895810488b5c24304883c4205fc3");
        let mut insert=hex("33db4c8d2d0000000083fb407351488d0cdd000000004533c04903cd488bd7e83c1d8dff84c07504ffc3ebdd");
        rel(&mut insert, 0x6000, 2, 7, 0x4000);
        rel(&mut insert, 0x6000, 0x1f, 5, 0x7000);
        let memory = Memory(BTreeMap::from([
            (0x1000, entry),
            (0x2000, reference),
            (0x212e, fast),
            (0x3000, hex("488b4108c3")),
            (0x5000, allocator),
            (0x6000, insert),
        ]));
        let syms = symbols([
            ("PspSetCreateProcessNotifyRoutine".into(), 0x1000),
            ("ExReferenceCallBackBlock".into(), 0x2000),
            ("ExGetCallBackBlockRoutine".into(), 0x3000),
            ("PspCreateProcessNotifyRoutine".into(), 0x4000),
        ]);
        (memory, syms)
    }
    #[test]
    fn derives_all_missing_private_fields_from_connected_instructions() {
        let (m, s) = fixture();
        let l = derive(&m, &s).unwrap();
        assert_eq!(
            (l.slots, l.bits, l.function, l.context),
            (64, 4, 8, Some(16))
        );
        // The bound is extracted and independently checked, not fixed at 64.
        let (mut m, s) = fixture();
        m.0.get_mut(&0x1000).unwrap()[0x50] = 32;
        m.0.get_mut(&0x6000).unwrap()[0xb] = 32;
        assert_eq!(derive(&m, &s).unwrap().slots, 32);
    }
    #[test]
    fn enumerates_with_no_private_pdb_types_and_cross_checks_partial_types() {
        let (mut m, mut s) = fixture();
        let mut table = vec![0; 64 * 8];
        table[..8].copy_from_slice(&0x800fu64.to_le_bytes());
        let mut block = vec![0; 24];
        block[8..16].copy_from_slice(&0x9000u64.to_le_bytes());
        block[16..24].copy_from_slice(&0x1234u64.to_le_bytes());
        m.0.insert(0x4000, table);
        m.0.insert(0x8000, block);
        let rows = super::super::callbacks(&m, &s, &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].block, rows[0].function, rows[0].context),
            (0x8000, 0x9000, Some(0x1234))
        );
        assert!(rows[0].layout_source.contains("validated AMD64"));
        s.callback_slots = Some(32);
        assert!(super::super::callbacks(&m, &s, &[])
            .unwrap_err()
            .contains("conflict"));
        s.callback_slots = None;
        s.fast_ref_bits = Some(3);
        assert!(super::super::callbacks(&m, &s, &[])
            .unwrap_err()
            .contains("conflict"));
        s.fast_ref_bits = None;
        s.types.insert(
            "_EX_CALLBACK_ROUTINE_BLOCK".into(),
            BTreeMap::from([("Function".into(), 16)]),
        );
        assert!(super::super::callbacks(&m, &s, &[])
            .unwrap_err()
            .contains("conflict"));
    }
    #[test]
    fn altered_bounds_addresses_stride_masks_and_fields_fail_closed() {
        for (base, offset, value) in [
            (0x1000, 0x50, 63),
            (0x6000, 0xb, 63), // independent bounds disagree
            (0x1000, 0x3b, 0),
            (0x1000, 0x57, 0xed), // unrelated table or stride
            (0x1000, 0x8a, 0xc1),
            (0x1000, 0x62, 0), // wrong back edge/reference helper
            (0x2000, 0x15, 7),
            (0x212e, 3, 0xf8), // inconsistent extraction/pointer masks
            (0x3000, 3, 16),
            (0x5000, 0x33, 8),
        ] {
            // function mismatch/overlapping context
            let (mut m, s) = fixture();
            let original = m.0[&base][offset];
            let value = if original == value { value ^ 1 } else { value };
            m.0.get_mut(&base).unwrap()[offset] = value;
            assert!(
                derive(&m, &s).is_err(),
                "accepted mutation {base:#x}+{offset:#x}"
            );
        }
    }
    #[test]
    fn embedded_constants_or_mid_instruction_templates_are_not_evidence() {
        let (mut m, s) = fixture();
        m.0.get_mut(&0x1000).unwrap().insert(0, 0x90);
        assert!(derive(&m, &s).is_err());
        let (mut m, s) = fixture();
        m.0.get_mut(&0x2000).unwrap().truncate(32);
        assert!(derive(&m, &s).is_err());
    }
    #[test]
    #[ignore = "requires external live diagnostic snapshots and exact-build cold instruction blocks"]
    fn optional_offline_live_diagnostics() {
        let path =
            std::env::var("VROOM_X64_CALLBACK_DIAGNOSTICS").expect("set the diagnostic JSON path");
        let path = std::path::Path::new(&path);
        let dir = path.parent().unwrap();
        let j: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let mut names: BTreeMap<String, u64> = BTreeMap::new();
        let mut memory = BTreeMap::new();
        for name in [
            "PspSetCreateProcessNotifyRoutine",
            "ExReferenceCallBackBlock",
            "ExGetCallBackBlockRoutine",
            "PspCreateProcessNotifyRoutine",
        ] {
            let address = j[name]["symbol"]["address"].as_u64().unwrap();
            names.insert(name.into(), address);
            memory.insert(
                address,
                std::fs::read(dir.join(format!("{name}.bin"))).unwrap(),
            );
        }
        let entry = names["PspSetCreateProcessNotifyRoutine"];
        let code = &memory[&entry];
        let target = |at: usize, len: usize| {
            (entry + at as u64 + len as u64)
                .checked_add_signed(i32::from_le_bytes(
                    code[at + len - 4..at + len].try_into().unwrap(),
                ) as i64)
                .unwrap()
        };
        let allocator = target(0xd9, 5);
        let insert = target(0xe4, 6);
        memory.insert(
            allocator,
            std::fs::read(dir.join("x64-allocate.bin")).unwrap(),
        );
        memory.insert(
            insert,
            std::fs::read(dir.join("x64-insert-cold.bin")).unwrap(),
        );
        let identity = &j["kernel"]["pdb"];
        let identity = vroom_kernel::windows::PdbIdentity {
            name: identity["name"].as_str().unwrap().into(),
            guid: identity["guid"].as_str().unwrap().into(),
            age: identity["age"].as_u64().unwrap() as u32,
        };
        let syms = Symbols::load(
            std::path::Path::new(j["symbols"]["path"].as_str().unwrap()),
            &identity,
            j["kernel"]["base"].as_u64().unwrap(),
        )
        .unwrap();
        let layout = derive(&Memory(memory), &syms).unwrap();
        assert_eq!(
            (layout.slots, layout.bits, layout.function, layout.context),
            (64, 4, 8, Some(16))
        );
    }
}

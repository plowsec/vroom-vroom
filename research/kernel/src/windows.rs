//! Bounded PE inspection over guest virtual memory. No build-specific offsets.
use crate::arm64::AddressSpace;
use crate::{Error, PhysicalMemory, Result};
use serde::Serialize;
use std::collections::BTreeMap;

pub trait VirtualMemory {
    fn read(&self, address: u64, data: &mut [u8]) -> Result<()>;
    fn u64(&self, address: u64) -> Result<u64> {
        let mut data = [0; 8];
        self.read(address, &mut data)?;
        Ok(u64::from_le_bytes(data))
    }
}
impl<M: PhysicalMemory + ?Sized> VirtualMemory for AddressSpace<'_, M> {
    fn read(&self, address: u64, data: &mut [u8]) -> Result<()> {
        AddressSpace::read(self, address, data)
    }
}
fn bad(s: &str) -> Error {
    Error::Invalid(s.into())
}
fn u16at(b: &[u8], n: usize) -> u16 {
    u16::from_le_bytes([b[n], b[n + 1]])
}
fn u32at(b: &[u8], n: usize) -> u32 {
    u32::from_le_bytes([b[n], b[n + 1], b[n + 2], b[n + 3]])
}

#[derive(Debug, Clone, Serialize)]
pub struct PdbIdentity {
    pub name: String,
    pub guid: String,
    pub age: u32,
}
impl PdbIdentity {
    pub fn key(&self) -> String {
        format!("{}{:X}", self.guid, self.age)
    }
    pub fn url(&self) -> String {
        format!(
            "https://msdl.microsoft.com/download/symbols/{}/{}/{}",
            self.name,
            self.key(),
            self.name
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Image {
    pub base: u64,
    pub size: u32,
    pub timestamp: u32,
    pub name: String,
    pub pdb: Option<PdbIdentity>,
    pub exports: BTreeMap<String, u64>,
    /// Original loaded base-relocation directory, retained for analysis copies.
    pub relocation_directory: (u32, u32),
}

impl Image {
    /// Reconstruct a PE file with section file offsets equal to loaded RVAs.
    /// Relocations are already applied; preserve the runtime image base.
    /// Unreadable pages are explicitly reported, never presented as real code.
    pub fn export(
        &self,
        memory: &impl VirtualMemory,
        allow_missing: bool,
    ) -> Result<(Vec<u8>, Vec<u32>)> {
        if !(4096..=64 * 1024 * 1024).contains(&self.size)
            || self.base.checked_add(self.size as u64).is_none()
        {
            return Err(bad("invalid analysis export range (limit 64 MiB)"));
        }
        let mut image = vec![0; self.size as usize];
        let mut missing = Vec::new();
        for (i, page) in image.chunks_mut(4096).enumerate() {
            if memory.read(self.base + i as u64 * 4096, page).is_err() {
                if !allow_missing {
                    return Err(bad(
                        "module contains unreadable pages; an explicit partial export is required",
                    ));
                }
                page.fill(0);
                missing.push((i * 4096) as u32);
            }
        }
        if missing.contains(&0) {
            return Err(bad("module header is unreadable"));
        }
        let pe = u32at(&image, 0x3c) as usize;
        if !(64..=4096).contains(&pe) || pe + 264 > image.len() {
            return Err(bad("invalid export PE header offset"));
        }
        let optional = pe + 24;
        let sections = u16at(&image, pe + 6) as usize;
        if &image[..2] != b"MZ"
            || &image[pe..pe + 4] != b"PE\0\0"
            || u16at(&image, pe + 4) != 0xaa64
            || u16at(&image, optional) != 0x20b
            || u16at(&image, pe + 20) < 240
            || !(1..=96).contains(&sections)
            || u32at(&image, optional + 56) != self.size
        {
            return Err(bad("invalid or changed export PE headers"));
        }
        let table = optional + u16at(&image, pe + 20) as usize;
        if table + sections * 40 > image.len()
            || missing
                .iter()
                .any(|&p| (p as usize) < table + sections * 40)
        {
            return Err(bad("incomplete section headers"));
        }
        image[optional + 24..optional + 32].copy_from_slice(&self.base.to_le_bytes());
        // Debug directory file pointers still describe the original disk file.
        // In this reconstructed image every file offset equals its loaded RVA.
        let debug_rva = u32at(&image, optional + 112 + 6 * 8) as usize;
        let debug_size = u32at(&image, optional + 116 + 6 * 8) as usize;
        if debug_size > 28 * 128 || debug_rva + debug_size > image.len() {
            return Err(bad("invalid export debug directory"));
        }
        for off in (debug_rva..debug_rva + debug_size).step_by(28) {
            if off + 28 > debug_rva + debug_size {
                return Err(bad("incomplete export debug directory"));
            }
            let rva = u32at(&image, off + 20);
            image[off + 24..off + 28].copy_from_slice(&rva.to_le_bytes());
        }
        // No on-disk certificate or relocation processing for a loaded image.
        image[optional + 112 + 4 * 8..optional + 112 + 6 * 8].fill(0);
        let flags = u16at(&image, pe + 22) | 1;
        image[pe + 22..pe + 24].copy_from_slice(&flags.to_le_bytes());
        let dll = u16at(&image, optional + 70) & !0x40;
        image[optional + 70..optional + 72].copy_from_slice(&dll.to_le_bytes());
        let alignment = u32at(&image, optional + 36);
        if alignment == 0 || !alignment.is_power_of_two() || alignment > 65536 {
            return Err(bad("invalid PE file alignment"));
        }
        for i in 0..sections {
            let off = table + i * 40;
            let va = u32at(&image, off + 12);
            let size = u32at(&image, off + 8);
            let raw_size = size
                .checked_add(alignment - 1)
                .ok_or_else(|| bad("section size overflow"))?
                & !(alignment - 1);
            if va as u64 + raw_size as u64 > self.size as u64 {
                return Err(bad("section exceeds loaded image"));
            }
            image[off + 16..off + 20].copy_from_slice(&raw_size.to_le_bytes());
            image[off + 20..off + 24].copy_from_slice(&va.to_le_bytes());
        }
        Ok((image, missing))
    }

    pub fn read(memory: &impl VirtualMemory, base: u64) -> Result<Self> {
        let mut dos = [0; 64];
        memory.read(base, &mut dos)?;
        if &dos[..2] != b"MZ" {
            return Err(bad("not a PE image"));
        }
        let pe = u32at(&dos, 0x3c) as u64;
        if !(64..=4096).contains(&pe) {
            return Err(bad("invalid PE header offset"));
        }
        let mut header = [0; 264];
        memory.read(
            base.checked_add(pe)
                .ok_or_else(|| bad("PE address overflow"))?,
            &mut header,
        )?;
        if &header[..4] != b"PE\0\0" || u16at(&header, 4) != 0xaa64 || u16at(&header, 24) != 0x20b {
            return Err(bad("expected ARM64 PE32+ image"));
        }
        if u16at(&header, 20) < 240 || u16at(&header, 6) == 0 || u16at(&header, 6) > 96 {
            return Err(bad("invalid PE optional header or section count"));
        }
        let size = u32at(&header, 24 + 56);
        if !(4096..=1 << 30).contains(&size) || base.checked_add(size as u64).is_none() {
            return Err(bad("invalid PE image size"));
        }
        let mut image = Self {
            base,
            size,
            timestamp: u32at(&header, 8),
            name: String::new(),
            pdb: None,
            exports: BTreeMap::new(),
            relocation_directory: (
                u32at(&header, 24 + 112 + 5 * 8),
                u32at(&header, 24 + 116 + 5 * 8),
            ),
        };
        let directories = u32at(&header, 24 + 108);
        if directories > 0 {
            let export_rva = u32at(&header, 24 + 112);
            let export_size = u32at(&header, 24 + 116);
            if export_rva != 0 && export_size >= 40 {
                let mut e = [0; 40];
                image.read_rva(memory, export_rva, &mut e)?;
                image.name = image.string(memory, u32at(&e, 12), 512)?;
                let count = u32at(&e, 24);
                let functions = u32at(&e, 20);
                if count > 100_000 || functions > 100_000 {
                    return Err(bad("excessive PE exports"));
                }
                let mut names = vec![0; count as usize * 4];
                image.read_rva(memory, u32at(&e, 32), &mut names)?;
                let mut ordinals = vec![0; count as usize * 2];
                image.read_rva(memory, u32at(&e, 36), &mut ordinals)?;
                let mut addresses = vec![0; functions as usize * 4];
                image.read_rva(memory, u32at(&e, 28), &mut addresses)?;
                for i in 0..count as usize {
                    let ord = u16at(&ordinals, i * 2) as usize;
                    if ord >= functions as usize {
                        return Err(bad("PE export ordinal out of range"));
                    }
                    let rva = u32at(&addresses, ord * 4);
                    if rva == 0 || rva >= size {
                        continue;
                    }
                    if (export_rva as u64..export_rva as u64 + export_size as u64)
                        .contains(&(rva as u64))
                    {
                        continue;
                    }
                    image.exports.insert(
                        image.string(memory, u32at(&names, i * 4), 1024)?,
                        base + rva as u64,
                    );
                }
            }
        }
        if directories > 6 {
            let debug_rva = u32at(&header, 24 + 112 + 6 * 8);
            let debug_size = u32at(&header, 24 + 116 + 6 * 8);
            if debug_size > 28 * 128 {
                return Err(bad("excessive PE debug entries"));
            }
            if debug_rva != 0 {
                for i in 0..debug_size / 28 {
                    let mut d = [0; 28];
                    image.read_rva(
                        memory,
                        debug_rva
                            .checked_add(i * 28)
                            .ok_or_else(|| bad("debug RVA overflow"))?,
                        &mut d,
                    )?;
                    if u32at(&d, 12) != 2 {
                        continue;
                    }
                    let len = u32at(&d, 16);
                    if !(25..=4096).contains(&len) {
                        continue;
                    }
                    let mut cv = vec![0; len as usize];
                    image.read_rva(memory, u32at(&d, 20), &mut cv)?;
                    if &cv[..4] != b"RSDS" {
                        continue;
                    }
                    let end = cv[24..]
                        .iter()
                        .position(|&c| c == 0)
                        .ok_or_else(|| bad("unterminated PDB name"))?;
                    let path = std::str::from_utf8(&cv[24..24 + end])
                        .map_err(|_| bad("invalid PDB name"))?;
                    let name = path.rsplit(['\\', '/']).next().unwrap_or("");
                    if name.is_empty()
                        || !name.ends_with(".pdb")
                        || !name
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
                    {
                        return Err(bad("unsafe PDB basename"));
                    }
                    let guid = format!(
                        "{:08X}{:04X}{:04X}{}",
                        u32at(&cv, 4),
                        u16at(&cv, 8),
                        u16at(&cv, 10),
                        cv[12..20]
                            .iter()
                            .map(|b| format!("{b:02X}"))
                            .collect::<String>()
                    );
                    image.pdb = Some(PdbIdentity {
                        name: name.into(),
                        guid,
                        age: u32at(&cv, 20),
                    });
                    break;
                }
            }
        }
        Ok(image)
    }

    pub fn read_rva(&self, memory: &impl VirtualMemory, rva: u32, data: &mut [u8]) -> Result<()> {
        if rva as u64 + data.len() as u64 > self.size as u64 {
            return Err(bad("PE RVA outside image"));
        }
        memory.read(self.base + rva as u64, data)
    }
    fn string(&self, memory: &impl VirtualMemory, rva: u32, limit: usize) -> Result<String> {
        let mut bytes = Vec::new();
        for i in 0..limit {
            let mut b = [0];
            self.read_rva(
                memory,
                rva.checked_add(i as u32)
                    .ok_or_else(|| bad("string RVA overflow"))?,
                &mut b,
            )?;
            if b[0] == 0 {
                return String::from_utf8(bytes).map_err(|_| bad("invalid PE string"));
            }
            bytes.push(b[0]);
        }
        Err(bad("unterminated PE string"))
    }
}

/// VBAR normally points inside nt. Bound the scan and verify the export name.
pub fn discover_kernel(memory: &impl VirtualMemory, anchors: &[u64]) -> Result<Image> {
    for &anchor in anchors {
        if anchor >> 48 != 0xffff {
            continue;
        }
        for offset in 0..4096u64 {
            let Some(base) = (anchor & !0xffff).checked_sub(offset * 65536) else {
                break;
            };
            let mut magic = [0; 2];
            if memory.read(base, &mut magic).is_err() || &magic != b"MZ" {
                continue;
            }
            if let Ok(image) = Image::read(memory, base) {
                if image.name.eq_ignore_ascii_case("ntoskrnl.exe")
                    || image.name.eq_ignore_ascii_case("ntkrnlmp.exe")
                {
                    return Ok(image);
                }
            }
        }
    }
    Err(bad(
        "nt kernel not found within 256 MiB below VBAR/PC; Windows may still be booting",
    ))
}

#[derive(Debug, Clone, Serialize)]
pub struct Module {
    pub base: u64,
    pub size: u32,
    pub name: String,
    pub path: String,
}

/// Field offsets must come from the matching kernel PDB.
#[derive(Debug, Clone)]
pub struct ModuleLayout {
    pub links: u64,
    pub base: u64,
    pub size: u64,
    pub name: u64,
    pub path: u64,
}

pub fn modules(
    memory: &impl VirtualMemory,
    head: u64,
    layout: &ModuleLayout,
) -> Result<Vec<Module>> {
    let mut next = memory.u64(head)?;
    let mut previous = head;
    let mut seen = std::collections::HashSet::new();
    let mut modules = Vec::new();
    while next != head {
        if next == 0 || next & 7 != 0 || !seen.insert(next) || seen.len() > 4096 {
            return Err(bad("invalid/cyclic loaded module list"));
        }
        if memory.u64(next.checked_add(8).ok_or_else(|| bad("list overflow"))?)? != previous {
            return Err(bad("inconsistent loaded module backlinks"));
        }
        let object = next
            .checked_sub(layout.links)
            .ok_or_else(|| bad("module entry underflow"))?;
        let field = |off: u64| {
            object
                .checked_add(off)
                .ok_or_else(|| bad("module field overflow"))
        };
        let base = memory.u64(field(layout.base)?)?;
        let mut size = [0; 4];
        memory.read(field(layout.size)?, &mut size)?;
        modules.push(Module {
            base,
            size: u32::from_le_bytes(size),
            name: unicode(memory, field(layout.name)?)?,
            path: unicode(memory, field(layout.path)?)?,
        });
        previous = next;
        next = memory.u64(next)?;
    }
    if memory.u64(
        head.checked_add(8)
            .ok_or_else(|| bad("list head overflow"))?,
    )? != previous
    {
        return Err(bad("inconsistent module list tail"));
    }
    Ok(modules)
}

pub fn unicode(memory: &impl VirtualMemory, address: u64) -> Result<String> {
    let mut s = [0; 16];
    memory.read(address, &mut s)?;
    let len = u16at(&s, 0) as usize;
    if len % 2 != 0 || len > u16at(&s, 2) as usize || len > 32768 {
        return Err(bad("invalid UNICODE_STRING"));
    }
    let ptr = u64::from_le_bytes(s[8..16].try_into().map_err(|_| bad("invalid pointer"))?);
    if len != 0 && ptr == 0 {
        return Err(bad("null Unicode string buffer"));
    }
    let mut b = vec![0; len];
    memory.read(ptr, &mut b)?;
    String::from_utf16(
        &b.chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect::<Vec<_>>(),
    )
    .map_err(|_| bad("invalid UTF-16"))
}

#[derive(Debug, Clone, Serialize)]
pub struct Callback {
    pub slot: u32,
    pub block: u64,
    pub function: u64,
    pub context: Option<u64>,
    pub layout_source: String,
    pub module: Option<String>,
    pub module_offset: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessEvent {
    pub kind: &'static str,
    pub process: u64,
    pub pid: u64,
    pub parent_pid: Option<u64>,
    pub creator_pid: Option<u64>,
    pub creator_tid: Option<u64>,
    pub image: Option<String>,
    pub command_line: Option<String>,
    pub creation_status: Option<u32>,
    pub exact_image_name: bool,
    pub layout_source: &'static str,
}

/// Explicit Ex/Ex2 callback interpretation, never inferred from a driver name.
/// PS_CREATE_NOTIFY_INFO is a public WDK ABI; its ARM64 size is checked before
/// reading pointers. This does not apply to legacy process callbacks.
pub fn process_event(memory: &impl VirtualMemory, x: &[u64; 31]) -> Result<ProcessEvent> {
    let mut event = ProcessEvent {
        kind: if x[2] == 0 { "exit" } else { "create" },
        process: x[0],
        pid: x[1],
        parent_pid: None,
        creator_pid: None,
        creator_tid: None,
        image: None,
        command_line: None,
        creation_status: None,
        exact_image_name: false,
        layout_source: "explicit Ex callback; public ARM64 WDK ABI",
    };
    if x[2] == 0 {
        return Ok(event);
    }
    if memory.u64(x[2])? != 72 {
        return Err(bad(
            "not the supported ARM64 PS_CREATE_NOTIFY_INFO; verify Ex callback entry",
        ));
    }
    let mut info = [0; 72];
    memory.read(x[2], &mut info)?;
    let pointer = |off| {
        let mut bytes = [0; 8];
        bytes.copy_from_slice(&info[off..off + 8]);
        u64::from_le_bytes(bytes)
    };
    event.parent_pid = Some(pointer(16));
    event.creator_pid = Some(pointer(24));
    event.creator_tid = Some(pointer(32));
    if pointer(48) != 0 {
        event.image = Some(unicode(memory, pointer(48))?);
    }
    if pointer(56) != 0 {
        event.command_line = Some(unicode(memory, pointer(56))?);
    }
    event.creation_status = Some(u32at(&info, 64));
    event.exact_image_name = u32at(&info, 8) & 1 != 0;
    Ok(event)
}

/// Read-only enumeration using types or validated code from the matching kernel.
pub fn process_callbacks(
    memory: &impl VirtualMemory,
    symbols: &crate::symbols::Symbols,
    modules: &[Module],
) -> Result<Vec<Callback>> {
    let table = symbols.resolve("PspCreateProcessNotifyRoutine")?;
    let typed = symbols
        .callback_slots
        .zip(symbols.fast_ref_bits)
        .zip(symbols.field("_EX_CALLBACK_ROUTINE_BLOCK", "Function").ok());
    let (slots, bits, function_offset, layout_source) = if let Some(((slots, bits), offset)) = typed
    {
        (slots, bits, offset, "matching PDB types")
    } else {
        let (slots, bits, offset) = derive_callback_layout(memory, symbols)?;
        (
            slots,
            bits,
            offset,
            "validated ARM64 kernel accessor and loop instructions",
        )
    };
    let context_offset = symbols.field("_EX_CALLBACK_ROUTINE_BLOCK", "Context").ok();
    let mut callbacks = Vec::new();
    for slot in 0..slots {
        let address = table
            .checked_add(slot as u64 * 8)
            .ok_or_else(|| bad("callback table overflow"))?;
        let block = memory.u64(address)? & !((1u64 << bits) - 1);
        if block == 0 {
            continue;
        }
        let function = memory.u64(
            block
                .checked_add(function_offset)
                .ok_or_else(|| bad("callback block overflow"))?,
        )?;
        let context = context_offset
            .map(|offset| {
                memory.u64(
                    block
                        .checked_add(offset)
                        .ok_or_else(|| bad("callback block overflow"))?,
                )
            })
            .transpose()?;
        let owner = modules
            .iter()
            .find(|m| function >= m.base && function - m.base < m.size as u64);
        callbacks.push(Callback {
            slot,
            block,
            function,
            context,
            layout_source: layout_source.into(),
            module: owner.map(|m| m.name.clone()),
            module_offset: owner.map(|m| function - m.base),
        });
    }
    Ok(callbacks)
}

/// Public symbols often omit private types. Accept only instruction shapes
/// whose meaning we can check; a changed implementation must fail closed.
/// This is code-derived layout evidence, not PDB type information.
fn derive_callback_layout(
    memory: &impl VirtualMemory,
    symbols: &crate::symbols::Symbols,
) -> Result<(u32, u8, u64)> {
    fn words(memory: &impl VirtualMemory, address: u64, length: usize) -> Result<Vec<u32>> {
        let mut bytes = vec![0; length];
        memory.read(address, &mut bytes)?;
        Ok(bytes
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect())
    }
    let getter = words(memory, symbols.resolve("ExGetCallBackBlockRoutine")?, 8)?;
    // LDR X0,[X0,#unsigned_imm12*8]; RET X30.
    if getter[0] & 0xffc003ff != 0xf9400000 || getter[1] != 0xd65f03c0 {
        return Err(bad(
            "unrecognized callback routine accessor; no layout inferred",
        ));
    }
    let offset = ((getter[0] >> 10) & 4095) as u64 * 8;
    if offset > 64 {
        return Err(bad("unexpected callback routine field offset"));
    }
    let reference = words(memory, symbols.resolve("ExReferenceCallBackBlock")?, 512)?;
    // AND Xd,Xn,#0xfffffffffffffff0 (operand registers deliberately ignored).
    if !reference.iter().any(|w| w & 0xfffffc00 == 0x927cec00) {
        return Err(bad("callback fast-reference mask not recognized"));
    }
    for name in [
        "PspSetCreateProcessNotifyRoutine",
        "PspCallProcessNotifyRoutines",
    ] {
        let code = words(memory, symbols.resolve(name)?, 768)?;
        // CMP Wn,#64. Require the bound in both registration and dispatch.
        if !code.iter().any(|w| w & 0xfffffc1f == 0x7101001f) {
            return Err(bad(
                "callback array bound not recognized in registration/dispatch",
            ));
        }
    }
    Ok((64, 4, offset))
}

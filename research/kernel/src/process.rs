//! Process inspection using offsets from the running kernel's verified PDB.
use crate::symbols::Symbols;
use crate::windows::{self, Module, ModuleLayout, VirtualMemory};
use crate::{Error, Result};
use serde::Serialize;
use std::collections::HashSet;

#[derive(Debug, Clone, Serialize)]
pub struct Process {
    pub eprocess: u64,
    pub pid: u64,
    pub parent_pid: u64,
    pub name: String,
    pub directory_table: u64,
    pub peb: u64,
}

pub fn list(memory: &impl VirtualMemory, symbols: &Symbols) -> Result<Vec<Process>> {
    let head = symbols.resolve("PsActiveProcessHead")?;
    let links = symbols.field("_EPROCESS", "ActiveProcessLinks")?;
    let pid = symbols.field("_EPROCESS", "UniqueProcessId")?;
    let parent = symbols.field("_EPROCESS", "InheritedFromUniqueProcessId")?;
    let image = symbols.field("_EPROCESS", "ImageFileName")?;
    let peb = symbols.field("_EPROCESS", "Peb")?;
    let dtb =
        symbols.field("_EPROCESS", "Pcb")? + symbols.field("_KPROCESS", "DirectoryTableBase")?;
    let mut node = memory.u64(head)?;
    let mut previous = head;
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    while node != head {
        if !seen.insert(node) || seen.len() > 65536 || node < links || node & 7 != 0 {
            return Err(Error::Invalid("corrupt active process list".into()));
        }
        if memory.u64(node + 8)? != previous {
            return Err(Error::Invalid("process list backlink mismatch".into()));
        }
        let base = node - links;
        let mut name = [0; 15];
        memory.read(base + image, &mut name)?;
        result.push(Process {
            eprocess: base,
            pid: memory.u64(base + pid)?,
            parent_pid: memory.u64(base + parent)?,
            name: String::from_utf8_lossy(name.split(|b| *b == 0).next().unwrap_or(&[])).into(),
            directory_table: memory.u64(base + dtb)?,
            peb: memory.u64(base + peb)?,
        });
        previous = node;
        node = memory.u64(node)?;
    }
    if memory.u64(head + 8)? != previous {
        return Err(Error::Invalid("process list tail mismatch".into()));
    }
    Ok(result)
}

#[derive(Debug, Serialize)]
pub struct Peb {
    pub address: u64,
    pub image_base: u64,
    pub ldr: u64,
    pub parameters: u64,
    pub image_path: Option<String>,
    pub command_line: Option<String>,
    pub errors: Vec<String>,
}
pub fn peb(memory: &impl VirtualMemory, symbols: &Symbols, process: &Process) -> Result<Peb> {
    if process.peb == 0 {
        return Err(Error::Invalid("process has no native PEB".into()));
    }
    let pointer = |field| memory.u64(process.peb + symbols.field("_PEB", field)?);
    let parameters = pointer("ProcessParameters")?;
    let mut errors = Vec::new();
    let mut string = |field| match symbols
        .field("_RTL_USER_PROCESS_PARAMETERS", field)
        .and_then(|off| windows::unicode(memory, parameters + off))
    {
        Ok(s) => Some(s),
        Err(e) => {
            errors.push(e.to_string());
            None
        }
    };
    Ok(Peb {
        address: process.peb,
        image_base: pointer("ImageBaseAddress")?,
        ldr: pointer("Ldr")?,
        parameters,
        image_path: string("ImagePathName"),
        command_line: string("CommandLine"),
        errors,
    })
}
pub fn modules(
    memory: &impl VirtualMemory,
    symbols: &Symbols,
    process: &Process,
) -> Result<Vec<Module>> {
    let peb = peb(memory, symbols, process)?;
    let head = peb.ldr + symbols.field("_PEB_LDR_DATA", "InLoadOrderModuleList")?;
    let field = |name| symbols.field("_LDR_DATA_TABLE_ENTRY", name);
    windows::modules(
        memory,
        head,
        &ModuleLayout {
            links: field("InLoadOrderLinks")?,
            base: field("DllBase")?,
            size: field("SizeOfImage")?,
            name: field("BaseDllName")?,
            path: field("FullDllName")?,
        },
    )
}

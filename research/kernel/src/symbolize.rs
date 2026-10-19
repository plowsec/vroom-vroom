//! Names for guest code addresses, `module!function+0xoff`, and symbolized
//! call stacks. The kernel's from its PDB (fetched into a cache); another
//! module's from its PDB when the symbol server has it, else from names the
//! caller adds (a fuzz harness's block list), else its exports, else
//! `module+0xoff`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::arm64::Registers;
use crate::symbols::{self, Symbols};
use crate::unwind::{self, Tables};
use crate::windows::{self, Image, Module, VirtualMemory};
use crate::{Error, Result};

/// Per module, found on its first address.
#[derive(Debug, Default)]
struct Names {
    pdb: Option<Symbols>,
    /// Exports and the caller's names, by address.
    other: BTreeMap<u64, String>,
}

#[derive(Debug)]
pub struct Symbolizer {
    cache: PathBuf,
    kernel: u64,
    modules: Vec<Module>,
    names: HashMap<u64, Names>,
    /// The caller's names, by address, before their module is known.
    extra: BTreeMap<u64, String>,
    seen: HashMap<u64, String>,
    unwind: Tables,
}

impl Symbolizer {
    /// The kernel (found from VBAR and the PC), its PDB (from `cache`, or
    /// the symbol server into it) and the loaded modules.
    pub fn new(memory: &impl VirtualMemory, registers: &Registers, cache: &Path) -> Result<Self> {
        let kernel = windows::discover_kernel(memory, &[registers.vbar, registers.pc])?;
        let identity = kernel.pdb.as_ref().ok_or_else(|| Error::Invalid("kernel has no PDB identity".into()))?;
        let pdb = Symbols::load(&symbols::fetch(cache, identity)?, identity, kernel.base)?;
        let modules = windows::modules(memory, pdb.resolve("PsLoadedModuleList")?, &pdb.module_layout()?)?;
        let mut names = HashMap::new();
        let other = kernel.exports.iter().map(|(n, &a)| (a, n.clone())).collect();
        names.insert(kernel.base, Names { pdb: Some(pdb), other });
        Ok(Symbolizer { cache: cache.to_path_buf(), kernel: kernel.base, modules, names, extra: BTreeMap::new(), seen: HashMap::new(), unwind: Tables::default() })
    }

    /// Function names the caller knows (by start address), for modules
    /// without a PDB.
    pub fn add_names(&mut self, names: impl IntoIterator<Item = (u64, String)>) {
        self.extra.extend(names);
        self.seen.clear();
    }

    pub fn modules(&self) -> &[Module] {
        &self.modules
    }

    fn module(&self, address: u64) -> Option<&Module> {
        self.modules.iter().find(|m| address >= m.base && address - m.base < m.size as u64)
    }

    /// `module!function+0xoff`, `module+0xoff`, or the bare address.
    pub fn name(&mut self, memory: &impl VirtualMemory, address: u64) -> String {
        if let Some(n) = self.seen.get(&address) {
            return n.clone();
        }
        let n = self.lookup(memory, address);
        self.seen.insert(address, n.clone());
        n
    }

    fn lookup(&mut self, memory: &impl VirtualMemory, address: u64) -> String {
        let Some(module) = self.module(address).cloned() else {
            return format!("{address:#x}");
        };
        let short = module.name.rsplit_once('.').map_or(module.name.as_str(), |(stem, _)| stem).to_string();
        let short = if module.base == self.kernel { "nt".to_string() } else { short };
        if !self.names.contains_key(&module.base) {
            let found = self.load(memory, &module);
            self.names.insert(module.base, found);
        }
        let names = &self.names[&module.base];
        let range = module.base..=address;
        let pdb = names.pdb.as_ref().and_then(|s| s.addresses.range(range.clone()).next_back());
        // Without a PDB: the nearest name below, the caller's or an export.
        let other = names.other.range(range.clone()).next_back();
        let extra = self.extra.range(range).next_back();
        let best = pdb.or_else(|| match (other, extra) {
            (Some(o), Some(e)) => Some(if e.0 >= o.0 { e } else { o }),
            (o, e) => o.or(e),
        });
        match best {
            Some((&at, name)) if at == address => format!("{short}!{name}"),
            Some((&at, name)) => format!("{short}!{name}+{:#x}", address - at),
            None => format!("{short}+{:#x}", address - module.base),
        }
    }

    fn load(&self, memory: &impl VirtualMemory, module: &Module) -> Names {
        let Ok(image) = Image::read(memory, module.base) else { return Names::default() };
        let pdb = image
            .pdb
            .as_ref()
            .and_then(|id| symbols::fetch(&self.cache, id).ok().and_then(|p| Symbols::load(&p, id, module.base).ok()));
        let other = image.exports.iter().map(|(n, &a)| (a, n.clone())).collect();
        Names { pdb, other }
    }

    /// The call stack of `registers`' thread, innermost first, named.
    pub fn stack(&mut self, memory: &impl VirtualMemory, registers: &Registers) -> Vec<String> {
        let modules = self.modules.clone();
        let mut tables = std::mem::take(&mut self.unwind);
        let stack = unwind::walk(memory, registers, &modules, &mut tables);
        self.unwind = tables;
        let mut out: Vec<String> = stack.frames.iter().map(|f| self.name(memory, f.pc)).collect();
        if let Some(stop) = stack.stop {
            out.push(format!("({stop})"));
        }
        out
    }
}

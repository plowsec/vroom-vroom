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
    /// The base the image was linked for: functions known only from
    /// `.pdata` are `sub_<linked address>`, as disassemblers name them. The
    /// loader rewrites ImageBase when it relocates, so then the linker's
    /// default (0x140000000).
    linked: Option<u64>,
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
        names.insert(kernel.base, Names { pdb: Some(pdb), other, linked: None });
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

    /// A kernel symbol's address (`KiDispatchException`).
    pub fn resolve(&self, name: &str) -> Option<u64> {
        self.names.get(&self.kernel)?.pdb.as_ref()?.resolve(name).ok()
    }

    /// The loaded module named `name` (case-insensitive): base, end.
    pub fn module_range(&self, name: &str) -> Option<(u64, u64)> {
        self.modules
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case(name))
            .map(|m| (m.base, m.base + u64::from(m.size)))
    }

    /// The registers a kernel trap frame (`_KTRAP_FRAME` at `frame`) holds:
    /// where the thread was when the exception came. `like` gives what the
    /// frame doesn't (translation registers, x19-x28).
    pub fn trap_registers(&self, memory: &impl VirtualMemory, frame: u64, like: &Registers) -> Result<Registers> {
        let pdb = self.names.get(&self.kernel).and_then(|n| n.pdb.as_ref()).ok_or_else(|| Error::Invalid("no kernel PDB".into()))?;
        // `frame` comes from the guest (x2 at KiDispatchException): any value.
        let at = |offset: u64| frame.checked_add(offset).ok_or_else(|| Error::Invalid(format!("trap frame {frame:#x} wraps")));
        let field = |name: &str| pdb.field("_KTRAP_FRAME", name).and_then(at);
        let mut r = like.clone();
        r.pc = memory.u64(field("Pc")?)?;
        r.sp = memory.u64(field("Sp")?)?;
        let x0 = field("X0")?;
        for i in 0..19 {
            r.x[i] = memory.u64(x0.checked_add(8 * i as u64).ok_or_else(|| Error::Invalid("trap frame wraps".into()))?)?;
        }
        r.x[29] = memory.u64(field("Fp")?)?;
        r.x[30] = memory.u64(field("Lr")?)?;
        Ok(r)
    }

    /// Whether `address` is in a loaded module.
    pub fn in_module(&self, address: u64) -> bool {
        self.module(address).is_some()
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
        let pdata = self.unwind.function_start(memory, &module, address);
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
        // Without a PDB, a name below the function's start is another
        // function's.
        let unnamed = match (names.pdb.is_none(), pdata, names.linked) {
            (true, Some(start), Some(linked)) if best.is_none_or(|(&at, _)| at < start) => Some((start, format!("sub_{:x}", (start - module.base).wrapping_add(linked)))),
            _ => None,
        };
        if let Some((at, name)) = unnamed {
            return if at == address { format!("{short}!{name}") } else { format!("{short}!{name}+{:#x}", address - at) };
        }
        match best {
            Some((&at, name)) if at == address => format!("{short}!{name}"),
            Some((&at, name)) => format!("{short}!{name}+{:#x}", address - at),
            None => format!("{short}+{:#x}", address - module.base),
        }
    }

    fn load(&self, memory: &impl VirtualMemory, module: &Module) -> Names {
        let Ok(image) = Image::read(memory, module.base) else { return Names::default() };
        let linked = Some(if image.image_base == module.base { 0x1_4000_0000 } else { image.image_base });
        let pdb = image
            .pdb
            .as_ref()
            .and_then(|id| symbols::fetch(&self.cache, id).ok().and_then(|p| Symbols::load(&p, id, module.base).ok()));
        let other = image.exports.iter().map(|(n, &a)| (a, n.clone())).collect();
        Names { pdb, other, linked }
    }

    /// The call stack of `registers`' thread, innermost first, named.
    pub fn stack(&mut self, memory: &impl VirtualMemory, registers: &Registers) -> Vec<String> {
        let (frames, stop) = self.frames(memory, registers);
        let mut out: Vec<String> = frames.into_iter().map(|(_, n)| n).collect();
        out.extend(stop.map(|s| format!("({s})")));
        out
    }

    /// The call stack as (pc, name), and why the walk stopped early.
    pub fn frames(&mut self, memory: &impl VirtualMemory, registers: &Registers) -> (Vec<(u64, String)>, Option<String>) {
        let (pcs, stop) = self.pcs(memory, registers);
        let frames = pcs.into_iter().map(|pc| (pc, self.name(memory, pc))).collect();
        (frames, stop)
    }

    /// The call stack's PCs, unnamed (no PDB loaded), and why the walk
    /// stopped early.
    pub fn pcs(&mut self, memory: &impl VirtualMemory, registers: &Registers) -> (Vec<u64>, Option<String>) {
        let modules = self.modules.clone();
        let mut tables = std::mem::take(&mut self.unwind);
        let stack = unwind::walk(memory, registers, &modules, &mut tables);
        self.unwind = tables;
        (stack.frames.iter().map(|f| f.pc).collect(), stack.stop)
    }
}

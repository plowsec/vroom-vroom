//! Microsoft symbol cache, verified against the PE's CodeView GUID and age.
use crate::windows::{ModuleLayout, PdbIdentity};
use crate::{Error, Result};
use pdb::FallibleIterator;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn err(e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("symbols: {e}"))
}

#[derive(Debug)]
pub struct Symbols {
    pub names: BTreeMap<String, u64>,
    pub functions: Vec<(String, u64)>,
    pub addresses: BTreeMap<u64, String>,
    pub types: BTreeMap<String, BTreeMap<String, u64>>,
    pub path: PathBuf,
    pub callback_slots: Option<u32>,
    pub fast_ref_bits: Option<u8>,
}

fn verify(pdb: &mut pdb::PDB<'_, std::fs::File>, identity: &PdbIdentity) -> Result<()> {
    let info = pdb.pdb_information().map_err(err)?;
    let guid = info.guid.simple().to_string().to_uppercase();
    let age = info.age;
    if guid != identity.guid || age < identity.age {
        return Err(err("PDB GUID/age does not match the running image"));
    }
    if let Some(age) = pdb.debug_information().map_err(err)?.age() {
        if age != identity.age {
            return Err(err("PDB DBI age does not match the running image"));
        }
    }
    Ok(())
}

pub fn fetch(cache: &Path, identity: &PdbIdentity) -> Result<PathBuf> {
    // Identity is derived from untrusted guest memory: validate it again at
    // the filesystem/network boundary even when used outside the PE reader.
    if identity.name.is_empty()
        || !identity.name.ends_with(".pdb")
        || !identity
            .name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        || identity.guid.len() != 32
        || !identity.guid.bytes().all(|c| c.is_ascii_hexdigit())
    {
        return Err(err("unsafe symbol identity"));
    }
    let directory = cache.join(&identity.name).join(identity.key());
    std::fs::create_dir_all(&directory).map_err(err)?;
    let final_path = directory.join(&identity.name);
    if final_path.exists() {
        let mut pdb =
            pdb::PDB::open(std::fs::File::open(&final_path).map_err(err)?).map_err(err)?;
        verify(&mut pdb, identity)?;
        return Ok(final_path);
    }
    let temporary = directory.join(format!("download-{}.part", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&temporary)
        .map_err(err)?;
    let result = (|| {
        let response = std::process::Command::new("curl")
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--max-redirs",
                "3",
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "--connect-timeout",
                "20",
                "--max-time",
                "180",
                "--max-filesize",
                "536870912",
            ])
            .arg(identity.url())
            .stdout(file.try_clone().map_err(err)?)
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(err)?;
        if !response.status.success() {
            return Err(err(String::from_utf8_lossy(&response.stderr)));
        }
        file.sync_all().map_err(err)?;
        let mut pdb = pdb::PDB::open(std::fs::File::open(&temporary).map_err(err)?).map_err(err)?;
        verify(&mut pdb, identity)?;
        // Never overwrite a cache object published by a concurrent download.
        std::fs::hard_link(&temporary, &final_path).map_err(err)?;
        Ok(final_path)
    })();
    let _ = std::fs::remove_file(&temporary);
    result
}

impl Symbols {
    pub fn load(path: &Path, identity: &PdbIdentity, base: u64) -> Result<Self> {
        let mut pdb = pdb::PDB::open(std::fs::File::open(path).map_err(err)?).map_err(err)?;
        verify(&mut pdb, identity)?;
        let map = pdb.address_map().map_err(err)?;
        let globals = pdb.global_symbols().map_err(err)?;
        let mut iter = globals.iter();
        let mut names = BTreeMap::new();
        let mut functions = BTreeMap::new();
        let mut callback_array = None;
        while let Some(s) = iter.next().map_err(err)? {
            let (offset, name, function) = match s.parse() {
                Ok(pdb::SymbolData::Public(s)) => (s.offset, s.name, s.function),
                Ok(pdb::SymbolData::Data(s)) => {
                    if s.name.to_string() == "PspCreateProcessNotifyRoutine" {
                        callback_array = Some(s.type_index);
                    }
                    (s.offset, s.name, false)
                }
                Ok(pdb::SymbolData::Procedure(s)) => (s.offset, s.name, true),
                _ => continue,
            };
            if let Some(rva) = offset.to_rva(&map) {
                if let Some(address) = base.checked_add(rva.0 as u64) {
                    let name = name.to_string().into_owned();
                    if function {
                        functions.insert(name.clone(), address);
                    }
                    names.insert(name, address);
                }
            }
        }
        let information = pdb.type_information().map_err(err)?;
        let mut finder = information.finder();
        let mut iter = information.iter();
        let mut wanted = BTreeMap::new();
        while let Some(t) = iter.next().map_err(err)? {
            finder.update(&iter);
            if let Ok(pdb::TypeData::Union(c)) = t.parse() {
                if !c.properties.forward_reference() && c.name.to_string() == "_EX_FAST_REF" {
                    wanted.insert(c.name.to_string().into_owned(), c.fields);
                }
            } else if let Ok(pdb::TypeData::Class(c)) = t.parse() {
                let name = c.name.to_string().into_owned();
                if !c.properties.forward_reference()
                    && matches!(
                        name.as_str(),
                        "_KLDR_DATA_TABLE_ENTRY"
                            | "_LDR_DATA_TABLE_ENTRY"
                            | "_EPROCESS"
                            | "_KPROCESS"
                            | "_PEB"
                            | "_PEB_LDR_DATA"
                            | "_RTL_USER_PROCESS_PARAMETERS"
                            | "_ETHREAD"
                            | "_KTHREAD"
                            | "_TEB"
                            | "_PS_CREATE_NOTIFY_INFO"
                            | "_EX_CALLBACK_ROUTINE_BLOCK"
                            | "_KTRAP_FRAME"
                    )
                {
                    if let Some(fields) = c.fields {
                        wanted.insert(name, fields);
                    }
                }
            }
        }
        let mut types = BTreeMap::new();
        let mut fast_ref_bits = None;
        for (name, first) in wanted {
            let mut fields = BTreeMap::new();
            let mut current = Some(first);
            let mut seen = std::collections::HashSet::new();
            while let Some(index) = current {
                if !seen.insert(index.0) || seen.len() > 1024 {
                    return Err(err("cyclic PDB field lists"));
                }
                let item = finder.find(index).map_err(err)?;
                let pdb::TypeData::FieldList(list) = item.parse().map_err(err)? else {
                    return Err(err("expected PDB field list"));
                };
                for f in list.fields {
                    if let pdb::TypeData::Member(m) = f {
                        if name == "_EX_FAST_REF" && m.name.to_string() == "RefCnt" {
                            if let Ok(item) = finder.find(m.field_type) {
                                if let Ok(pdb::TypeData::Bitfield(bit)) = item.parse() {
                                    if bit.position == 0 && bit.length <= 6 {
                                        fast_ref_bits = Some(bit.length);
                                    }
                                }
                            }
                        }
                        fields.insert(m.name.to_string().into_owned(), m.offset);
                    }
                }
                current = list.continuation;
            }
            types.insert(name, fields);
        }
        let callback_slots = callback_array
            .and_then(|index| finder.find(index).ok())
            .and_then(|t| t.parse().ok())
            .and_then(|ty| match ty {
                pdb::TypeData::Array(a) if a.dimensions.len() == 1 => a.dimensions.first().copied(),
                _ => None,
            })
            .filter(|size| *size > 0 && *size <= 8192 && *size % 8 == 0)
            .map(|size| size / 8);
        Ok(Self {
            addresses: names.iter().map(|(n, &a)| (a, n.clone())).collect(),
            names,
            functions: functions.into_iter().collect(),
            types,
            path: path.into(),
            callback_slots,
            fast_ref_bits,
        })
    }

    pub fn resolve(&self, name: &str) -> Result<u64> {
        let name = name.strip_prefix("nt!").unwrap_or(name);
        self.names
            .get(name)
            .copied()
            .ok_or_else(|| err(format!("symbol {name} unavailable in this PDB")))
    }
    pub fn field(&self, ty: &str, name: &str) -> Result<u64> {
        self.types
            .get(ty)
            .and_then(|fields| fields.get(name))
            .copied()
            .ok_or_else(|| err(format!("field {ty}.{name} unavailable in this PDB")))
    }
    pub fn module_layout(&self) -> Result<ModuleLayout> {
        let ty = if self.types.contains_key("_KLDR_DATA_TABLE_ENTRY") {
            "_KLDR_DATA_TABLE_ENTRY"
        } else {
            "_LDR_DATA_TABLE_ENTRY"
        };
        Ok(ModuleLayout {
            links: self.field(ty, "InLoadOrderLinks")?,
            base: self.field(ty, "DllBase")?,
            size: self.field(ty, "SizeOfImage")?,
            name: self.field(ty, "BaseDllName")?,
            path: self.field(ty, "FullDllName")?,
        })
    }
}

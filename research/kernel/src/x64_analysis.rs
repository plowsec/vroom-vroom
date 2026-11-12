//! Advanced AMD64 lab analysis, isolated from execution control.
use super::*;
use vroom_kernel::windows::{self, Callback, ProcessEvent, VirtualMemory};

/// Publish both analysis artifacts without clobbering existing files. If the
/// second create/write fails, remove only files created by this invocation.
fn write_export(path: &Path, bytes: &[u8], manifest: &Value) -> Result<(), String> {
    let metadata_path = path.with_extension("json");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(string)?;
    let mut metadata_created = false;
    let result = (|| {
        file.write_all(bytes).map_err(string)?;
        file.flush().map_err(string)?;
        let mut metadata = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&metadata_path)
            .map_err(string)?;
        metadata_created = true;
        serde_json::to_writer_pretty(&mut metadata, manifest).map_err(string)?;
        metadata.flush().map_err(string)
    })();
    drop(file);
    if result.is_err() {
        let _ = std::fs::remove_file(path);
        if metadata_created {
            let _ = std::fs::remove_file(metadata_path);
        }
    }
    result
}

#[path = "x64_callbacks.rs"]
mod callback_layout;
#[path = "x64_image.rs"]
mod image;

/// One-frame caller recovery for execution control. Uses the supplied CPU
/// context verbatim; callers must not replace CR3 with a selected process.
/// Returns (caller RIP, caller RSP), committing no guest state.
pub(crate) fn caller_in_image(
    memory: &impl VirtualMemory,
    registers: &Registers,
    base: u64,
) -> Result<(u64, u64), String> {
    let image = Image::read_amd64(memory, base).map_err(string)?;
    let mut context = unwind::Context::from(&registers.regs);
    unwind::step(memory, &image, &mut context, registers.regs.rip)?;
    if context.pc == 0 {
        return Err("caller return address is null".into());
    }
    Ok((context.pc, context.sp()))
}
#[path = "x64_unwind.rs"]
pub(crate) mod unwind;

fn callbacks(
    m: &impl VirtualMemory,
    s: &Symbols,
    modules: &[windows::Module],
) -> Result<Vec<Callback>, String> {
    let typed_function = s.field("_EX_CALLBACK_ROUTINE_BLOCK", "Function").ok();
    let typed_context = s.field("_EX_CALLBACK_ROUTINE_BLOCK", "Context").ok();
    let (slots, bits, offset, context, source) = match (
        s.callback_slots,
        s.fast_ref_bits,
        typed_function,
        typed_context,
    ) {
        (Some(slots), Some(bits), Some(function), context) => {
            (slots, bits, function, context, "matching AMD64 PDB types")
        }
        _ => {
            let layout = callback_layout::derive(m, s)?;
            if s.callback_slots.is_some_and(|v| v != layout.slots)
                || s.fast_ref_bits.is_some_and(|v| v != layout.bits)
                || typed_function.is_some_and(|v| v != layout.function)
                || typed_context.is_some_and(|v| Some(v) != layout.context)
            {
                return Err("matching PDB callback fields conflict with validated AMD64 instruction evidence".into());
            }
            (
                layout.slots, layout.bits, layout.function, layout.context,
                "validated AMD64 registration/removal loops, fast-reference helper, routine accessor and allocation stores",
            )
        }
    };
    if !(1..=256).contains(&slots)
        || !(1..=4).contains(&bits)
        || offset > 4096
        || context.is_some_and(|v| v > 4096)
    {
        return Err("invalid PDB callback layout".into());
    }
    let table = s.resolve("PspCreateProcessNotifyRoutine").map_err(string)?;
    let mut out = Vec::new();
    for slot in 0..slots {
        let at = table
            .checked_add(slot as u64 * 8)
            .ok_or("callback table overflow")?;
        let block = m.u64(at).map_err(string)? & !((1u64 << bits) - 1);
        if block == 0 {
            continue;
        }
        if !canonical(block) {
            return Err("noncanonical callback block".into());
        }
        let field = |off| {
            m.u64(block.checked_add(off).ok_or("callback field overflow")?)
                .map_err(string)
        };
        let function = field(offset)?;
        if !canonical(function) || function == 0 {
            return Err("invalid callback function".into());
        }
        let owner = modules
            .iter()
            .find(|m| function >= m.base && function - m.base < m.size as u64);
        out.push(Callback {
            slot,
            block,
            function,
            context: context.map(field).transpose()?,
            layout_source: source.into(),
            module: owner.map(|m| m.name.clone()),
            module_offset: owner.map(|m| function - m.base),
        });
    }
    Ok(out)
}

fn process_event(
    m: &impl VirtualMemory,
    r: &vroom_kernel::x86_64::General,
) -> Result<ProcessEvent, String> {
    // Both 64-bit WDK ABIs have the same documented structure layout, but
    // argument registers and provenance are explicitly AMD64 here.
    let mut e = ProcessEvent {
        kind: if r.r8 == 0 { "exit" } else { "create" },
        process: r.rcx,
        pid: r.rdx,
        parent_pid: None,
        creator_pid: None,
        creator_tid: None,
        image: None,
        command_line: None,
        creation_status: None,
        exact_image_name: false,
        layout_source: "explicit Ex callback; public AMD64 WDK ABI",
    };
    if r.r8 == 0 {
        return Ok(e);
    }
    if m.u64(r.r8).map_err(string)? != 72 {
        return Err("unsupported AMD64 PS_CREATE_NOTIFY_INFO; stop at Ex/Ex2 callback entry, not a legacy callback".into());
    }
    let mut b = [0; 72];
    m.read(r.r8, &mut b).map_err(string)?;
    let ptr = |off| {
        let mut value = [0; 8];
        value.copy_from_slice(&b[off..off + 8]);
        u64::from_le_bytes(value)
    };
    e.parent_pid = Some(ptr(16));
    e.creator_pid = Some(ptr(24));
    e.creator_tid = Some(ptr(32));
    if ptr(48) != 0 {
        e.image = Some(windows::unicode(m, ptr(48)).map_err(string)?);
    }
    if ptr(56) != 0 {
        e.command_line = Some(windows::unicode(m, ptr(56)).map_err(string)?);
    }
    e.creation_status = Some(u32::from_le_bytes([b[64], b[65], b[66], b[67]]));
    e.exact_image_name = b[8] & 1 != 0;
    Ok(e)
}

impl Session {
    pub(super) fn analysis_command(
        &mut self,
        op: &str,
        cpu: usize,
        q: &Value,
    ) -> Result<Value, String> {
        match op {
            "cache" => Ok(
                json!({"physical_pages":0,"capacity_pages":0,"module_pdbs":self.symbols.len(),
                "page_cache_enabled":false,"unwind_cache_entries":0}),
            ),
            "process_event" => {
                let r = self.registers(cpu, false)?;
                let m = AddressSpace {
                    memory: &self.vm,
                    registers: &r,
                };
                Ok(json!(process_event(&m, &r.regs)?))
            }
            "callbacks" => {
                self.find_kernel(cpu)?;
                let m = AddressSpace {
                    memory: &self.vm,
                    registers: self.kernel_registers()?,
                };
                let s = self.kernel_symbols()?;
                let modules = windows::modules(
                    &m,
                    s.resolve("PsLoadedModuleList").map_err(string)?,
                    &s.module_layout().map_err(string)?,
                )
                .map_err(string)?;
                Ok(json!(callbacks(&m, s, &modules)?))
            }
            "stack" => self.analysis_stack(cpu, q),
            "image" | "export" => self.analysis_image(op, cpu, q),
            _ => Err(format!("unknown analysis operation {op}")),
        }
    }
    fn analysis_stack(&mut self, cpu: usize, q: &Value) -> Result<Value, String> {
        if q.get("vector_length").is_some_and(|value| !value.is_null()) {
            return Err(
                "vector_length is ARM64-only; AMD64 stack unwinding does not use SVE".into(),
            );
        }
        // Never substitute the selected process's address space for a CPU stack.
        let r = self.registers(cpu, false)?;
        let m = AddressSpace {
            memory: &self.vm,
            registers: &r,
        };
        let limit = q.get("limit").map(number).transpose()?.unwrap_or(64);
        if !(1..=256).contains(&limit) {
            return Err("stack limit must be 1..256".into());
        }
        let mut c = unwind::Context::from(&r.regs);
        let mut frames = Vec::new();
        let mut stop = None;
        let mut method = "context";
        let mut images: Vec<Image> = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for index in 0..limit {
            if c.pc == 0 {
                break;
            }
            if !seen.insert((c.pc, c.sp())) {
                stop = Some("cyclic AMD64 stack context".into());
                break;
            }
            let lookup = if index == 0 || method == "machine_frame" {
                c.pc
            } else {
                c.pc.saturating_sub(1)
            };
            // Bounded image discovery works for kernel and user frames without
            // requiring private process/module symbols.
            let module = if let Some(im) = images
                .iter()
                .find(|im| lookup >= im.base && lookup - im.base < im.size as u64)
            {
                Ok(im.clone())
            } else {
                find_image(&m, lookup).inspect(|im| images.push(im.clone()))
            };
            let symbol = module.as_ref().ok().map(|im| {
                let name = self
                    .symbols
                    .get(&im.base)
                    .and_then(|s| s.addresses.range(..=c.pc).next_back())
                    .map(|(&at, name)| format!("{}!{}+0x{:x}", im.name, name, c.pc - at));
                name.unwrap_or_else(|| format!("{}+0x{:x}", im.name, c.pc - im.base))
            });
            frames.push(json!({"pc":c.pc,"sp":c.sp(),"module":module.as_ref().ok().map(|im|im.name.as_str()).unwrap_or("unknown"),"symbol":symbol,"method":method}));
            match module.and_then(|im| unwind::step(&m, &im, &mut c, lookup)) {
                Ok(next) => method = next,
                Err(e) => {
                    stop = Some(e);
                    break;
                }
            }
        }
        Ok(
            json!({"frames":frames,"stop":stop,"truncated":stop.is_none() && frames.len()==limit as usize && c.pc!=0,
            "architecture":"AMD64","cr3":r.sregs.cr3,
            "memory_context":"actual CPU CR3; no KPTI/system CR3 substitution",
            "limitations":"v1/v2 unwind metadata; canonical mirrored v2 epilogs; no dynamic function tables or trap-context synthesis"}),
        )
    }
    fn analysis_image(&mut self, op: &str, cpu: usize, q: &Value) -> Result<Value, String> {
        let base = number(&q["base"])?;
        let r = self.registers(cpu, true)?;
        let m = AddressSpace {
            memory: &self.vm,
            registers: &r,
        };
        let live = Image::read_amd64(&m, base).map_err(string)?;
        let (data, missing, im, source) = if op == "image" {
            let path = match q["path"].as_str() {
                Some(p) => PathBuf::from(p),
                None => image::fetch(&self.state.join("symbols"), &live).map_err(string)?,
            };
            let disk = image::Disk::load(&path, &live).map_err(string)?;
            let (data, missing) = image::export(&disk.image, &disk, false).map_err(string)?;
            (
                data,
                missing,
                disk.image,
                json!({"kind":"file","path":path,"runtime_base":base}),
            )
        } else {
            let partial = match q.get("allow_missing").filter(|v| !v.is_null()) {
                Some(v) => v.as_bool().ok_or("allow_missing: expected boolean")?,
                None => false,
            };
            let (data, missing) = image::export(&live, &m, partial).map_err(string)?;
            (
                data,
                missing,
                live,
                json!({"kind":"live","runtime_base":base}),
            )
        };
        let dir = self.state.join("exports");
        std::fs::create_dir_all(&dir).map_err(string)?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(string)?
            .as_nanos();
        let path = dir.join(format!("image-{base:016x}-{stamp}.exe"));
        let manifest = json!({"path":path,"image":im,"missing_page_rvas":missing,"source":source,
            "architecture":"AMD64","layout":"flat PE; section file offsets equal RVAs; analysis only"});
        write_export(&path, &data, &manifest)?;
        Ok(manifest)
    }
}

fn find_image(m: &impl VirtualMemory, address: u64) -> Result<Image, String> {
    if !canonical(address) {
        return Err("noncanonical stack PC".into());
    }
    let top = address & !0xfff;
    for i in 0..16384u64 {
        let Some(base) = top.checked_sub(i * 4096) else {
            break;
        };
        let mut magic = [0; 2];
        if m.read(base, &mut magic).is_ok() && magic == *b"MZ" {
            if let Ok(image) = Image::read_amd64(m, base) {
                if address >= base && address - base < image.size as u64 {
                    return Ok(image);
                }
            }
        }
    }
    Err("no AMD64 PE containing stack PC within 64 MiB; dynamic code unsupported".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Memory(Vec<u8>);
    impl VirtualMemory for Memory {
        fn read(&self, a: u64, b: &mut [u8]) -> vroom_kernel::Result<()> {
            let end = (a as usize)
                .checked_add(b.len())
                .ok_or_else(|| vroom_kernel::Error::Invalid("overflow".into()))?;
            let source = self
                .0
                .get(a as usize..end)
                .ok_or_else(|| vroom_kernel::Error::Invalid("unmapped".into()))?;
            b.copy_from_slice(source);
            Ok(())
        }
    }
    fn put(m: &mut Memory, a: usize, v: u64) {
        m.0[a..a + 8].copy_from_slice(&v.to_le_bytes());
    }
    #[test]
    fn export_artifacts_do_not_clobber_and_cleanup_failed_pairs() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = if Path::new("/tmp/opencode").is_dir() {
            PathBuf::from("/tmp/opencode")
        } else {
            std::env::temp_dir()
        };
        let path = root.join(format!("vroom-analysis-{}-{stamp}.exe", std::process::id()));
        let meta = path.with_extension("json");
        std::fs::write(&meta, b"existing metadata").unwrap();
        assert!(write_export(&path, b"MZ", &json!({})).is_err());
        assert!(!path.exists());
        assert_eq!(std::fs::read(&meta).unwrap(), b"existing metadata");
        std::fs::remove_file(&meta).unwrap();
        write_export(&path, b"MZ", &json!({"ok":true})).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"MZ");
        assert!(write_export(&path, b"overwrite", &json!({})).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"MZ");
        assert_eq!(
            serde_json::from_slice::<Value>(&std::fs::read(&meta).unwrap()).unwrap(),
            json!({"ok":true})
        );
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(meta).unwrap();
    }
    #[test]
    fn amd64_callback_argument_registers_and_layout() {
        let mut m = Memory(vec![0; 4096]);
        let mut r = vroom_kernel::x86_64::General {
            rcx: 0xffff_8000_1234_0000,
            rdx: 42,
            r8: 128,
            ..Default::default()
        };
        put(&mut m, 128, 72);
        put(&mut m, 128 + 8, 1);
        put(&mut m, 128 + 16, 4);
        put(&mut m, 128 + 24, 5);
        put(&mut m, 128 + 32, 6);
        m.0[128 + 64..128 + 68].copy_from_slice(&0xc000_0001u32.to_le_bytes());
        let e = process_event(&m, &r).unwrap();
        assert_eq!(
            (e.process, e.pid, e.parent_pid, e.creator_tid),
            (r.rcx, 42, Some(4), Some(6))
        );
        assert_eq!(e.creation_status, Some(0xc000_0001));
        assert!(e.exact_image_name);
        put(&mut m, 128, 64);
        assert!(process_event(&m, &r).is_err());
        r.r8 = 0;
        assert_eq!(process_event(&m, &r).unwrap().kind, "exit");
    }
    #[test]
    fn callback_layout_uses_types_or_requires_validated_code() {
        let mut m = Memory(vec![0; 4096]);
        put(&mut m, 128, 0x20b);
        put(&mut m, 0x210, 0x555);
        put(&mut m, 0x218, 0x777);
        let mut s = Symbols {
            names: BTreeMap::from([("PspCreateProcessNotifyRoutine".into(), 128)]),
            functions: vec![],
            addresses: BTreeMap::new(),
            types: BTreeMap::from([(
                "_EX_CALLBACK_ROUTINE_BLOCK".into(),
                BTreeMap::from([("Function".into(), 16), ("Context".into(), 24)]),
            )]),
            path: PathBuf::new(),
            callback_slots: Some(1),
            fast_ref_bits: Some(4),
        };
        let rows = callbacks(&m, &s, &[]).unwrap();
        assert_eq!(
            (rows[0].block, rows[0].function, rows[0].context),
            (0x200, 0x555, Some(0x777))
        );
        s.callback_slots = None;
        assert!(callbacks(&m, &s, &[]).is_err());
        s.callback_slots = Some(257);
        assert!(callbacks(&m, &s, &[]).is_err());
    }
}

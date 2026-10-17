use vroom_kernel::windows::{self, Image, ModuleLayout, VirtualMemory};
use vroom_kernel::{Error, Result};

struct Memory(Vec<u8>);
impl VirtualMemory for Memory {
    fn read(&self, address: u64, data: &mut [u8]) -> Result<()> {
        let end = address
            .checked_add(data.len() as u64)
            .ok_or_else(|| Error::Invalid("overflow".into()))?;
        let bytes = self
            .0
            .get(address as usize..end as usize)
            .ok_or_else(|| Error::Invalid("unmapped".into()))?;
        data.copy_from_slice(bytes);
        Ok(())
    }
}
fn w16(m: &mut Memory, at: usize, value: u16) {
    m.0[at..at + 2].copy_from_slice(&value.to_le_bytes());
}
fn w32(m: &mut Memory, at: usize, value: u32) {
    m.0[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
fn w64(m: &mut Memory, at: usize, value: u64) {
    m.0[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

fn pe() -> Memory {
    let mut m = Memory(vec![0; 0x3000]);
    m.0[..2].copy_from_slice(b"MZ");
    w32(&mut m, 0x3c, 0x80);
    m.0[0x80..0x84].copy_from_slice(b"PE\0\0");
    w16(&mut m, 0x84, 0xaa64);
    w16(&mut m, 0x86, 1);
    w16(&mut m, 0x94, 240);
    let o = 0x98;
    w16(&mut m, o, 0x20b);
    w32(&mut m, o + 56, 0x3000);
    w32(&mut m, o + 36, 512);
    w32(&mut m, o + 108, 16);
    w32(&mut m, o + 112 + 6 * 8, 0x400);
    w32(&mut m, o + 116 + 6 * 8, 28);
    w32(&mut m, 0x400 + 12, 2);
    w32(&mut m, 0x400 + 16, 35);
    w32(&mut m, 0x400 + 20, 0x500);
    m.0[0x500..0x504].copy_from_slice(b"RSDS");
    for i in 0..16 {
        m.0[0x504 + i] = i as u8;
    }
    w32(&mut m, 0x514, 1);
    m.0[0x518..0x523].copy_from_slice(b"sample.pdb\0");
    let section = o + 240;
    w32(&mut m, section + 8, 0x1000);
    w32(&mut m, section + 12, 0x1000);
    w32(&mut m, section + 16, 0x200);
    w32(&mut m, section + 20, 0x200);
    m.0[0x1000..0x1004].copy_from_slice(&0xd503201fu32.to_le_bytes());
    m
}

#[test]
fn codeview_identity_and_reconstructed_image() {
    let m = pe();
    let image = Image::read(&m, 0).unwrap();
    let pdb = image.pdb.as_ref().unwrap();
    assert_eq!(pdb.guid, "030201000504070608090A0B0C0D0E0F");
    assert_eq!(pdb.key(), "030201000504070608090A0B0C0D0E0F1");
    assert!(pdb.url().ends_with("/sample.pdb"));
    let (bytes, missing) = image.export(&m, false).unwrap();
    assert!(missing.is_empty());
    assert_eq!(&bytes[0x418..0x41c], &0x500u32.to_le_bytes());
    assert_eq!(&bytes[0x1000..0x1004], &0xd503201fu32.to_le_bytes());
    assert_eq!(
        &bytes[0x98 + 240 + 20..0x98 + 240 + 24],
        &0x1000u32.to_le_bytes()
    );
}

#[test]
fn corrupt_pe_and_missing_pages_are_explicit() {
    let mut m = pe();
    w32(&mut m, 0x3c, u32::MAX);
    assert!(Image::read(&m, 0).is_err());
    let mut m = pe();
    m.0[0x518] = b';';
    assert!(Image::read(&m, 0).is_err());
    let mut m = pe();
    let image = Image::read(&m, 0).unwrap();
    m.0.truncate(0x2000);
    assert!(image.export(&m, false).is_err());
    assert_eq!(image.export(&m, true).unwrap().1, vec![0x2000]);
}

#[test]
fn module_list_backlinks_and_cycles() {
    let mut m = Memory(vec![0; 0x1000]);
    let head = 0x100;
    w64(&mut m, head, 0x200);
    w64(&mut m, head + 8, 0x200);
    w64(&mut m, 0x200, head as u64);
    w64(&mut m, 0x208, head as u64);
    w64(&mut m, 0x210, 0xfffff80000000000);
    w32(&mut m, 0x218, 0x1000);
    let layout = ModuleLayout {
        links: 0,
        base: 0x10,
        size: 0x18,
        name: 0x20,
        path: 0x30,
    };
    let modules = windows::modules(&m, head as u64, &layout).unwrap();
    assert_eq!(modules.len(), 1);
    w64(&mut m, 0x200, 0x200);
    assert!(windows::modules(&m, head as u64, &layout).is_err());
    w64(&mut m, 0x200, head as u64);
    w64(&mut m, 0x208, 0);
    assert!(windows::modules(&m, head as u64, &layout).is_err());
}

#[test]
fn export_revalidates_headers_after_discovery() {
    let mut m = pe();
    let image = Image::read(&m, 0).unwrap();
    w32(&mut m, 0x3c, u32::MAX);
    assert!(image.export(&m, false).is_err());
    let mut m = pe();
    w16(&mut m, 0x94, 0);
    assert!(image.export(&m, false).is_err());
    let mut tiny = image;
    tiny.size = 16;
    assert!(tiny.export(&m, false).is_err());
}

#[test]
fn ex_process_event_validates_size_and_nullable_strings() {
    let mut m = Memory(vec![0; 0x1000]);
    let mut x = [0; 31];
    x[0] = 0xffff800000001000;
    x[1] = 123;
    assert_eq!(windows::process_event(&m, &x).unwrap().kind, "exit");
    x[2] = 0x100;
    assert!(windows::process_event(&m, &x).is_err());
    w64(&mut m, 0x100, 72);
    w64(&mut m, 0x110, 456);
    w64(&mut m, 0x118, 789);
    w64(&mut m, 0x130, 0x200);
    w16(&mut m, 0x200, 8);
    w16(&mut m, 0x202, 8);
    w64(&mut m, 0x208, 0x300);
    for (i, c) in "demo".encode_utf16().enumerate() {
        w16(&mut m, 0x300 + i * 2, c);
    }
    let event = windows::process_event(&m, &x).unwrap();
    assert_eq!(event.parent_pid, Some(456));
    assert_eq!(event.creator_pid, Some(789));
    assert_eq!(event.image.as_deref(), Some("demo"));
    assert!(event.command_line.is_none());
    w16(&mut m, 0x200, 9);
    assert!(windows::process_event(&m, &x).is_err());
}

#[test]
fn symbol_download_rejects_untrusted_paths_before_io() {
    let identity = windows::PdbIdentity {
        name: "../../bad.pdb".into(),
        guid: "0".repeat(32),
        age: 1,
    };
    assert!(vroom_kernel::symbols::fetch(std::path::Path::new("/nonexistent"), &identity).is_err());
}

#[test]
fn callback_code_evidence_and_module_ownership() {
    use std::collections::BTreeMap;
    let mut m = Memory(vec![0; 0x10000]);
    // LDR X0,[X0,#8]; RET. AND X21,X8,#-16. CMP W24,#64.
    w32(&mut m, 0x1000, 0xf9400400);
    w32(&mut m, 0x1004, 0xd65f03c0);
    w32(&mut m, 0x2000, 0x927ced15);
    w32(&mut m, 0x3000, 0x7101031f);
    w32(&mut m, 0x4000, 0x710102df);
    w64(&mut m, 0x5000, 0x6003);
    w64(&mut m, 0x6008, 0xf040);
    let symbols = vroom_kernel::symbols::Symbols {
        names: BTreeMap::from([
            ("ExGetCallBackBlockRoutine".into(), 0x1000),
            ("ExReferenceCallBackBlock".into(), 0x2000),
            ("PspSetCreateProcessNotifyRoutine".into(), 0x3000),
            ("PspCallProcessNotifyRoutines".into(), 0x4000),
            ("PspCreateProcessNotifyRoutine".into(), 0x5000),
        ]),
        functions: Vec::new(),
        addresses: BTreeMap::new(),
        types: BTreeMap::new(),
        path: Default::default(),
        callback_slots: None,
        fast_ref_bits: None,
    };
    let modules = [windows::Module {
        base: 0xf000,
        size: 0x100,
        name: "example.sys".into(),
        path: "example.sys".into(),
    }];
    let callbacks = windows::process_callbacks(&m, &symbols, &modules).unwrap();
    assert_eq!(callbacks.len(), 1);
    assert_eq!(callbacks[0].function, 0xf040);
    assert_eq!(callbacks[0].module.as_deref(), Some("example.sys"));
    assert_eq!(callbacks[0].module_offset, Some(0x40));
    assert_eq!(
        callbacks[0].context, None,
        "missing type information stays unknown"
    );
    // A changed bound must not silently use an old layout.
    w32(&mut m, 0x4000, 0x710202df);
    assert!(windows::process_callbacks(&m, &symbols, &modules).is_err());
}

#[test]
#[ignore = "requires a local matching PDB; no proprietary symbols are checked in"]
fn real_pdb_identity_and_windows_layouts() {
    let path = std::path::PathBuf::from(std::env::var("VROOM_TEST_PDB").expect("VROOM_TEST_PDB"));
    let identity = windows::PdbIdentity {
        name: "ntkrnlmp.pdb".into(),
        guid: std::env::var("VROOM_TEST_PDB_GUID").expect("VROOM_TEST_PDB_GUID"),
        age: 1,
    };
    let symbols = vroom_kernel::symbols::Symbols::load(&path, &identity, 0).unwrap();
    assert!(symbols.resolve("PsLoadedModuleList").unwrap() > 0);
    assert!(
        symbols
            .resolve("PsSetCreateProcessNotifyRoutineEx")
            .unwrap()
            > 0
    );
    assert!(symbols.module_layout().unwrap().base > 0);
    eprintln!(
        "types: {:?}; slots {:?}; fastref {:?}",
        symbols.types.keys().collect::<Vec<_>>(),
        symbols.callback_slots,
        symbols.fast_ref_bits
    );
    let wrong = windows::PdbIdentity {
        guid: "0".repeat(32),
        ..identity
    };
    assert!(vroom_kernel::symbols::Symbols::load(&path, &wrong, 0).is_err());
}

#[test]
fn disk_image_verifies_identity_and_rejects_overlapping_sections() {
    use vroom_kernel::binary::DiskImage;
    let mut m = pe();
    w32(&mut m, 0x98 + 60, 4096);
    let expected = Image::read(&m, 0).unwrap();
    let (bytes, _) = expected.export(&m, false).unwrap();
    let path = std::env::temp_dir().join(format!(
        "vroom-pe-test-{}-{}.exe",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&path, &bytes).unwrap();
    let disk = DiskImage::load(&path, &expected).unwrap();
    assert_eq!(
        disk.image.pdb.unwrap().guid,
        expected.pdb.as_ref().unwrap().guid
    );
    let wrong = Image {
        timestamp: 1,
        ..expected.clone()
    };
    assert!(DiskImage::load(&path, &wrong).is_err());
    let mut corrupt = bytes.clone();
    corrupt[0x504] ^= 1;
    std::fs::write(&path, &corrupt).unwrap();
    assert!(DiskImage::load(&path, &expected).is_err());
    let mut corrupt = bytes;
    corrupt[0x98 + 240 + 12..0x98 + 240 + 16].copy_from_slice(&0x800u32.to_le_bytes());
    std::fs::write(&path, &corrupt).unwrap();
    assert!(DiskImage::load(&path, &expected).is_err());
    std::fs::remove_file(path).unwrap();
}

#[test]
fn process_list_uses_pdb_fields_and_rejects_corrupt_links() {
    use std::collections::BTreeMap;
    use vroom_kernel::{process, symbols::Symbols};
    let mut m = Memory(vec![0; 4096]);
    let fields = |items: &[(&str, u64)]| items.iter().map(|(k, v)| (k.to_string(), *v)).collect();
    let symbols = Symbols {
        names: BTreeMap::from([("PsActiveProcessHead".into(), 0x100)]),
        functions: Default::default(),
        addresses: Default::default(),
        path: Default::default(),
        callback_slots: None,
        fast_ref_bits: None,
        types: BTreeMap::from([
            (
                "_EPROCESS".into(),
                fields(&[
                    ("ActiveProcessLinks", 0x40),
                    ("UniqueProcessId", 0x60),
                    ("InheritedFromUniqueProcessId", 0x68),
                    ("ImageFileName", 0x70),
                    ("Peb", 0x80),
                    ("Pcb", 0),
                ]),
            ),
            ("_KPROCESS".into(), fields(&[("DirectoryTableBase", 0x20)])),
        ]),
    };
    w64(&mut m, 0x100, 0x240);
    w64(&mut m, 0x108, 0x240);
    w64(&mut m, 0x240, 0x100);
    w64(&mut m, 0x248, 0x100);
    w64(&mut m, 0x260, 42);
    w64(&mut m, 0x268, 4);
    w64(&mut m, 0x220, 0x8000);
    m.0[0x270..0x27b].copy_from_slice(b"example.exe");
    let processes = process::list(&m, &symbols).unwrap();
    assert_eq!(processes.len(), 1);
    assert_eq!(processes[0].pid, 42);
    assert_eq!(processes[0].name, "example.exe");
    w64(&mut m, 0x240, 0x240);
    assert!(process::list(&m, &symbols).is_err());
    w64(&mut m, 0x240, 0x100);
    w64(&mut m, 0x108, 0);
    assert!(process::list(&m, &symbols).is_err());
}

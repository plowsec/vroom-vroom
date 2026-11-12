//! Input ownership and the actual public early gate. No hypervisor executes.
use super::*;
#[cfg(not(feature = "research"))]
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
use vroom_config::{Disk, DiskSync};
use vroom_hv::{IrqChipInfo, Vm, VmParams};

#[derive(Debug)]
struct TestHv { creates: AtomicU64, arch: Arch, name: &'static str }
impl TestHv {
    fn kvm() -> Arc<Self> { Arc::new(Self { creates: AtomicU64::new(0), arch: Arch::X86_64, name: "kvm" }) }
}
impl Hypervisor for TestHv {
    fn name(&self) -> &'static str { self.name }
    fn arch(&self) -> Arch { self.arch }
    fn irqchip_info(&self) -> vroom_hv::Result<IrqChipInfo> { panic!("preflight must not request an irqchip") }
    fn create_vm(&self, _: &VmParams) -> vroom_hv::Result<Arc<dyn Vm>> {
        self.creates.fetch_add(1, SeqCst);
        Err(vroom_hv::Error::Unsupported("test must not create a VM"))
    }
}

fn shape(root: &Path) -> (VmConfig, VmmOptions) {
    let mut config = VmConfig::from_toml("name='efi-inputs'\ncpus=1\nmemory_mib=512\nballoon=false\nos='other'\n[clipboard]\nhost_to_guest=false\nguest_to_host=false\n[boot]\ntype='uefi'\nvars='unused'\n").unwrap();
    config.boot = Boot::Uefi { firmware: Some(root.join("code")), vars: root.join("vars"), secure_boot: false };
    config.disks = vec![Disk { path: root.join("disk"), readonly: true, format: DiskFormat::Raw,
        cdrom: false, bus: DiskBus::Nvme, sync: DiskSync::Fsync, backing_dirs: Vec::new() }];
    let opts = VmmOptions { serial_out: Box::new(io::sink()), start: StartMode::BootPaused,
        clipboard: None, ui: None, state_dir: root.join("state"), sandbox: true };
    (config, opts)
}

fn fixture(root: &Path) -> (VmConfig, VmmOptions, Pins) {
    let code = vec![0x51; CODE_SIZE as usize];
    let vars = vec![0x71; VARS_SIZE as usize];
    let disk = vec![0x91; MIN_DISK_SIZE as usize];
    std::fs::write(root.join("code"), &code).unwrap();
    std::fs::write(root.join("vars"), &vars).unwrap();
    std::fs::write(root.join("disk"), disk).unwrap();
    let pins = Pins { code: Sha256::digest(&code).into(), vars: Sha256::digest(&vars).into() };
    let (config, opts) = shape(root);
    (config, opts, pins)
}

fn with_pins<T>(pins: Pins, run: impl FnOnce() -> T) -> T {
    struct Reset(Option<Pins>);
    impl Drop for Reset { fn drop(&mut self) { TEST_PINS.with(|slot| slot.set(self.0)); } }
    let _reset = Reset(TEST_PINS.with(|slot| slot.replace(Some(pins))));
    run()
}

fn small_bounds() -> Bounds { Bounds { min: 1, max: 4096, alignment: 1 } }

#[test]
#[cfg(not(feature = "research"))] // This preflight explicitly requires the default build.
fn selected_pair_pins_and_exact_geometry_are_fixed() {
    let hex = |b: [u8; 32]| b.iter().map(|v| format!("{v:02x}")).collect::<String>();
    assert_eq!(hex(CODE_SHA256), "32807682a9e5c0e2d192ecb6077941d4be8bcfa6b9cd6e60830c3a9f48c0555a");
    assert_eq!(hex(VARS_SHA256), "5d2ac383371b408398accee7ec27c8c09ea5b74a0de0ceea6513388b15be5d1e");
    assert!(Q35FirmwareLayout::for_images(CODE_SIZE as usize, VARS_SIZE as usize).is_ok());
    let temp = tempfile::tempdir().unwrap();
    let (config, opts, _) = fixture(temp.path());
    assert!(matches!(prepare(&config, TestHv::kvm().as_ref(), &opts), Err(Error::Setup(s)) if s.contains("digest")));
}

#[test]
fn shape_rejections_precede_all_input_opens() {
    let temp = tempfile::tempdir().unwrap(); // All three paths remain absent.
    let mutations: Vec<Box<dyn Fn(&mut VmConfig, &mut VmmOptions)>> = vec![
        Box::new(|c, _| c.cpus = 2), Box::new(|c, _| c.memory_mib = 1024),
        Box::new(|_, o| o.start = StartMode::Boot), Box::new(|_, o| o.sandbox = false),
        Box::new(|c, _| c.security.sandbox = false), Box::new(|c, _| c.nested = true),
        Box::new(|c, _| c.balloon = true), Box::new(|c, _| c.tpm = true),
        Box::new(|c, _| c.camera = true), Box::new(|c, _| c.audio.output = true),
        Box::new(|c, _| c.audio.input = true), Box::new(|c, _| c.os = Some(GuestOs::Windows)),
        Box::new(|c, _| c.os = None), Box::new(|c, _| c.identity.product = "other".into()),
        Box::new(|c, _| c.virtio_transport = Transport::Pci),
        Box::new(|c, _| c.serial = Serial::File { path: "must-not-create".into() }),
        Box::new(|c, _| c.clipboard.host_to_guest = true),
        Box::new(|c, _| c.clipboard.guest_to_host = true),
        Box::new(|c, _| c.clipboard.max_bytes += 1),
        Box::new(|c, _| c.display.width += 1),
        Box::new(|c, _| c.nets.push(serde_json::from_value(serde_json::json!({"mode":"nat"})).unwrap())),
        Box::new(|c, _| c.shares.push(vroom_config::Share { path: "absent".into(), tag: "x".into(), readonly: true })),
        Box::new(|c, _| c.usb.push(vroom_config::Usb { id: "1234:5678".into(), name: None, serial: None, mode: Default::default(), readonly: true })),
        Box::new(|c, _| if let Boot::Uefi { firmware, .. } = &mut c.boot { *firmware = None; }),
        Box::new(|c, _| if let Boot::Uefi { secure_boot, .. } = &mut c.boot { *secure_boot = true; }),
        Box::new(|c, _| c.disks.clear()),
        Box::new(|c, _| c.disks.push(c.disks[0].clone())),
        Box::new(|c, _| c.disks[0].readonly = false),
        Box::new(|c, _| c.disks[0].cdrom = true),
        Box::new(|c, _| c.disks[0].bus = DiskBus::Virtio),
        Box::new(|c, _| c.disks[0].format = DiskFormat::Auto),
        Box::new(|c, _| c.disks[0].format = DiskFormat::Qcow2),
    ];
    for mutate in mutations {
        let (mut config, mut opts) = shape(temp.path()); mutate(&mut config, &mut opts);
        assert!(matches!(prepare(&config, TestHv::kvm().as_ref(), &opts), Err(Error::Setup(_))));
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }
    for (arch, name) in [(Arch::Aarch64, "kvm"), (Arch::X86_64, "other")] {
        let hv = TestHv { creates: AtomicU64::new(0), arch, name };
        let (config, opts) = shape(temp.path());
        assert!(matches!(prepare(&config, &hv, &opts), Err(Error::Setup(_))));
    }
    let (config, mut opts) = shape(temp.path());
    let (ui, _peer) = std::os::unix::net::UnixStream::pair().unwrap(); opts.ui = Some(ui);
    assert!(matches!(prepare(&config, TestHv::kvm().as_ref(), &opts), Err(Error::Setup(_))));
}

#[test]
fn regular_type_size_and_alignment_are_checked_before_reading() {
    let temp = tempfile::tempdir().unwrap(); let path = temp.path().join("input");
    for size in [0, 1, 513, 4097] {
        File::create(&path).unwrap().set_len(size).unwrap();
        let result = read_input_with(&path, Bounds { min: 512, max: 4096, alignment: 512 }, None,
            |_, _| panic!("invalid length must not reach reader"));
        assert!(result.is_err());
    }
    std::fs::remove_file(&path).unwrap(); std::fs::create_dir(&path).unwrap();
    assert!(read_input_with(&path, small_bounds(), None, |_, _| panic!("directory read")).is_err());
    std::fs::remove_dir(&path).unwrap();
    let target = temp.path().join("target"); std::fs::write(&target, b"ok").unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(read_input_with(&path, small_bounds(), None, |_, _| panic!("symlink read")).is_err());
    std::fs::remove_file(&path).unwrap();
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // Only a new FIFO in this test's owned temp directory; never read it.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(read_input_with(&path, small_bounds(), None, |_, _| panic!("FIFO read")).is_err());
}

#[test]
fn short_growing_truncated_or_failed_reads_do_not_produce_owned_input() {
    let temp = tempfile::tempdir().unwrap(); let path = temp.path().join("input");
    std::fs::write(&path, vec![7; 1024]).unwrap();
    assert!(read_input_with(&path, small_bounds(), None, |_, _| Ok(vec![7; 1023])).is_err());
    assert!(read_input_with(&path, small_bounds(), None, |_, _| Err(io::Error::other("injected read error"))).is_err());
    for size in [512, 2048, 8192] {
        std::fs::write(&path, vec![7; 1024]).unwrap();
        assert!(read_input_with(&path, small_bounds(), None, |file, limit| {
            OpenOptions::new().write(true).open(&path)?.set_len(size)?;
            read_bytes(file, limit)
        }).is_err());
    }
    std::fs::write(&path, vec![7; 1024]).unwrap();
    assert!(read_input_with(&path, small_bounds(), None, |file, limit| {
        let bytes = read_bytes(file, limit)?;
        OpenOptions::new().write(true).open(&path)?.set_len(2048)?;
        Ok(bytes)
    }).is_err());
    let before = Identity::of(&std::fs::metadata(&path).unwrap());
    let mut changed = before; changed.mtime_nsec ^= 1; assert_ne!(before, changed);
    changed = before; changed.ctime_nsec ^= 1; assert_ne!(before, changed);
}

#[test]
#[cfg(not(feature = "research"))]
fn firmware_pair_size_digest_and_disk_sector_bounds_are_enforced() {
    let temp = tempfile::tempdir().unwrap(); let (config, opts, pins) = fixture(temp.path());
    let hv = TestHv::kvm();
    with_pins(pins, || {
        for (name, size) in [("code", CODE_SIZE), ("vars", VARS_SIZE)] {
            let path = temp.path().join(name); let bytes = std::fs::read(&path).unwrap();
            for wrong in [size - 4096, size + 4096] {
                File::create(&path).unwrap().set_len(wrong).unwrap();
                assert!(prepare(&config, hv.as_ref(), &opts).is_err());
            }
            std::fs::write(&path, &bytes).unwrap();
            OpenOptions::new().write(true).open(&path).unwrap().write_at(&[0], 0).unwrap();
            assert!(matches!(prepare(&config, hv.as_ref(), &opts), Err(Error::Setup(s)) if s.contains("digest")));
            std::fs::write(&path, bytes).unwrap();
        }
        for bad in [0, MIN_DISK_SIZE - 512, MIN_DISK_SIZE + 1, MAX_DISK_SIZE + 512] {
            File::create(temp.path().join("disk")).unwrap().set_len(bad).unwrap();
            assert!(prepare(&config, hv.as_ref(), &opts).is_err());
        }
        File::create(temp.path().join("disk")).unwrap().set_len(MAX_DISK_SIZE).unwrap();
        assert_eq!(prepare(&config, hv.as_ref(), &opts).unwrap().disk.size(), MAX_DISK_SIZE);
    });
    assert_eq!(hv.creates.load(SeqCst), 0);
}

#[test]
#[cfg(not(feature = "research"))]
fn owned_descriptors_and_snapshots_survive_path_and_same_inode_changes() {
    let temp = tempfile::tempdir().unwrap(); let (config, opts, pins) = fixture(temp.path());
    let prepared = with_pins(pins, || prepare(&config, TestHv::kvm().as_ref(), &opts).unwrap());
    for (name, input) in [("code", &prepared.code), ("vars", &prepared.vars), ("disk", &prepared.disk.0)] {
        assert_eq!(Identity::of(&input._file.metadata().unwrap()), input.identity);
        assert!(input._file.write_at(b"no", 0).is_err()); // actually O_RDONLY
        let path = temp.path().join(name); let old = temp.path().join(format!("{name}.old"));
        std::fs::rename(&path, &old).unwrap(); std::fs::write(&path, b"replacement").unwrap();
        assert_eq!(input._file.metadata().unwrap().ino(), std::fs::metadata(&old).unwrap().ino());
        assert_ne!(input._file.metadata().unwrap().ino(), std::fs::metadata(&path).unwrap().ino());
        OpenOptions::new().write(true).open(&old).unwrap().write_at(b"changed original", 0).unwrap();
        assert_eq!(<[u8; 32]>::from(Sha256::digest(&input.bytes)), input.sha256);
    }
    assert_eq!(prepared.layout.ordinary_ram().iter().map(|r| r.1).sum::<u64>(), 527_958_016);
    assert_eq!(prepared.code.bytes[0], 0x51); assert_eq!(prepared.vars.bytes[0], 0x71);
    let mut sector = [0; 512]; prepared.disk.read_at(&mut sector, 0).unwrap(); assert_eq!(sector, [0x91; 512]);
}

#[test]
fn readonly_disk_rejects_mutation_and_checks_full_reads_before_buffer_changes() {
    let temp = tempfile::tempdir().unwrap(); let path = temp.path().join("input");
    let bytes: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect(); std::fs::write(&path, &bytes).unwrap();
    let disk = ReadonlyDisk(read_input(&path, small_bounds(), None).unwrap());
    assert!(disk.read_only()); assert_eq!(disk.size(), 1024); assert!(disk.flush().is_ok());
    let mut out = [0x55; 512]; disk.read_at(&mut out, 512).unwrap(); assert_eq!(&out, &bytes[512..]);
    for offset in [513, 1024, u64::MAX] {
        out.fill(0x55); assert!(disk.read_at(&mut out, offset).is_err()); assert_eq!(out, [0x55; 512]);
    }
    disk.read_at(&mut [], 1024).unwrap(); assert!(disk.read_at(&mut [], 1025).is_err());
    assert!(matches!(disk.write_at(b"x", 0), Err(vroom_block::Error::ReadOnly)));
    assert!(matches!(disk.write_at(&[], u64::MAX), Err(vroom_block::Error::ReadOnly)));
    assert!(matches!(disk.discard(0, 0), Err(vroom_block::Error::ReadOnly)));
    assert!(matches!(disk.write_zeroes(0, 0), Err(vroom_block::Error::ReadOnly)));
    let memory = GuestMemory::new(&[(vroom_memory::GuestAddress(0), 64 << 10)]).unwrap();
    assert!(matches!(disk.write_from_guest(&memory, 0, &[(vroom_memory::GuestAddress(u64::MAX), 1)]), Err(vroom_block::Error::ReadOnly)));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
#[cfg(not(feature = "research"))]
fn valid_owned_inputs_still_fail_public_start_before_all_state_or_vm_effects() {
    let temp = tempfile::tempdir().unwrap(); let (config, opts, pins) = fixture(temp.path());
    let before: Vec<_> = ["code", "vars", "disk"].iter().map(|p| Sha256::digest(std::fs::read(temp.path().join(p)).unwrap())).collect();
    let hv = TestHv::kvm();
    let result = with_pins(pins, || crate::Vmm::start(config, hv.clone(), opts));
    assert!(matches!(result, Err(Error::Unavailable(EXECUTION_DISABLED))));
    assert_eq!(hv.creates.load(SeqCst), 0);
    assert!(!temp.path().join("state").exists());
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 3);
    for (p, digest) in ["code", "vars", "disk"].iter().zip(before) {
        assert_eq!(Sha256::digest(std::fs::read(temp.path().join(p)).unwrap()), digest);
    }
    assert_eq!(super::pins().code, CODE_SHA256); // thread-scoped test override restored
}

#[test]
fn preflight_never_creates_or_replaces_persistent_vars() {
    let temp = tempfile::tempdir().unwrap(); let (mut config, opts, pins) = fixture(temp.path());
    let path = temp.path().join("vars"); std::fs::remove_file(&path).unwrap();
    with_pins(pins, || assert!(prepare(&config, TestHv::kvm().as_ref(), &opts).is_err()));
    assert!(!path.exists()); assert!(!temp.path().join("vars.bak").exists());
    std::fs::write(&path, b"existing unrelated persistent vars").unwrap();
    if let Boot::Uefi { secure_boot, .. } = &mut config.boot { *secure_boot = true; }
    assert!(prepare(&config, TestHv::kvm().as_ref(), &opts).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"existing unrelated persistent vars");
    assert!(!temp.path().join("vars.bak").exists());
}

#[test]
fn diagnostic_debug_contains_identity_and_hashes_but_never_input_bytes() {
    let temp = tempfile::tempdir().unwrap(); let path = temp.path().join("input");
    std::fs::write(&path, b"PRIVATE_PAYLOAD_MUST_NOT_BE_LOGGED").unwrap();
    let input = read_input(&path, small_bounds(), None).unwrap();
    let debug = format!("{input:?}");
    assert!(debug.contains("sha256")); assert!(!debug.contains("PRIVATE_PAYLOAD"));
    assert!(!debug.contains(&path.display().to_string()));
}

//! Owned inputs for a future, volatile first-EFI board. Execution is disabled.
//!
//! This module opens only bounded read-only inputs. It never creates VARS,
//! state directories, devices, helpers or a VM. The existing ARM firmware
//! module owns its separate persistent-VARS behavior.

use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use vroom_arch::x86_64::Q35FirmwareLayout;
use vroom_block::{DiskImage, GuestMemory, GuestSegs};
use vroom_config::{Boot, DiskBus, DiskFormat, Display, GuestOs, Serial, Transport, VmConfig};
use vroom_hv::{Arch, Hypervisor};

use crate::{Error, Result, StartMode, VmmOptions};

pub(crate) const EXECUTION_DISABLED: &str = "native x64 firmware execution is not enabled";
const CODE_SIZE: u64 = 3_653_632;
const VARS_SIZE: u64 = 540_672;
const MIN_DISK_SIZE: u64 = 1 << 20;
const MAX_DISK_SIZE: u64 = 32 << 20;

// The selected SMM-requiring QEMU 11.1.2 OVMF pair. The blank template is
// SetupMode, not enrolled Secure Boot. No caller can select different pins.
const CODE_SHA256: [u8; 32] = [
    0x32, 0x80, 0x76, 0x82, 0xa9, 0xe5, 0xc0, 0xe2, 0xd1, 0x92, 0xec, 0xb6, 0x07, 0x79, 0x41, 0xd4,
    0xbe, 0x8b, 0xcf, 0xa6, 0xb9, 0xcd, 0x6e, 0x60, 0x83, 0x0c, 0x3a, 0x9f, 0x48, 0xc0, 0x55, 0x5a,
];
const VARS_SHA256: [u8; 32] = [
    0x5d, 0x2a, 0xc3, 0x83, 0x37, 0x1b, 0x40, 0x83, 0x98, 0xac, 0xce, 0xe7, 0xec, 0x27, 0xc8, 0xc0,
    0x9e, 0xa5, 0xb7, 0x4a, 0x0d, 0xe0, 0xce, 0xea, 0x65, 0x13, 0x38, 0x8b, 0x15, 0xbe, 0x5d, 0x1e,
];

#[derive(Clone, Copy)]
struct Pins { code: [u8; 32], vars: [u8; 32] }

fn pins() -> Pins {
    let selected = Pins { code: CODE_SHA256, vars: VARS_SHA256 };
    #[cfg(test)]
    let selected = TEST_PINS.with(|value| value.get().unwrap_or(selected));
    selected
}

// Deterministic synthetic files can exercise the public early rejection in
// unit tests. This seam has no production code, environment or config input.
#[cfg(test)]
std::thread_local! {
    static TEST_PINS: std::cell::Cell<Option<Pins>> = const { std::cell::Cell::new(None) };
}

fn invalid(why: &'static str) -> Error {
    Error::Setup(format!("x64 firmware preflight: {why}"))
}

/// Descriptor identity before and after the bounded read. These observations
/// detect ordinary mutation; they are not a lock against an external writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Identity {
    dev: u64, ino: u64, len: u64,
    mtime: i64, mtime_nsec: i64, ctime: i64, ctime_nsec: i64,
}

impl Identity {
    fn of(m: &Metadata) -> Self {
        Self { dev: m.dev(), ino: m.ino(), len: m.len(), mtime: m.mtime(),
            mtime_nsec: m.mtime_nsec(), ctime: m.ctime(), ctime_nsec: m.ctime_nsec() }
    }
}

struct OwnedInput {
    // Retain the exact original read-only descriptor; later consumers use
    // bytes, never reread this descriptor or reopen a pathname.
    _file: File,
    identity: Identity,
    bytes: Arc<[u8]>,
    sha256: [u8; 32],
}

impl std::fmt::Debug for OwnedInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedInput").field("identity", &self.identity)
            .field("length", &self.bytes.len()).field("sha256", &self.sha256).finish()
    }
}

#[derive(Clone, Copy)]
struct Bounds { min: u64, max: u64, alignment: u64 }

fn read_bytes(file: &mut File, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn read_input(path: &Path, bounds: Bounds, digest: Option<[u8; 32]>) -> Result<OwnedInput> {
    read_input_with(path, bounds, digest, read_bytes)
}

fn read_input_with(
    path: &Path, bounds: Bounds, digest: Option<[u8; 32]>,
    read: impl FnOnce(&mut File, u64) -> io::Result<Vec<u8>>,
) -> Result<OwnedInput> {
    // O_NONBLOCK avoids waiting on a FIFO before fstat rejects its type.
    let mut file = OpenOptions::new().read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW).open(path)?;
    let before = file.metadata()?;
    if !before.is_file() || before.len() < bounds.min || before.len() > bounds.max
        || before.len() % bounds.alignment != 0 {
        return Err(invalid("input must be a regular file with the required bounded size/alignment"));
    }
    let identity = Identity::of(&before);
    let bytes = read(&mut file, bounds.max)?;
    let after = file.metadata()?;
    if !after.is_file() || Identity::of(&after) != identity
        || bytes.len() as u64 != identity.len || bytes.len() as u64 > bounds.max {
        return Err(invalid("input changed during its bounded read"));
    }
    let sha256 = Sha256::digest(&bytes).into();
    if digest.is_some_and(|expected| expected != sha256) {
        return Err(invalid("firmware digest does not match the selected image pair"));
    }
    Ok(OwnedInput { _file: file, identity, bytes: bytes.into(), sha256 })
}

/// Immutable raw storage, built once from the checked descriptor. In
/// particular, same-inode external writes cannot change the guest's bytes.
struct ReadonlyDisk(OwnedInput);

impl std::fmt::Debug for ReadonlyDisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ReadonlyDisk").field(&self.0).finish()
    }
}

impl DiskImage for ReadonlyDisk {
    fn size(&self) -> u64 { self.0.bytes.len() as u64 }
    fn read_only(&self) -> bool { true }
    fn read_at(&self, out: &mut [u8], offset: u64) -> vroom_block::Result<()> {
        vroom_block::check_range(self.size(), offset, out.len() as u64)?;
        let start = offset as usize; // checked against the <=32 MiB backing
        out.copy_from_slice(&self.0.bytes[start..start + out.len()]);
        Ok(())
    }
    fn write_at(&self, _: &[u8], _: u64) -> vroom_block::Result<()> { Err(vroom_block::Error::ReadOnly) }
    fn write_from_guest(&self, _: &GuestMemory, _: u64, _: &GuestSegs) -> vroom_block::Result<()> {
        Err(vroom_block::Error::ReadOnly)
    }
    fn discard(&self, _: u64, _: u64) -> vroom_block::Result<()> { Err(vroom_block::Error::ReadOnly) }
    fn write_zeroes(&self, _: u64, _: u64) -> vroom_block::Result<()> { Err(vroom_block::Error::ReadOnly) }
    fn flush(&self) -> vroom_block::Result<()> { Ok(()) }
}

/// All fields are private. The gate in machine_x86 currently drops this
/// without building anything; a later reviewed board must consume these
/// exact owners and must not open the paths again.
pub(crate) struct PreparedFirmware {
    code: OwnedInput,
    vars: OwnedInput,
    disk: Arc<ReadonlyDisk>,
    layout: Q35FirmwareLayout,
}

impl std::fmt::Debug for PreparedFirmware {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedFirmware").field("code", &self.code).field("vars", &self.vars)
            .field("disk", &self.disk).field("layout", &self.layout).finish()
    }
}

pub(crate) fn prepare(config: &VmConfig, hv: &dyn Hypervisor, opts: &VmmOptions) -> Result<PreparedFirmware> {
    if hv.arch() != Arch::X86_64 || hv.name() != "kvm" || cfg!(feature = "research") {
        return Err(invalid("requires x86_64 KVM and the default feature set"));
    }
    if config.cpus != 1 || config.memory_mib != 512 || !matches!(opts.start, StartMode::BootPaused)
        || !opts.sandbox || !config.security.sandbox {
        return Err(invalid("requires one CPU, 512 MiB, BootPaused and sandboxing"));
    }
    if config.nested || !config.nets.is_empty() || !config.shares.is_empty() || !config.usb.is_empty()
        || config.tpm || config.balloon || config.camera || config.audio != Default::default() {
        return Err(invalid("extra devices and nested virtualization are unavailable"));
    }
    if config.display != Display::default() || opts.ui.is_some() || opts.clipboard.is_some()
        || config.clipboard.host_to_guest || config.clipboard.guest_to_host
        || config.clipboard.max_bytes != vroom_config::Clipboard::default().max_bytes {
        return Err(invalid("requires default headless display settings and disabled clipboard"));
    }
    if config.os != Some(GuestOs::Other) || config.identity != Default::default()
        || config.virtio_transport != Transport::Auto || config.serial != Serial::Stdio {
        return Err(invalid("requires explicit other OS, default identity/transport and stdio serial"));
    }
    let Boot::Uefi { firmware: Some(code_path), vars: vars_path, secure_boot: false } = &config.boot else {
        return Err(invalid("requires explicit CODE and existing blank VARS input, without Secure Boot enrollment"));
    };
    let [disk_config] = config.disks.as_slice() else {
        return Err(invalid("requires exactly one read-only raw NVMe disk input"));
    };
    if !disk_config.readonly || disk_config.cdrom || disk_config.bus != DiskBus::Nvme
        || disk_config.format != DiskFormat::Raw {
        return Err(invalid("requires exactly one read-only raw NVMe disk input"));
    }
    // Finish every shape check before opening any input. There is no VARS
    // creation, firmware search path, image format sniffing or disk overlay.
    let pins = pins();
    let code = read_input(code_path, Bounds { min: CODE_SIZE, max: CODE_SIZE, alignment: 4096 }, Some(pins.code))?;
    let vars = read_input(vars_path, Bounds { min: VARS_SIZE, max: VARS_SIZE, alignment: 4096 }, Some(pins.vars))?;
    let disk = Arc::new(ReadonlyDisk(read_input(&disk_config.path,
        Bounds { min: MIN_DISK_SIZE, max: MAX_DISK_SIZE, alignment: 512 }, None)?));
    let layout = Q35FirmwareLayout::for_images(code.bytes.len(), vars.bytes.len())?;
    Ok(PreparedFirmware { code, vars, disk, layout })
}

#[cfg(test)]
#[path = "firmware_x86_tests.rs"]
mod tests;

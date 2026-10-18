//! Assemble a machine from a config: memory, irqchip, devices, vCPUs.

use std::io::Write;
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};

use vroom_arch::aarch64::{self as board, layout};
use vroom_config::{Boot, DiskBus, Gpu, GuestOs, SoundDevice, VmConfig};
use vroom_devices::audio::{AudioSink, AudioSource, NullSink, Silence};
use vroom_devices::hda::{codec::CodecIds, Hda, HdaIds};
use vroom_virtio::devices::sound::Sound;
use vroom_devices::{pl011::Pl011, pl031::Pl031, BusBuilder, BusDevice, ConcurrentBusDevice};
use vroom_hv::{
    aarch64::mpidr_for, BootRegs, GicMsiConfig, Hypervisor, IrqChipConfig, IrqChipInfo, Vm, VmParams,
};
use vroom_memory::{GuestAddress, GuestMemory};
use vroom_snapshot::Snapshottable;
use vroom_virtio::devices::gpu::{DisplaySink, Gpu as GpuDev, GpuControl, ThreeD};
use vroom_virtio::devices::gpu3d::{Mapping, Renderer3d, ShmMapper};
use vroom_virtio::devices::input::{Input, InputKind};
use vroom_virtio::devices::{block::Block, console::Console, fs::Fs, net::Net, rng::Rng};
use vroom_devices::nvme::{Namespace, Nvme};
use vroom_devices::usb::hid::UsbIds;
use vroom_devices::usb::xhci::Xhci;
use vroom_devices::pci::{Intx, IntxRouter, MsiSender, PciRoot, RootPort, SharedPciDevice, VendorIds};
use vroom_devices::IrqLine;
use vroom_virtio::mmio::{ShmRegion, VirtioMmio};
use vroom_virtio::{pci::VirtioPci, VirtioDevice};

use crate::agent::{self, Agent, ClipboardProvider};
use crate::disks::DiskSet;
use crate::display::{DisplayServer, FbSink, InputPipes, ViewerTx};
use crate::hotplug::{self, Hotplug};
use crate::irq::{GicMsi, SpiLine};
use crate::psci::CpuTable;
use crate::vcpu::{self, VcpuHandle, VcpuShared, VmEvent};
use crate::{Error, Result, StatefulDevice};

/// Hands out virtio-mmio slots in order.
#[derive(Default)]
struct VirtioSlots {
    next: u32,
}

impl VirtioSlots {
    fn next(&mut self) -> Result<(u64, u32)> {
        if self.next >= layout::VIRTIO_MMIO_SLOTS {
            return Err(Error::Setup("out of virtio-mmio slots".into()));
        }
        let s = layout::virtio_mmio_slot(self.next);
        self.next += 1;
        Ok(s)
    }
}

/// Device attachment: owns the bus registry, virtio-mmio and PCI slot
/// allocation, and interrupt wiring, so callers just say "add this".
struct Board {
    vm: Arc<dyn Vm>,
    mem: GuestMemory,
    reg: DeviceRegistry,
    fdt: Vec<board::FdtDevice>,
    mmio_slots: VirtioSlots,
    /// Next free spot in the shared memory window area.
    shm_next: u64,
    use_pci: bool,
    pci: Vec<(u8, SharedPciDevice)>,
    next_pci_slot: u8,
    intx: Arc<IntxRouter>,
    msi: Arc<GicMsi>,
    /// Devices a resumed VM had in hot plug slots: (port, device).
    hot: Vec<(usize, Box<dyn VirtioDevice>)>,
}

impl Board {
    fn new(vm: Arc<dyn Vm>, mem: GuestMemory, use_pci: bool) -> Self {
        let intx = IntxRouter::new(
            (0..4).map(|i| SpiLine::new(vm.clone(), layout::PCI_INTX_SPI + i) as Arc<dyn IrqLine>).collect(),
        );
        let msi = GicMsi::new(vm.clone());
        Board {
            vm,
            mem,
            reg: DeviceRegistry { bus: BusBuilder::new(), stateful: Vec::new() },
            fdt: Vec::new(),
            mmio_slots: VirtioSlots::default(),
            shm_next: layout::SHM_BASE,
            use_pci,
            pci: Vec::new(),
            next_pci_slot: 1,
            intx,
            msi,
            hot: Vec::new(),
        }
    }

    /// Add a PCI function; `make` gets its slot and interrupt wiring.
    fn add_pci(
        &mut self,
        make: impl FnOnce(u8, Arc<Intx>, Arc<dyn MsiSender>) -> SharedPciDevice,
    ) -> Result<u8> {
        let slot = self.next_pci_slot;
        if slot > 31 {
            return Err(Error::Setup("out of PCI slots".into()));
        }
        self.next_pci_slot += 1;
        let dev = make(slot, self.intx.pin(slot, 1), self.msi.clone());
        self.pci.push((slot, dev));
        Ok(slot)
    }

    /// virtio device on whichever transport this machine uses.
    fn add_virtio(&mut self, dev: Box<dyn VirtioDevice>) -> Result<()> {
        if self.use_pci {
            let mem = self.mem.clone();
            self.add_pci(move |slot, intx, msi| Arc::new(Mutex::new(VirtioPci::new(slot, dev, mem, intx, msi))))?;
            return Ok(());
        }
        let (base, spi) = self.mmio_slots.next()?;
        let t = VirtioMmio::new(base, dev, self.mem.clone(), SpiLine::new(self.vm.clone(), spi));
        let mut regions = Vec::new();
        for (id, len) in t.shm_wanted() {
            // 1 GiB aligned windows, all inside the reserved area.
            let at = self.shm_next.next_multiple_of(1 << 30);
            let end = at.checked_add(len).filter(|&e| e <= layout::SHM_BASE + layout::SHM_SIZE);
            let Some(end) = end else { return Err(Error::Setup(format!("no room for a {len:#x} shm window"))) };
            regions.push(ShmRegion { id, base: at, len });
            self.shm_next = end;
        }
        let t = t.with_shm(regions);
        self.reg.add(base, layout::VIRTIO_MMIO_SIZE, Arc::new(Mutex::new(t)))?;
        self.fdt.push(board::FdtDevice::VirtioMmio { base, size: layout::VIRTIO_MMIO_SIZE, spi });
        Ok(())
    }

    /// Put the root complex on the bus if anything needs it. A machine on
    /// PCI gets the hot plug root ports, last (the other devices keep
    /// their slots), with a resumed VM's devices in them.
    fn finish_pci(&mut self, host_ids: VendorIds, port_ids: VendorIds) -> Result<Option<Arc<PciRoot>>> {
        if self.pci.is_empty() && !self.use_pci {
            return Ok(None);
        }
        let mut ports = Vec::new();
        if self.use_pci {
            let mut hot = std::mem::take(&mut self.hot);
            for i in 0..hotplug::PORTS {
                let slot = self.next_pci_slot;
                if slot > 31 {
                    log::warn!("pci: no slot left for hot plug port {}", i + 1);
                    break;
                }
                self.next_pci_slot += 1;
                let mut port = RootPort::new(slot, i as u8, port_ids, self.intx.pin(slot, 1), self.msi.clone());
                if let Some(k) = hot.iter().position(|(p, _)| *p == i) {
                    let (_, dev) = hot.remove(k);
                    let f = hotplug::function_in(slot, i, dev, &self.mem, &self.msi, &self.intx);
                    let pristine = f.lock().unwrap_or_else(|e| e.into_inner()).save()?;
                    port.plug(f, pristine, true).map_err(|e| Error::Setup(e.into()))?;
                }
                ports.push((slot, Arc::new(Mutex::new(port))));
            }
            if !hot.is_empty() {
                return Err(Error::Setup("the snapshot has a device in a hot plug slot this VM doesn't have".into()));
            }
        }
        let root = PciRoot::with_ports(std::mem::take(&mut self.pci), host_ids, ports);
        self.reg.add(layout::PCI_ECAM_BASE, layout::PCI_ECAM_SIZE, Arc::new(Mutex::new(root.ecam_port())))?;
        self.reg.add_concurrent(layout::PCI_MMIO_BASE, layout::PCI_MMIO_SIZE, Arc::new(root.window_port(layout::PCI_MMIO_BASE)))?;
        self.reg.add_concurrent(layout::PCI_MMIO64_BASE, layout::PCI_MMIO64_SIZE, Arc::new(root.window_port(layout::PCI_MMIO64_BASE)))?;
        self.fdt.push(board::FdtDevice::PciEcam {
            ecam_base: layout::PCI_ECAM_BASE,
            ecam_size: layout::PCI_ECAM_SIZE,
            buses: (layout::PCI_ECAM_SIZE >> 20) as u32,
        });
        Ok(Some(root))
    }
}

pub(crate) struct Machine {
    pub vm: Arc<dyn Vm>,
    pub mem: GuestMemory,
    pub shared: Arc<VcpuShared>,
    pub vcpus: Vec<VcpuHandle>,
    pub devices: Vec<StatefulDevice>,
    pub uart: Arc<Mutex<Pl011>>,
    pub agent: Agent,
    /// Each virtio-gpu's control handle (window size, Direct3D state).
    pub gpus: Vec<GpuControl>,
    pub viewer: Option<ViewerTx>,
    /// Every monitor, the first (with the firmware screen) first.
    pub screens: Vec<Arc<FbSink>>,
    pub boot: BootImage,
    /// The network adapters built (a resumed VM: the ones it was saved
    /// with), as the VM file had them: what a later save records.
    pub nets: Vec<vroom_config::Net>,
    /// The adapters while the VM runs (and those plugged in since).
    pub net_ports: Arc<crate::net::NetPorts>,
    /// The hot plug slots (a VM on PCI).
    pub hotplug: Option<Arc<Hotplug>>,
}

/// Registers MMIO devices on the bus and in the snapshot list in one go,
/// so nothing can be on the bus but missing from snapshots.
pub(crate) struct DeviceRegistry {
    bus: BusBuilder,
    stateful: Vec<StatefulDevice>,
}

impl DeviceRegistry {
    pub fn add<T>(&mut self, base: u64, len: u64, dev: Arc<Mutex<T>>) -> Result<()>
    where
        T: BusDevice + Snapshottable + Send + 'static,
    {
        self.bus.insert(base, len, dev.clone()).map_err(Error::Setup)?;
        self.stateful.push(dev);
        Ok(())
    }

    /// On the bus, but its state lives elsewhere (the emulated MSI frame).
    pub fn add_bus_only<T: BusDevice + 'static>(&mut self, base: u64, len: u64, dev: T) -> Result<()> {
        self.bus.insert(base, len, Arc::new(Mutex::new(dev))).map_err(Error::Setup)
    }

    /// On the bus without a lock of its own; its state lives elsewhere
    /// (PCI windows: the devices behind them are snapshotted by the root).
    pub fn add_concurrent(&mut self, base: u64, len: u64, dev: Arc<dyn ConcurrentBusDevice>) -> Result<()> {
        self.bus.insert_concurrent(base, len, dev).map_err(Error::Setup)
    }
}

/// Host-side facts the board needs.
pub(crate) struct BuildEnv {
    /// SMBIOS system UUID.
    pub uuid: [u8; 16],
    pub state_dir: std::path::PathBuf,
    /// Restoring a snapshot / suspend image rather than cold booting.
    pub resuming: bool,
    /// The config the snapshot was taken with, when restoring: its
    /// network adapters are the guest's.
    pub saved: Option<VmConfig>,
    /// Network adapters get a control queue (not when the snapshot's had
    /// none).
    pub net_ctrl: bool,
    /// The VMM will lock itself down (what NAT may reach later).
    pub sandbox: bool,
    /// Removable shares for USB disks' files (`usb::FILE_SLOTS` on a
    /// Mac; a resumed VM: as many as it was saved with).
    pub usb_file_slots: usize,
    /// A resumed VM's network adapters in hot plug slots: adapter index
    /// to port.
    pub hot_nets: std::collections::BTreeMap<usize, usize>,
    /// Windows' 3D GPU gets memory in BAR 0 (a resumed VM: if it had it).
    pub gpu3d_vram: bool,
}

/// What gets loaded into RAM at (re)boot. Kept so reset can reload it.
pub(crate) enum BootImage {
    /// Direct Linux boot. The kernel stays as read from its file (often
    /// compressed, a third of its size) and is unpacked at each load.
    Kernel { kernel: Vec<u8>, initrd: Option<Vec<u8>>, ram: (u64, u64), fdt_cfg: board::FdtConfig },
    /// UEFI in flash; the FDT goes at the start of RAM for it to find.
    Firmware { fdt_cfg: board::FdtConfig },
}

impl BootImage {
    /// Put boot blobs into RAM. Returns boot CPU regs.
    pub fn load(&self, mem: &GuestMemory) -> Result<BootRegs> {
        match self {
            BootImage::Kernel { kernel, initrd, ram, fdt_cfg } => {
                let kernel = vroom_arch::decompress_if_needed(kernel.clone())?;
                let k = board::load_kernel(mem, ram.0, &kernel)?;
                let plan = board::place_boot_blobs(*ram, k.end, initrd.as_ref().map(|i| i.len() as u64))?;
                let mut fdt_cfg = fdt_cfg.clone();
                if let (Some(addr), Some(data)) = (plan.initrd_addr, initrd) {
                    board::load_initrd(mem, addr, data)?;
                    fdt_cfg.initrd = Some((addr, data.len() as u64));
                }
                let fdt = board::create_fdt(&fdt_cfg)?;
                mem.write(GuestAddress(plan.fdt_addr), &fdt)?;
                Ok(BootRegs::Aarch64 { pc: k.entry, x: [plan.fdt_addr, 0, 0, 0] })
            }
            BootImage::Firmware { fdt_cfg } => {
                let fdt = board::create_fdt(fdt_cfg)?;
                mem.write(GuestAddress(layout::RAM_BASE), &fdt)?;
                Ok(BootRegs::Aarch64 { pc: layout::FLASH0_BASE, x: [layout::RAM_BASE, 0, 0, 0] })
            }
        }
    }
}

/// Where the app finds a VM whose window it lost: a socket in the user's
/// own temp dir (socket paths can't be long, state dirs can), named after
/// the state dir, whose `viewer` file says where it is.
pub(crate) const VIEWER_FILE: &str = "viewer";

fn viewer_socket(state_dir: &std::path::Path) -> std::io::Result<std::os::unix::net::UnixListener> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    let dir = std::env::temp_dir().join("vroom");
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let real = std::fs::canonicalize(state_dir).unwrap_or_else(|_| state_dir.to_path_buf());
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in real.as_os_str().as_bytes() {
        h = (h ^ *b as u64).wrapping_mul(0x100_0000_01b3);
    }
    let p = dir.join(format!("{h:016x}.sock"));
    // One left by a VMM that died: we hold the state lock, so it's stale.
    if let Ok(m) = std::fs::symlink_metadata(&p) {
        if !m.file_type().is_socket() {
            return Err(std::io::Error::other(format!("{} is not a socket", p.display())));
        }
        std::fs::remove_file(&p)?;
    }
    let l = std::os::unix::net::UnixListener::bind(&p)?;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(state_dir.join(VIEWER_FILE), p.as_os_str().as_bytes())?;
    Ok(l)
}

/// Host-visible window size for GPU buffers (address space, not memory).
const GPU_SHM_SIZE: u64 = 8 << 30;
/// Windows' extra GPUs: the most its 2D driver's framebuffer takes
/// (16 MiB, `agent::WINDOWS_MAX_PIXELS`), with room. Only what the guest
/// writes is ever memory.
const EXTRA_GPU_VRAM: u64 = 32 << 20;
/// Windows' 3D GPU: when its driver stops (vroom turns it off for a save)
/// it hands Windows' basic display driver a 1024x768 framebuffer at BAR 0
/// (3 MiB). Under 16 MiB, which the driver would take as video memory.
const GPU3D_VRAM: u64 = 8 << 20;

/// Whether the renderer runs in the VMM (`VROOM_GPU_IN_PROCESS=1`, for
/// debugging, and off macOS) rather than in a GPU process of its own.
pub fn gpu_in_process() -> bool {
    !cfg!(target_os = "macos") || std::env::var_os("VROOM_GPU_IN_PROCESS").is_some_and(|v| v != "0")
}

/// Venus, Direct3D 11 and GL (if asked) through virglrenderer, in the GPU
/// process (or in here, see [`gpu_in_process`]), or a clear error.
fn three_d(vm: &Arc<dyn Vm>, config: &VmConfig, mem: &GuestMemory, state_dir: &std::path::Path) -> Result<ThreeD> {
    let r: Box<dyn Renderer3d> = if gpu_in_process() { in_process(vm, config)? } else { gpu_process(vm, config, mem, state_dir)? };
    if config.display.opengl && !r.capsets().iter().any(|c| c.0 == 2) {
        log::warn!("gpu: no OpenGL for the guest (opengl_host = \"angle\" needs ANGLE next to virglrenderer: scripts/build-angle.sh); Vulkan only");
    }
    Ok(ThreeD::new(r, Arc::new(VmShm { vm: vm.clone(), filler: Mutex::new(None) }), GPU_SHM_SIZE))
}

const NO_VIRGL: &str =
    "gpu = \"virtio3d\" needs virglrenderer with Venus (scripts/build-virglrenderer.sh, or brew install slp/krunkit/virglrenderer)";

fn in_process(vm: &Arc<dyn Vm>, config: &VmConfig) -> Result<Box<dyn Renderer3d>> {
    #[cfg(target_os = "macos")]
    let gl = config.display.opengl.then_some(match config.display.opengl_host {
        vroom_config::OpenglHost::Angle => vroom_gpu3d::GlHost::Angle,
        vroom_config::OpenglHost::Apple => vroom_gpu3d::GlHost::Apple,
    });
    #[cfg(not(target_os = "macos"))]
    let gl = config.display.opengl;
    if !vroom_gpu3d::load(gl) {
        return Err(Error::Setup(NO_VIRGL.into()));
    }
    vroom_gpu3d::set_map_granule(vm.map_granule());
    // Venus and Direct3D contexts still get processes of their own.
    #[cfg(target_os = "macos")]
    if let Err(e) = std::env::current_exe().and_then(|exe| vroom_gpu3d::server::start(&exe)) {
        log::warn!("gpu: no render server ({e}): Venus runs in here, no Direct3D");
    }
    let r = vroom_gpu3d::Virgl::new(config.identity.gl_renderer.clone())
        .ok_or_else(|| Error::Setup("virglrenderer: Venus init failed (no Vulkan driver?)".into()))?;
    log::warn!("gpu: the 3D renderer runs inside the VMM (VROOM_GPU_IN_PROCESS)");
    Ok(Box::new(r))
}

/// The GPU process: `vroom gpu-helper`, sandboxed on its own (the GPU,
/// guest RAM, its shader cache; no files, network or hypervisor). The
/// VMM then never loads virglrenderer, GL, MoltenVK or DXMT.
#[cfg(unix)]
fn gpu_process(vm: &Arc<dyn Vm>, config: &VmConfig, mem: &GuestMemory, state_dir: &std::path::Path) -> Result<Box<dyn Renderer3d>> {
    let exe = std::env::current_exe().map_err(|e| Error::Setup(format!("gpu process: {e}")))?;
    let shaders = state_dir.join("shaders");
    std::fs::create_dir_all(&shaders).map_err(|e| Error::Setup(format!("{}: {e}", shaders.display())))?;
    let gl = config.display.opengl.then_some(match config.display.opengl_host {
        vroom_config::OpenglHost::Angle => "angle",
        vroom_config::OpenglHost::Apple => "apple",
    });
    let args = vroom_gpu3d::remote::HelperArgs {
        exe: &exe,
        gl,
        gl_renderer: config.identity.gl_renderer.as_deref(),
        map_granule: vm.map_granule(),
        shaders: &shaders,
    };
    let r = vroom_gpu3d::remote::Remote::spawn(&args, mem.clone()).map_err(|e| {
        Error::Setup(format!("gpu process: {e} (3D needs virglrenderer: scripts/build-virglrenderer.sh; RUST_LOG=info shows more)"))
    })?;
    crate::note_helper(r.pid());
    Ok(Box::new(r))
}

#[cfg(not(unix))]
fn gpu_process(vm: &Arc<dyn Vm>, config: &VmConfig, _mem: &GuestMemory, _state_dir: &std::path::Path) -> Result<Box<dyn Renderer3d>> {
    in_process(vm, config)
}

/// Maps GPU buffers into the guest with the hypervisor.
/// Where no buffer is, the window is backed by anonymous memory of ours
/// (reserved, only what the guest touches is real), so it behaves like a
/// card's memory: Windows reads and writes buffers it already gave back
/// (when its 3D driver stops, for one), and a hole there stopped the VM.
struct VmShm {
    vm: Arc<dyn Vm>,
    /// (window base, host memory, size) once the guest placed the window.
    filler: Mutex<Option<(u64, usize, u64)>>,
}

impl VmShm {
    /// Put the filler back under `gpa..gpa + size` (inside the window).
    fn fill(&self, gpa: u64, size: u64) {
        let Some((base, host, len)) = *self.filler.lock().unwrap_or_else(|e| e.into_inner()) else { return };
        if gpa < base || gpa.saturating_add(size) > base.saturating_add(len) {
            return;
        }
        let ptr = (host + (gpa - base) as usize) as *mut u8;
        // SAFETY: a range of our own reservation, kept for the VM's life.
        if let Err(e) = unsafe { self.vm.map_host(gpa, ptr, size) } {
            log::warn!("gpu: window {gpa:#x}: {e}");
        }
    }
}

impl ShmMapper for VmShm {
    fn map(&self, gpa: u64, m: &Mapping) -> bool {
        // The filler goes first (an unmapped range is fine to unmap).
        let _ = self.vm.unmap_host(gpa, m.size);
        // SAFETY: the renderer keeps the buffer mapped until unmap_blob,
        // which the GPU device only calls after `unmap` below.
        match unsafe { self.vm.map_host(gpa, m.ptr, m.size) } {
            Ok(()) => true,
            Err(e) => {
                log::warn!("gpu: map {gpa:#x}: {e}");
                self.fill(gpa, m.size);
                false
            }
        }
    }

    fn unmap(&self, gpa: u64, size: u64) {
        if let Err(e) = self.vm.unmap_host(gpa, size) {
            log::warn!("gpu: unmap {gpa:#x}: {e}");
        }
        self.fill(gpa, size);
    }

    fn granule(&self) -> u64 {
        self.vm.map_granule()
    }

    fn place(&self, gpa: u64, size: u64) {
        // A BAR sits at a multiple of its size. Anything else is the
        // firmware sizing it (all ones) or halfway through moving it.
        if gpa == 0 || size == 0 || !gpa.is_multiple_of(size) || gpa.checked_add(size).is_none() {
            return;
        }
        // Halfway through sizing a 64-bit BAR (all ones in its low half
        // only) a small BAR sits just under 4 GiB, which is RAM: the
        // filler went there, and moving it on unmapped the guest's RAM.
        let inside = |base: u64, len: u64| gpa >= base && gpa + size <= base + len;
        if !inside(layout::PCI_MMIO_BASE, layout::PCI_MMIO_SIZE) && !inside(layout::PCI_MMIO64_BASE, layout::PCI_MMIO64_SIZE) {
            return;
        }
        let mut f = self.filler.lock().unwrap_or_else(|e| e.into_inner());
        let host = match *f {
            Some((old, host, len)) if len == size => {
                if old == gpa {
                    return;
                }
                let _ = self.vm.unmap_host(old, len);
                host
            }
            Some(_) => return,
            None => {
                // SAFETY: a fresh private reservation; nothing else uses it.
                let p = unsafe {
                    libc::mmap(std::ptr::null_mut(), size as usize, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE, -1, 0)
                };
                if p == libc::MAP_FAILED {
                    log::warn!("gpu: can't reserve the window's backing: {}", std::io::Error::last_os_error());
                    return;
                }
                p as usize
            }
        };
        *f = Some((gpa, host, size));
        drop(f);
        self.fill(gpa, size);
    }
}

/// A `*_pci_id` from `[identity]` (the config checked it already).
fn vendor_ids(s: &str) -> Result<VendorIds> {
    let [vendor, device, subsys_vendor, subsys] =
        vroom_config::pci_ids(s).ok_or_else(|| Error::Setup(format!("bad PCI id {s:?}")))?;
    Ok(VendorIds { vendor, device, subsys_vendor, subsys })
}

/// The sound card `[audio]` asks for, fed by the audio helper. Returns
/// the helper (its microphone gate follows the VM window).
fn sound_card(
    board: &mut Board,
    config: &VmConfig,
    dev: SoundDevice,
    ui: Option<&DisplayServer>,
) -> Result<Arc<crate::audio::AudioHelper>> {
    use crate::audio::{AudioHelper, HelperSink, HelperSource};
    // VROOM_AUDIO_WAV_DIR: test mode, record to / from WAV files instead
    // of the speakers and the microphone.
    let wav = std::env::var_os("VROOM_AUDIO_WAV_DIR").map(std::path::PathBuf::from);
    let (output, input) = (config.audio.output, config.audio.input);
    let helper = AudioHelper::spawn(wav.as_deref(), input)?;
    if let Some(ds) = ui {
        helper.set_viewer(ds.tx());
        helper.set_window(true);
    }
    let h = helper.clone();
    let make_sink = move || -> Box<dyn AudioSink> {
        if output {
            Box::new(HelperSink(h.clone()))
        } else {
            Box::new(NullSink)
        }
    };
    let h = helper.clone();
    let make_source = move || -> Box<dyn AudioSource> { Box::new(HelperSource(h.clone())) };
    match dev {
        SoundDevice::Hda => {
            let ident = &config.identity;
            let [vendor, device, sv, sd] = vroom_config::pci_ids(&ident.hda_codec_id)
                .ok_or_else(|| Error::Setup(format!("bad HD Audio codec id {:?}", ident.hda_codec_id)))?;
            let ids = HdaIds {
                pci: vendor_ids(&ident.hda_pci_id)?,
                codec: CodecIds { vendor, device, subsys: (sv as u32) << 16 | sd as u32 },
            };
            let mem = board.mem.clone();
            let mut keep = None;
            board.add_pci(|slot, intx, msi| {
                let source: Box<dyn AudioSource> = if input { make_source() } else { Box::new(Silence) };
                let d = Arc::new(Mutex::new(Hda::new(slot, mem, intx, msi, ids, input, make_sink(), source)));
                keep = Some(d.clone());
                d
            })?;
            if let Some(d) = keep {
                vroom_devices::hda::spawn_timer(&d)?;
            }
        }
        SoundDevice::Virtio | SoundDevice::Auto => {
            let mut s = Sound::new(Box::new(make_sink));
            if input {
                s = s.with_capture(Box::new(make_source));
            }
            board.add_virtio(Box::new(s))?;
        }
    }
    Ok(helper)
}
fn read_file(p: &std::path::Path, what: &str, max: u64) -> Result<Vec<u8>> {
    let meta = std::fs::metadata(p).map_err(|e| Error::Setup(format!("{what} {}: {e}", p.display())))?;
    if meta.len() > max {
        return Err(Error::Setup(format!("{what} {} too large", p.display())));
    }
    std::fs::read(p).map_err(|e| Error::Setup(format!("{what} {}: {e}", p.display())))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build(
    config: &VmConfig,
    hv: &dyn Hypervisor,
    serial_out: Box<dyn Write + Send>,
    clipboard: Option<Box<dyn ClipboardProvider>>,
    ui: Option<DisplayServer>,
    disks: &DiskSet,
    events: Sender<VmEvent>,
    env: &BuildEnv,
) -> Result<Machine> {
    if hv.arch() != vroom_hv::Arch::Aarch64 {
        return Err(Error::Setup("only the aarch64 board exists so far".into()));
    }
    // Names and numbers the guest can read about its hardware.
    let ident = &config.identity;
    let info: IrqChipInfo = hv.irqchip_info()?;
    let max = layout::max_vcpus(info.redist_size_per_cpu);
    if config.cpus > max {
        return Err(Error::Setup(format!("at most {max} vCPUs on this board")));
    }
    let ram_size = config.memory_mib << 20;
    if ram_size > layout::RAM_MAX {
        return Err(Error::Setup("too much RAM for this board".into()));
    }

    let msi = GicMsiConfig {
        base: layout::GIC_MSI_BASE,
        intid_base: layout::MSI_INTID_BASE,
        count: layout::MSI_INTID_COUNT,
    };
    let vm = hv.create_vm(&VmParams {
        vcpus: config.cpus,
        irqchip: IrqChipConfig::GicV3 {
            dist_base: layout::GIC_DIST_BASE,
            redist_base: layout::GIC_REDIST_BASE,
            msi: Some(msi),
        },
        nested: config.nested,
        // Venus maps GPU buffers wherever the guest kernel puts them, in
        // its own page size: 4 KiB for stock kernels.
        small_pages: config.display.gpu == Gpu::Virtio3d,
    })?;

    // With 3D, RAM is shared memory: the GPU process maps it too (the
    // buffers the guest hands the GPU live there).
    let ram = [(GuestAddress(layout::RAM_BASE), ram_size)];
    let mem = if config.display.gpu == Gpu::Virtio3d { GuestMemory::new_shared(&ram)? } else { GuestMemory::new(&ram)? };
    vm.map_memory(&mem)?;

    let mut board = Board::new(vm.clone(), mem.clone(), config.use_pci());
    let uart = Arc::new(Mutex::new(Pl011::new(
        layout::UART_BASE,
        serial_out,
        SpiLine::new(vm.clone(), layout::UART_SPI),
    )));
    board.reg.add(layout::UART_BASE, layout::MMIO_DEV_SIZE, uart.clone())?;
    let rtc = Arc::new(Mutex::new(Pl031::new(layout::RTC_BASE, SpiLine::new(vm.clone(), layout::RTC_SPI))));
    board.reg.add(layout::RTC_BASE, layout::MMIO_DEV_SIZE, rtc)?;

    if !info.msi_frame_in_kernel {
        // Backend has no MSI frame (KVM): emulate the GICv2m doorbell.
        let frame = crate::irq::V2mFrame::new(vm.clone());
        board.reg.add_bus_only(layout::GIC_MSI_BASE, info.msi_size, frame)?;
    }
    board.add_virtio(Box::new(Rng::new()))?;
    let audio = match config.sound_device() {
        Some(dev) => Some(sound_card(&mut board, config, dev, ui.as_ref())?),
        None => None,
    };
    for s in &config.shares {
        board.add_virtio(Box::new(Fs::new(&s.tag, s.path.clone(), s.readonly).map_err(|e| Error::Setup(e.to_string()))?))?;
    }
    let mut nvme_ns = Vec::new();
    for (d, vd) in config.disks.iter().zip(&disks.disks) {
        let i = vd.index;
        let img: Arc<dyn vroom_block::DiskImage> = vd.image.clone();
        match d.bus {
            DiskBus::Virtio => {
                let id = format!("vd{}", (b'a' + i as u8) as char);
                let serial = format!("{}{i:04}", ident.disk_serial);
                // A queue per vCPU: completions interrupt the vCPU that asked.
                let queues = config.cpus.min(vroom_virtio::devices::block::MAX_QUEUES as u32) as u16;
                board.add_virtio(Box::new(Block::new(&id, img).with_serial(&serial).with_queues(queues)))?;
            }
            DiskBus::Nvme => nvme_ns.push(Namespace {
                disk: img,
                read_only: d.readonly,
                block_size: if d.cdrom { 2048 } else { 512 },
            }),
        }
    }
    if !nvme_ns.is_empty() {
        let mem2 = mem.clone();
        let (model, prefix, fw) = (ident.disk_model.clone(), ident.disk_serial.clone(), ident.disk_firmware.clone());
        let ids = vendor_ids(&ident.nvme_pci_id)?;
        board.add_pci(move |slot, intx, msi| {
            let serial = format!("{prefix}{slot:04}");
            Nvme::new(slot, nvme_ns, mem2, intx, msi).with_ids(&model, &serial, &fw).with_pci_ids(ids).into_shared()
        })?;
    }

    // Display. UEFI always gets ramfb (firmware / installer screen);
    // virtio-gpu takes over once a guest driver drives it.
    // Several monitors: Linux gets one GPU with a scanout each; Windows
    // a GPU each (its drivers drive one scanout; the others 2D), added
    // last so the other devices keep their PCI slots.
    let uefi = matches!(config.boot, Boot::Uefi { .. });
    let monitors = if config.display.gpu == Gpu::None { 1 } else { config.display.monitors.clamp(1, vroom_config::MAX_MONITORS) as usize };
    let screens: Vec<Arc<FbSink>> = if config.display.gpu != Gpu::None || uefi {
        match &ui {
            Some(ds) => ds.sinks(monitors)?,
            None => FbSink::headless(monitors)?,
        }
    } else {
        Vec::new()
    };
    let screen = screens.first().cloned();
    let mut gpus = Vec::new();
    let mut heads: Vec<(GpuControl, usize)> = Vec::new();
    let mut later_gpus = Vec::new();
    if config.display.gpu != Gpu::None {
        let size = (config.display.width, config.display.height);
        let sinks: Vec<Arc<dyn DisplaySink>> = screens.iter().map(|s| s.clone() as Arc<dyn DisplaySink>).collect();
        let per_gpu = if config.guest_os() == GuestOs::Windows { 1 } else { sinks.len() };
        for (n, group) in sinks.chunks(per_gpu).enumerate() {
            let (gpu, ctl) = GpuDev::with_heads(&vec![size; group.len()], group.to_vec());
            let mut gpu = gpu.with_index(n as u16);
            // Windows' extra GPUs stay 2D: Direct3D can't share frames
            // between two 3D GPUs (cross-adapter surfaces), so Windows
            // draws on the first and copies to the others.
            if config.display.gpu == Gpu::Virtio3d && n == 0 {
                gpu = gpu.with_3d(three_d(&vm, config, &mem, &env.state_dir)?);
                if config.guest_os() == GuestOs::Windows && env.gpu3d_vram {
                    gpu = gpu.with_vram(Arc::new(VmShm { vm: vm.clone(), filler: Mutex::new(None) }), GPU3D_VRAM);
                }
            }
            if n > 0 {
                // Room for the framebuffer Windows' 2D driver clears.
                gpu = gpu.with_vram(Arc::new(VmShm { vm: vm.clone(), filler: Mutex::new(None) }), EXTRA_GPU_VRAM);
            }
            heads.extend((0..group.len()).map(|sid| (ctl.clone(), sid)));
            gpus.push(ctl);
            if n == 0 {
                board.add_virtio(Box::new(gpu))?;
            } else {
                later_gpus.push(gpu);
            }
        }
    }
    // Hardware video decoding: a V4L2 decoder whose work happens in the
    // video helper (VideoToolbox, sandboxed on its own).
    // Encoding: a second device, a V4L2 encoder, and a helper of its own.
    use vroom_virtio::devices::media::{Media, Role};
    for (on, role) in [(config.display.video_decode, Role::Decoder), (config.display.video_encode, Role::Encoder)] {
        if on {
            let (window, window_mem) = crate::video::Window::new(vm.clone())?;
            let (helper, codecs) = crate::video::Helper::spawn(&window_mem, role)?;
            board.add_virtio(Box::new(Media::new(helper, codecs, window_mem, window, role)))?;
        }
    }
    // Input: virtio-input for direct-boot Linux, USB HID otherwise (UEFI,
    // Windows setup: works with zero drivers). Never both: no doubled keys.
    // The xHCI also carries passthrough devices, each behind its own
    // helper process (started here, before the sandbox).
    let mut input = None;
    let mut xhci = None;
    let mut usb_ports = None;
    // The camera: a USB device on the xHCI, fed by its helper.
    let camera = if config.camera {
        let feed = Arc::new(vroom_devices::usb::uvc::CameraFeed::default());
        let test = std::env::var_os("VROOM_CAMERA_TEST").is_some_and(|v| v == "1");
        Some((crate::camera::CameraHelper::spawn(feed.clone(), test)?, feed))
    } else {
        None
    };
    if (screen.is_some() && board.use_pci) || !config.usb.is_empty() || camera.is_some() {
        let mut helpers = Vec::new();
        let mut links = Vec::new();
        for u in &config.usb {
            if u.mode != vroom_config::UsbMode::Device {
                // The app does those: it opens the disk or the volume.
                log::info!("usb {}: as {}, connected by the app when it shows this VM", u.id, u.mode.as_str());
                continue;
            }
            let (h, link) = crate::usb::spawn(&config.name, u)?;
            helpers.push(h);
            links.push(link);
        }
        let mem2 = mem.clone();
        let pci_ids = vendor_ids(&ident.usb_pci_id)?;
        board.add_pci(|slot, intx, msi| {
            let ids = UsbIds { vendor_id: ident.usb_vendor_id, manufacturer: ident.usb_manufacturer.clone() };
            let mut x = Xhci::new(slot, mem2, intx, msi).with_ids(&ids).with_pci_ids(pci_ids).with_passthrough(links);
            if let Some((_, feed)) = &camera {
                x = x.with_camera(vroom_devices::usb::uvc::Camera::new(&ids, feed.clone()));
            }
            let x = Arc::new(Mutex::new(x));
            xhci = Some(x.clone());
            x
        })?;
        if let Some(x) = &xhci {
            let viewer = ui.as_ref().map(|d| d.tx());
            usb_ports = Some(crate::usb::UsbPorts::new(x.clone(), helpers, viewer)?);
            if let Some((c, _)) = &camera {
                c.run(x.clone())?;
            }
        }
    }
    if screen.is_some() {
        if board.use_pci {
            input = Some(InputPipes { kbd: None, tablet: None, usb: xhci });
        } else {
            let (kbd_host, kbd_dev) = UnixDatagram::pair()?;
            let (tab_host, tab_dev) = UnixDatagram::pair()?;
            let (m, v) = (&ident.usb_manufacturer, ident.usb_vendor_id);
            board.add_virtio(Box::new(Input::new(InputKind::Keyboard, kbd_dev).with_ids(&format!("{m} Keyboard"), v)))?;
            board.add_virtio(Box::new(Input::new(InputKind::Tablet, tab_dev).with_ids(&format!("{m} Tablet"), v)))?;
            input = Some(InputPipes { kbd: Some(kbd_host), tablet: Some(tab_host), usb: None });
        }
    }

    // Guest agent channel: /dev/virtio-ports/<agent_port> in the guest.
    let (host_end, dev_end) = UnixStream::pair()?;
    board.add_virtio(Box::new(Console::new(vec![(ident.agent_port.clone(), dev_end)])?))?;
    let clipboard = match &ui {
        Some(ds) => Some(ds.clipboard()),
        None => clipboard,
    };
    let max_pixels = if config.guest_os() == GuestOs::Windows { agent::WINDOWS_MAX_PIXELS } else { 0 };
    let viewer = ui.as_ref().map(|d| d.tx());
    let screen_ctl = agent::Screen { heads, max_pixels, gpu3d: config.display.gpu == Gpu::Virtio3d, viewer: viewer.clone() };
    let agent = agent::spawn(host_end, config.clipboard.clone(), clipboard, screen_ctl)?;
    // Network adapters (right after the console: the device order is the
    // guest's layout). The viewer can change them while the VM runs.
    let planned = crate::net::plan(config, env.saved.as_ref())?;
    let reach = crate::net::Reach::of(planned.iter().map(|p| &p.net), env.sandbox);
    let mut nics = Vec::new();
    let mut nets = Vec::new();
    for (i, p) in planned.into_iter().enumerate() {
        let (backend, mac) = crate::net::boot_backend(&config.name, ident, i, &p.net, p.mac, reach.dns)?;
        let live = crate::net::live_of(&p.net)?;
        let mut dev = Net::with_link(&format!("eth{i}"), mac, backend, &crate::net::label(&live), p.net.connected && !p.removed);
        if !env.net_ctrl {
            dev = dev.without_ctrl();
        }
        let port = env.hot_nets.get(&i).copied();
        nics.push(crate::net::Nic::new(dev.control(), live, p.net.clone(), port));
        match port {
            Some(port) if board.use_pci => board.hot.push((port, Box::new(dev))),
            Some(_) => return Err(Error::Setup("the snapshot has an adapter in a hot plug slot, and this VM isn't on PCI".into())),
            None => board.add_virtio(Box::new(dev))?,
        }
        nets.push(p.net);
    }
    // Removable shares for USB disks' files (no hotplug: there from the
    // start; a resumed VM has as many as it was saved with).
    let mut usb_files = Vec::new();
    if let Some(u) = &usb_ports {
        let mut slots = Vec::new();
        for i in 0..env.usb_file_slots {
            let tag = format!("{}{i}", vroom_config::USB_SHARE_PREFIX);
            let (fs, slot) = Fs::removable(&tag).map_err(|e| Error::Setup(e.to_string()))?;
            usb_files.push(fs);
            slots.push((tag, slot));
        }
        u.set_files(slots, agent.clone());
        crate::usb::test_seam(u);
    }
    let net_ports = crate::net::NetPorts::new(nics, viewer.clone(), reach, ident.clone(), env.net_ctrl);
    if let Some(ds) = ui {
        if screen.is_none() {
            return Err(Error::Setup("a viewer needs a display (display.gpu or UEFI boot)".into()));
        }
        // The microphone and camera gates close with the window.
        let cam = camera.as_ref().map(|(c, _)| c.clone());
        if let Some(c) = &cam {
            c.set_viewer(ds.tx());
            c.set_window(true);
        }
        let window = (audio.is_some() || cam.is_some()).then(|| {
            Box::new(move |open| {
                if let Some(a) = &audio {
                    a.set_window(open);
                }
                if let Some(c) = &cam {
                    c.set_window(open);
                }
            }) as Box<dyn Fn(bool) + Send + Sync>
        });
        // A viewer can come back (the app crashed, then started again).
        let listener = match viewer_socket(&env.state_dir) {
            Ok(l) => Some(l),
            Err(e) => {
                log::warn!("display: no viewer socket in the state dir ({e}): a window lost can't be reopened");
                None
            }
        };
        let windows = config.guest_os() == GuestOs::Windows;
        ds.run(input, agent.clone(), events.clone(), window, listener, screens.clone(), windows, usb_ports, Some(net_ports.clone()))?;
    }
    for gpu in later_gpus {
        board.add_virtio(Box::new(gpu))?;
    }
    // Last, so the devices before keep their places in VMs made before.
    for fs in usb_files {
        board.add_virtio(Box::new(fs))?;
    }
    let root = board.finish_pci(vendor_ids(&ident.pci_bridge_id)?, vendor_ids(&ident.pcie_port_id)?)?;
    let need_pci = root.is_some();
    let hotplug = root
        .filter(|r| r.ports() > 0)
        .map(|r| Hotplug::new(r, mem.clone(), board.msi.clone(), board.intx.clone()));
    if let Some(h) = &hotplug {
        net_ports.set_hotplug(h.clone());
    }
    let Board { mut reg, fdt: mut fdt_devices, .. } = board;
    if config.tpm {
        let backend = crate::tpm::Swtpm::start(&env.state_dir.join("tpm"), !env.resuming)?;
        let dev = vroom_devices::tpm::Tpm::new(layout::TPM_BASE, Box::new(backend));
        reg.add(layout::TPM_BASE, layout::TPM_SIZE, Arc::new(Mutex::new(dev)))?;
        fdt_devices.push(board::FdtDevice::Tpm { base: layout::TPM_BASE, size: layout::TPM_SIZE });
    }

    let mpidrs: Vec<u64> = (0..config.cpus).map(mpidr_for).collect();
    let fdt_base = |devices: Vec<board::FdtDevice>, cmdline: String| board::FdtConfig {
        cpu_mpidrs: mpidrs.clone(),
        ram: vec![(layout::RAM_BASE, ram_size)],
        cmdline,
        initrd: None,
        gic_dist_size: info.dist_size,
        gic_redist_size: info.redist_size_per_cpu,
        gic_msi: Some((layout::GIC_MSI_BASE, info.msi_size)),
        devices,
        psci_smc: config.nested,
        model: ident.dt_model.clone(),
    };
    let boot = match &config.boot {
        Boot::Kernel { kernel, initrd, cmdline } => {
            let kernel = read_file(kernel, "kernel", 1 << 30)?;
            let initrd = match initrd {
                Some(p) => Some(read_file(p, "initrd", ram_size / 2)?),
                None => None,
            };
            BootImage::Kernel {
                kernel,
                initrd,
                ram: (layout::RAM_BASE, ram_size),
                fdt_cfg: fdt_base(fdt_devices, cmdline.clone()),
            }
        }
        Boot::Uefi { firmware, vars } => {
            let fw = crate::firmware::Firmware { code: firmware.clone(), vars: vars.clone() };
            fw.setup(
                &vm,
                &mem,
                &mut reg,
                config.cpus,
                &mpidrs,
                &info,
                need_pci,
                config.nested,
                fdt_devices
                    .iter()
                    .filter_map(|d| match *d {
                        board::FdtDevice::VirtioMmio { base, size, spi } => Some((base, size, spi)),
                        _ => None,
                    })
                    .collect(),
                screen.as_ref().map(Arc::downgrade),
                config.tpm.then_some(layout::TPM_BASE),
                &vroom_arch::smbios::SmbiosConfig {
                    sku: ident.sku_or(&config.name).to_string(),
                    vendor: ident.vendor.clone(),
                    product: ident.product.clone(),
                    family: ident.family.clone(),
                    version: ident.version.clone(),
                    bios_date: ident.bios_date.clone(),
                    serial: ident.serial.clone(),
                    chassis_type: ident.chassis_type,
                    cpu_name: ident.cpu_name.clone(),
                    announce_vm: ident.announce_vm,
                    uuid: env.uuid,
                    cpus: config.cpus,
                    memory_bytes: ram_size,
                    ram_base: layout::RAM_BASE,
                },
                &vroom_arch::acpi::AcpiIds::new(
                    &ident.acpi_oem_id,
                    &ident.acpi_table_id,
                    &ident.acpi_creator_id,
                    Some(ident.hypervisor_id.as_str()).filter(|h| ident.announce_vm && !h.is_empty()),
                ),
            )?;
            fdt_devices.push(board::FdtDevice::Flash {
                base0: layout::FLASH0_BASE,
                base1: layout::FLASH1_BASE,
                size: layout::FLASH_SIZE,
            });
            fdt_devices.push(board::FdtDevice::FwCfg { base: layout::FW_CFG_BASE });
            BootImage::Firmware { fdt_cfg: fdt_base(fdt_devices, String::new()) }
        }
    };

    let shared = Arc::new(VcpuShared {
        vm: vm.clone(),
        mmio: reg.bus.build(),
        cpus: Mutex::new(CpuTable::new(mpidrs)),
        events,
        peers: OnceLock::new(),
        device_exits: std::sync::atomic::AtomicU64::new(0),
    });
    let mut vcpus = Vec::new();
    for i in 0..config.cpus {
        vcpus.push(vcpu::spawn(i, shared.clone())?);
    }
    let _ = shared.peers.set(vcpus.iter().map(|v| v.peer.clone()).collect());
    vm.finalize()?;

    Ok(Machine { vm, mem, shared, vcpus, devices: reg.stateful, uart, agent, gpus, viewer, screens, boot, nets, net_ports, hotplug })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macs_and_pci_ids_from_identity() {
        let mut ident = vroom_config::Identity::default();
        // Same MAC as before mac_prefix existed: 02 then five hash bytes.
        let m = ident.nic_mac("dev", 0);
        assert_eq!(m[0], 0x02);
        assert_ne!(ident.nic_mac("dev", 1), m);
        ident.mac_prefix = "00:1b:21".into();
        let v = ident.nic_mac("dev", 0);
        assert_eq!((&v[..3], &v[3..]), (&[0x00, 0x1b, 0x21][..], &m[1..4]));
        let ids = vendor_ids(&ident.nvme_pci_id).unwrap();
        assert_eq!(ids, VendorIds { vendor: 0x1b36, device: 0x0010, subsys_vendor: 0x1b36, subsys: 0 });
        let ids = vendor_ids("144d:a808:144d:a801").unwrap();
        assert_eq!((ids.device, ids.subsys), (0xa808, 0xa801));
        assert!(vendor_ids("144d").is_err());
    }
}

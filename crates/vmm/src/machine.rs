//! Assemble a machine from a config: memory, irqchip, devices, vCPUs.

use std::io::Write;
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};

use vroom_arch::aarch64::{self as board, layout};
use vroom_config::{Boot, DiskBus, Gpu, Identity, VmConfig};
use vroom_devices::{pl011::Pl011, pl031::Pl031, BusBuilder, BusDevice};
use vroom_hv::{
    aarch64::mpidr_for, BootRegs, GicMsiConfig, Hypervisor, IrqChipConfig, IrqChipInfo, Vm, VmParams,
};
use vroom_memory::{GuestAddress, GuestMemory};
use vroom_snapshot::Snapshottable;
use vroom_config::NetMode;
use vroom_net::nat::{Nat, NatConfig, NatPolicy, TcpForward};
use vroom_net::socket::FrameSocket;
use vroom_net::{netd, NetBackend};
use vroom_virtio::devices::gpu::{DisplaySink, Gpu as GpuDev, ThreeD};
use vroom_virtio::devices::gpu3d::{Mapping, ShmMapper};
use vroom_virtio::devices::input::{Input, InputKind};
use vroom_virtio::devices::{block::Block, console::Console, fs::Fs, net::Net, rng::Rng};
use vroom_devices::nvme::{Namespace, Nvme};
use vroom_devices::usb::hid::UsbIds;
use vroom_devices::usb::xhci::Xhci;
use vroom_devices::pci::{Intx, IntxRouter, MsiSender, PciRoot, SharedPciDevice, VendorIds};
use vroom_devices::IrqLine;
use vroom_virtio::mmio::{ShmRegion, VirtioMmio};
use vroom_virtio::{pci::VirtioPci, VirtioDevice};

use crate::agent::{self, Agent, ClipboardProvider};
use crate::disks::DiskSet;
use crate::display::{DisplayServer, FbSink, InputPipes, ViewerTx};
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

    /// Put the root complex on the bus if anything needs it.
    fn finish_pci(&mut self, host_ids: VendorIds) -> Result<bool> {
        if self.pci.is_empty() && !self.use_pci {
            return Ok(false);
        }
        let root = PciRoot::with_host_ids(std::mem::take(&mut self.pci), host_ids);
        self.reg.add(layout::PCI_ECAM_BASE, layout::PCI_ECAM_SIZE, Arc::new(Mutex::new(root.ecam_port())))?;
        self.reg.add_bus_only(layout::PCI_MMIO_BASE, layout::PCI_MMIO_SIZE, root.window_port(layout::PCI_MMIO_BASE))?;
        self.reg.add_bus_only(layout::PCI_MMIO64_BASE, layout::PCI_MMIO64_SIZE, root.window_port(layout::PCI_MMIO64_BASE))?;
        self.fdt.push(board::FdtDevice::PciEcam {
            ecam_base: layout::PCI_ECAM_BASE,
            ecam_size: layout::PCI_ECAM_SIZE,
            buses: (layout::PCI_ECAM_SIZE >> 20) as u32,
        });
        Ok(true)
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
    pub viewer: Option<ViewerTx>,
    pub screen: Option<Arc<FbSink>>,
    pub boot: BootImage,
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

    /// On the bus, but its state lives elsewhere (PCI windows).
    pub fn add_bus_only<T: BusDevice + 'static>(&mut self, base: u64, len: u64, dev: T) -> Result<()> {
        self.bus.insert(base, len, Arc::new(Mutex::new(dev))).map_err(Error::Setup)
    }
}

/// Host-side facts the board needs.
pub(crate) struct BuildEnv {
    /// SMBIOS system UUID.
    pub uuid: [u8; 16],
    pub state_dir: std::path::PathBuf,
    /// Restoring a snapshot / suspend image rather than cold booting.
    pub resuming: bool,
}

/// What gets loaded into RAM at (re)boot. Kept so reset can reload it.
pub(crate) enum BootImage {
    /// Direct Linux boot.
    Kernel { kernel: Vec<u8>, initrd: Option<Vec<u8>>, ram: (u64, u64), fdt_cfg: board::FdtConfig },
    /// UEFI in flash; the FDT goes at the start of RAM for it to find.
    Firmware { fdt_cfg: board::FdtConfig },
}

impl BootImage {
    /// Put boot blobs into RAM. Returns boot CPU regs.
    pub fn load(&self, mem: &GuestMemory) -> Result<BootRegs> {
        match self {
            BootImage::Kernel { kernel, initrd, ram, fdt_cfg } => {
                let k = board::load_kernel(mem, ram.0, kernel)?;
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

/// Host-visible window size for GPU buffers (address space, not memory).
const GPU_SHM_SIZE: u64 = 8 << 30;

/// Venus through virglrenderer, or a clear error.
fn three_d(vm: &Arc<dyn Vm>) -> Result<ThreeD> {
    if !vroom_gpu3d::load() {
        return Err(Error::Setup(
            "gpu = \"virtio3d\" needs virglrenderer with Venus (macOS: brew install slp/krunkit/virglrenderer)".into(),
        ));
    }
    let r = vroom_gpu3d::Virgl::new().ok_or_else(|| Error::Setup("virglrenderer: Venus init failed (no Vulkan driver?)".into()))?;
    Ok(ThreeD::new(Box::new(r), Arc::new(VmShm(vm.clone())), GPU_SHM_SIZE))
}

/// Maps GPU buffers into the guest with the hypervisor.
struct VmShm(Arc<dyn Vm>);

impl ShmMapper for VmShm {
    fn map(&self, gpa: u64, m: &Mapping) -> bool {
        // SAFETY: the renderer keeps the buffer mapped until unmap_blob,
        // which the GPU device only calls after `unmap` below.
        match unsafe { self.0.map_host(gpa, m.ptr, m.size) } {
            Ok(()) => true,
            Err(e) => {
                log::warn!("gpu: map {gpa:#x}: {e}");
                false
            }
        }
    }

    fn unmap(&self, gpa: u64, size: u64) {
        if let Err(e) = self.0.unmap_host(gpa, size) {
            log::warn!("gpu: unmap {gpa:#x}: {e}");
        }
    }
}

/// Deterministic MAC per (VM, NIC index), so the guest keeps the same DHCP
/// identity across boots without storing state. Starts with
/// `identity.mac_prefix` (default 02: locally administered).
fn stable_mac(ident: &Identity, name: &str, idx: usize) -> [u8; 6] {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes().chain([0, idx as u8]) {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    let b = h.to_le_bytes();
    ident.mac([b[0], b[1], b[2], b[3], b[4], b[5]])
}

/// A `*_pci_id` from `[identity]` (the config checked it already).
fn vendor_ids(s: &str) -> Result<VendorIds> {
    let [vendor, device, subsys_vendor, subsys] =
        vroom_config::pci_ids(s).ok_or_else(|| Error::Setup(format!("bad PCI id {s:?}")))?;
    Ok(VendorIds { vendor, device, subsys_vendor, subsys })
}

/// Backend for one NIC, and the MAC the guest must use.
fn net_backend(
    vm: &str,
    ident: &Identity,
    idx: usize,
    n: &vroom_config::Net,
    mac: [u8; 6],
) -> Result<(Box<dyn NetBackend>, [u8; 6])> {
    let via_netd = |attach: netd::Attach| -> Result<(Box<dyn NetBackend>, [u8; 6])> {
        let sock = std::env::var_os("VROOM_NETD_SOCKET")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| netd::DEFAULT_SOCKET.into());
        let req = netd::Request { version: netd::VERSION, vm: vm.to_string(), nic: idx as u32, attach, mac };
        let (s, assigned) = netd::attach(&sock, &req).map_err(|e| Error::Setup(e.to_string()))?;
        let fs = FrameSocket::new(s).map_err(|e| Error::Setup(format!("netd socket: {e}")))?;
        Ok((Box::new(fs), assigned))
    };
    match n.mode()? {
        NetMode::Nat { port_forwards } => {
            let mut cfg = NatConfig::new(mac);
            // Adapter N: the first adapter's /24 plus N (10.0.(2+N).0/24 by
            // default): two NICs never share a subnet in the guest's
            // routing table.
            (cfg.guest, cfg.gateway, cfg.dns) = ident.nat_addrs(idx);
            cfg.gateway_mac = vroom_config::parse_mac(&ident.nat_gateway_mac)?;
            cfg.policy = NatPolicy { allow_host_loopback: n.allow_host_loopback, block_private: n.block_private };
            for p in port_forwards {
                let ip: std::net::IpAddr =
                    p.host_addr.parse().map_err(|_| Error::Setup(format!("bad host_addr {}", p.host_addr)))?;
                cfg.forwards.push(TcpForward { host: (ip, p.host_port).into(), guest_port: p.guest_port });
            }
            let nat = Nat::new(cfg).map_err(|e| Error::Setup(format!("nat: {e}")))?;
            Ok((Box::new(nat), mac))
        }
        NetMode::Bridged { interface } => via_netd(netd::Attach::Bridged { interface }),
        NetMode::HostOnly { network } => via_netd(netd::Attach::HostOnly { network }),
    }
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
    })?;

    let mem = GuestMemory::new(&[(GuestAddress(layout::RAM_BASE), ram_size)])?;
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
    if config.audio.output {
        // VROOM_AUDIO_WAV_DIR: test mode, record instead of playing.
        let wav = std::env::var_os("VROOM_AUDIO_WAV_DIR").map(std::path::PathBuf::from);
        let helper = crate::audio::AudioHelper::spawn(wav.as_deref())?;
        let make = move || -> Box<dyn vroom_virtio::devices::sound::AudioSink> { Box::new(crate::audio::HelperSink(helper.clone())) };
        board.add_virtio(Box::new(vroom_virtio::devices::sound::Sound::new(Box::new(make))))?;
    }
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
                board.add_virtio(Box::new(Block::new(&id, img).with_serial(&serial)))?;
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
            Arc::new(Mutex::new(Nvme::new(slot, nvme_ns, mem2, intx, msi).with_ids(&model, &serial, &fw).with_pci_ids(ids)))
        })?;
    }

    // Display. UEFI always gets ramfb (firmware / installer screen);
    // virtio-gpu takes over once a guest driver drives it.
    let uefi = matches!(config.boot, Boot::Uefi { .. });
    let screen = if config.display.gpu != Gpu::None || uefi {
        Some(match &ui {
            Some(ds) => ds.sink()?,
            None => FbSink::headless()?,
        })
    } else {
        None
    };
    let mut gpu_ctl = None;
    if let (Some(fb), true) = (&screen, config.display.gpu != Gpu::None) {
        let sink: Arc<dyn DisplaySink> = fb.clone();
        let (mut gpu, ctl) = GpuDev::new(config.display.width, config.display.height, sink);
        if config.display.gpu == Gpu::Virtio3d {
            gpu = gpu.with_3d(three_d(&vm)?);
        }
        board.add_virtio(Box::new(gpu))?;
        gpu_ctl = Some(ctl);
    }
    // Input: virtio-input for direct-boot Linux, USB HID otherwise (UEFI,
    // Windows setup: works with zero drivers). Never both: no doubled keys.
    let mut input = None;
    if screen.is_some() {
        if board.use_pci {
            let mem2 = mem.clone();
            let mut keep = None;
            let pci_ids = vendor_ids(&ident.usb_pci_id)?;
            board.add_pci(|slot, intx, msi| {
                let ids = UsbIds { vendor_id: ident.usb_vendor_id, manufacturer: ident.usb_manufacturer.clone() };
                let x = Arc::new(Mutex::new(Xhci::new(slot, mem2, intx, msi).with_ids(&ids).with_pci_ids(pci_ids)));
                keep = Some(x.clone());
                x
            })?;
            input = Some(InputPipes { kbd: None, tablet: None, usb: keep });
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
    let agent = agent::spawn(host_end, config.clipboard.clone(), clipboard)?;
    let viewer = ui.as_ref().map(|d| d.tx());
    if let Some(ds) = ui {
        if screen.is_none() {
            return Err(Error::Setup("a viewer needs a display (display.gpu or UEFI boot)".into()));
        }
        ds.run(gpu_ctl, input, agent.clone(), events.clone())?;
    }
    for (i, n) in config.nets.iter().enumerate() {
        let mac = match &n.mac {
            Some(m) => vroom_config::parse_mac(m)?,
            None => stable_mac(ident, &config.name, i),
        };
        let (backend, mac) = net_backend(&config.name, ident, i, n, mac)?;
        board.add_virtio(Box::new(Net::new(&format!("eth{i}"), mac, backend)))?;
    }
    let need_pci = board.finish_pci(vendor_ids(&ident.pci_bridge_id)?)?;
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
            let raw = read_file(kernel, "kernel", 1 << 30)?;
            let kernel = vroom_arch::decompress_if_needed(raw)?;
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
    });
    let mut vcpus = Vec::new();
    for i in 0..config.cpus {
        vcpus.push(vcpu::spawn(i, shared.clone())?);
    }
    let _ = shared.peers.set(vcpus.iter().map(|v| v.peer.clone()).collect());
    vm.finalize()?;

    Ok(Machine { vm, mem, shared, vcpus, devices: reg.stateful, uart, agent, viewer, screen, boot })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macs_and_pci_ids_from_identity() {
        let mut ident = Identity::default();
        // Same MAC as before mac_prefix existed: 02 then five hash bytes.
        let m = stable_mac(&ident, "dev", 0);
        assert_eq!(m[0], 0x02);
        assert_ne!(stable_mac(&ident, "dev", 1), m);
        ident.mac_prefix = "00:1b:21".into();
        let v = stable_mac(&ident, "dev", 0);
        assert_eq!((&v[..3], &v[3..]), (&[0x00, 0x1b, 0x21][..], &m[1..4]));
        let ids = vendor_ids(&ident.nvme_pci_id).unwrap();
        assert_eq!(ids, VendorIds { vendor: 0x1b36, device: 0x0010, subsys_vendor: 0x1b36, subsys: 0 });
        let ids = vendor_ids("144d:a808:144d:a801").unwrap();
        assert_eq!((ids.device, ids.subsys), (0xa808, 0xa801));
        assert!(vendor_ids("144d").is_err());
    }
}

//! Assemble a machine from a config: memory, irqchip, devices, vCPUs.

use std::io::Write;
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};

use bulwark_arch::aarch64::{self as board, layout};
use bulwark_config::{Boot, DiskBus, DiskFormat, Gpu, VmConfig};
use bulwark_devices::{pl011::Pl011, pl031::Pl031, BusBuilder, BusDevice};
use bulwark_hv::{
    aarch64::mpidr_for, BootRegs, GicMsiConfig, Hypervisor, IrqChipConfig, IrqChipInfo, Vm, VmParams,
};
use bulwark_memory::{GuestAddress, GuestMemory};
use bulwark_snapshot::Snapshottable;
use bulwark_config::NetMode;
use bulwark_net::nat::{Nat, NatConfig, NatPolicy, TcpForward};
use bulwark_net::NetBackend;
use bulwark_proto::AGENT_PORT;
use bulwark_virtio::devices::gpu::{DisplaySink, Gpu as GpuDev};
use bulwark_virtio::devices::input::{Input, InputKind};
use bulwark_virtio::devices::{block::Block, console::Console, net::Net, rng::Rng};
use bulwark_devices::nvme::{Namespace, Nvme};
use bulwark_devices::pci::{IntxRouter, PciRoot, SharedPciDevice};
use bulwark_devices::IrqLine;
use bulwark_virtio::{mmio::VirtioMmio, pci::VirtioPci, VirtioDevice};

use crate::agent::{self, Agent, ClipboardProvider};
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

/// Deterministic, locally administered MAC per (VM, NIC index), so the guest
/// keeps the same DHCP identity across boots without storing state.
fn stable_mac(name: &str, idx: usize) -> [u8; 6] {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes().chain([0, idx as u8]) {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    let b = h.to_le_bytes();
    [0x02, b[0], b[1], b[2], b[3], b[4]]
}

fn net_backend(n: &bulwark_config::Net, mac: [u8; 6]) -> Result<Box<dyn NetBackend>> {
    match n.mode()? {
        NetMode::Nat { port_forwards } => {
            let mut cfg = NatConfig::new(mac);
            cfg.policy = NatPolicy { allow_host_loopback: n.allow_host_loopback, block_private: n.block_private };
            for p in port_forwards {
                let ip: std::net::IpAddr =
                    p.host_addr.parse().map_err(|_| Error::Setup(format!("bad host_addr {}", p.host_addr)))?;
                cfg.forwards.push(TcpForward { host: (ip, p.host_port).into(), guest_port: p.guest_port });
            }
            let nat = Nat::new(cfg).map_err(|e| Error::Setup(format!("nat: {e}")))?;
            Ok(Box::new(nat))
        }
        NetMode::Bridged { .. } | NetMode::HostOnly { .. } => Err(Error::Setup(
            "bridged / host-only need bulwark-netd (not wired up yet)".into(),
        )),
    }
}

fn read_file(p: &std::path::Path, what: &str, max: u64) -> Result<Vec<u8>> {
    let meta = std::fs::metadata(p).map_err(|e| Error::Setup(format!("{what} {}: {e}", p.display())))?;
    if meta.len() > max {
        return Err(Error::Setup(format!("{what} {} too large", p.display())));
    }
    std::fs::read(p).map_err(|e| Error::Setup(format!("{what} {}: {e}", p.display())))
}

pub(crate) fn build(
    config: &VmConfig,
    hv: &dyn Hypervisor,
    serial_out: Box<dyn Write + Send>,
    clipboard: Option<Box<dyn ClipboardProvider>>,
    ui: Option<DisplayServer>,
    events: Sender<VmEvent>,
) -> Result<Machine> {
    if hv.arch() != bulwark_hv::Arch::Aarch64 {
        return Err(Error::Setup("only the aarch64 board exists so far".into()));
    }
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
    })?;

    let mem = GuestMemory::new(&[(GuestAddress(layout::RAM_BASE), ram_size)])?;
    vm.map_memory(&mem)?;

    let mut reg = DeviceRegistry { bus: BusBuilder::new(), stateful: Vec::new() };
    let uart = Arc::new(Mutex::new(Pl011::new(
        layout::UART_BASE,
        serial_out,
        SpiLine::new(vm.clone(), layout::UART_SPI),
    )));
    reg.add(layout::UART_BASE, layout::MMIO_DEV_SIZE, uart.clone())?;
    let rtc = Arc::new(Mutex::new(Pl031::new(layout::RTC_BASE, SpiLine::new(vm.clone(), layout::RTC_SPI))));
    reg.add(layout::RTC_BASE, layout::MMIO_DEV_SIZE, rtc)?;

    let mut fdt_devices = Vec::new();
    let mut slots = VirtioSlots::default();
    let use_pci = config.use_pci();
    // PCI functions collected here, attached to the root at the end.
    let mut pci_devices: Vec<(u8, SharedPciDevice)> = Vec::new();
    let intx = IntxRouter::new(
        (0..4).map(|i| SpiLine::new(vm.clone(), layout::PCI_INTX_SPI + i) as Arc<dyn IrqLine>).collect(),
    );
    let msi = GicMsi::new(vm.clone());
    let mut next_pci_slot = 1u8;
    let mut add_virtio = |dev: Box<dyn VirtioDevice>| -> Result<()> {
        if use_pci {
            let slot = next_pci_slot;
            if slot > 31 {
                return Err(Error::Setup("out of PCI slots".into()));
            }
            next_pci_slot += 1;
            let d = VirtioPci::new(slot, dev, mem.clone(), intx.pin(slot, 1), msi.clone());
            pci_devices.push((slot, Arc::new(Mutex::new(d))));
            return Ok(());
        }
        let (base, spi) = slots.next()?;
        let t = VirtioMmio::new(base, dev, mem.clone(), SpiLine::new(vm.clone(), spi));
        reg.add(base, layout::VIRTIO_MMIO_SIZE, Arc::new(Mutex::new(t)))?;
        fdt_devices.push(board::FdtDevice::VirtioMmio { base, size: layout::VIRTIO_MMIO_SIZE, spi });
        Ok(())
    };
    add_virtio(Box::new(Rng::new()))?;
    let mut nvme_ns = Vec::new();
    for (i, d) in config.disks.iter().enumerate() {
        let fmt = match d.format {
            DiskFormat::Auto => None,
            DiskFormat::Raw => Some(bulwark_block::Format::Raw),
            DiskFormat::Qcow2 => Some(bulwark_block::Format::Qcow2),
        };
        let img = bulwark_block::open(&d.path, fmt, d.readonly)
            .map_err(|e| Error::Setup(format!("disk {}: {e}", d.path.display())))?;
        match d.bus {
            DiskBus::Virtio => {
                let id = format!("vd{}", (b'a' + i as u8) as char);
                add_virtio(Box::new(Block::new(&id, img)))?;
            }
            DiskBus::Nvme => nvme_ns.push(Namespace { disk: img, read_only: d.readonly }),
        }
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
    let mut input = None;
    if let (Some(fb), true) = (&screen, config.display.gpu != Gpu::None) {
        let sink: Arc<dyn DisplaySink> = fb.clone();
        let (gpu, ctl) = GpuDev::new(config.display.width, config.display.height, sink);
        add_virtio(Box::new(gpu))?;
        let (kbd_host, kbd_dev) = UnixDatagram::pair()?;
        let (tab_host, tab_dev) = UnixDatagram::pair()?;
        add_virtio(Box::new(Input::new(InputKind::Keyboard, kbd_dev)))?;
        add_virtio(Box::new(Input::new(InputKind::Tablet, tab_dev)))?;
        gpu_ctl = Some(ctl);
        input = Some(InputPipes { kbd: Some(kbd_host), tablet: Some(tab_host) });
    }

    // Guest agent channel: /dev/virtio-ports/org.bulwark.agent.0 in the guest.
    let (host_end, dev_end) = UnixStream::pair()?;
    add_virtio(Box::new(Console::new(vec![(AGENT_PORT.to_string(), dev_end)])?))?;
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
            Some(m) => bulwark_config::parse_mac(m)?,
            None => stable_mac(&config.name, i),
        };
        let backend = net_backend(n, mac)?;
        add_virtio(Box::new(Net::new(&format!("eth{i}"), mac, backend)))?;
    }

    if !nvme_ns.is_empty() {
        let slot = next_pci_slot;
        next_pci_slot += 1;
        let n = Nvme::new(slot, nvme_ns, mem.clone(), intx.pin(slot, 1), msi.clone());
        pci_devices.push((slot, Arc::new(Mutex::new(n))));
    }
    let _ = next_pci_slot;
    let need_pci = !pci_devices.is_empty() || use_pci;
    if need_pci {
        let root = PciRoot::new(pci_devices);
        reg.add(layout::PCI_ECAM_BASE, layout::PCI_ECAM_SIZE, Arc::new(Mutex::new(root.ecam_port())))?;
        reg.add_bus_only(layout::PCI_MMIO_BASE, layout::PCI_MMIO_SIZE, root.window_port(layout::PCI_MMIO_BASE))?;
        reg.add_bus_only(layout::PCI_MMIO64_BASE, layout::PCI_MMIO64_SIZE, root.window_port(layout::PCI_MMIO64_BASE))?;
        fdt_devices.push(board::FdtDevice::PciEcam {
            ecam_base: layout::PCI_ECAM_BASE,
            ecam_size: layout::PCI_ECAM_SIZE,
            buses: (layout::PCI_ECAM_SIZE >> 20) as u32,
        });
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
    };
    let boot = match &config.boot {
        Boot::Kernel { kernel, initrd, cmdline } => {
            let raw = read_file(kernel, "kernel", 1 << 30)?;
            let kernel = bulwark_arch::decompress_if_needed(raw)?;
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
                fdt_devices
                    .iter()
                    .filter_map(|d| match *d {
                        board::FdtDevice::VirtioMmio { base, size, spi } => Some((base, size, spi)),
                        _ => None,
                    })
                    .collect(),
                screen.as_ref().map(Arc::downgrade),
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

    Ok(Machine { vm, mem, shared, vcpus, devices: reg.stateful, uart, agent, viewer, screen, boot })
}

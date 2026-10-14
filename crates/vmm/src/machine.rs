//! Assemble a machine from a config: memory, irqchip, devices, vCPUs.

use std::io::Write;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};

use bulwark_arch::aarch64::{self as board, layout};
use bulwark_config::{Boot, VmConfig};
use bulwark_devices::{pl011::Pl011, pl031::Pl031, BusBuilder, BusDevice};
use bulwark_hv::{
    aarch64::mpidr_for, BootRegs, GicMsiConfig, Hypervisor, IrqChipConfig, IrqChipInfo, Vm, VmParams,
};
use bulwark_memory::{GuestAddress, GuestMemory};
use bulwark_snapshot::Snapshottable;

use crate::irq::SpiLine;
use crate::psci::CpuTable;
use crate::vcpu::{self, VcpuHandle, VcpuShared, VmEvent};
use crate::{Error, Result, StatefulDevice};

pub(crate) struct Machine {
    pub vm: Arc<dyn Vm>,
    pub mem: GuestMemory,
    pub shared: Arc<VcpuShared>,
    pub vcpus: Vec<VcpuHandle>,
    pub devices: Vec<StatefulDevice>,
    pub uart: Arc<Mutex<Pl011>>,
    pub boot: BootImage,
}

/// Registers MMIO devices on the bus and in the snapshot list in one go,
/// so nothing can be on the bus but missing from snapshots.
struct DeviceRegistry {
    bus: BusBuilder,
    stateful: Vec<StatefulDevice>,
}

impl DeviceRegistry {
    fn add<T>(&mut self, base: u64, len: u64, dev: Arc<Mutex<T>>) -> Result<()>
    where
        T: BusDevice + Snapshottable + Send + 'static,
    {
        self.bus.insert(base, len, dev.clone()).map_err(Error::Setup)?;
        self.stateful.push(dev);
        Ok(())
    }
}

/// What gets loaded into RAM at (re)boot. Kept so reset can reload it.
pub(crate) struct BootImage {
    kernel: Vec<u8>,
    initrd: Option<Vec<u8>>,
    ram: (u64, u64),
    fdt_cfg: board::FdtConfig,
}

impl BootImage {
    /// Copy kernel, initrd and a fresh FDT into RAM. Returns boot CPU regs.
    pub fn load(&self, mem: &GuestMemory) -> Result<BootRegs> {
        let k = board::load_kernel(mem, self.ram.0, &self.kernel)?;
        let plan = board::place_boot_blobs(
            self.ram,
            k.end,
            self.initrd.as_ref().map(|i| i.len() as u64),
        )?;
        let mut fdt_cfg = self.fdt_cfg.clone();
        if let (Some(addr), Some(data)) = (plan.initrd_addr, &self.initrd) {
            board::load_initrd(mem, addr, data)?;
            fdt_cfg.initrd = Some((addr, data.len() as u64));
        }
        let fdt = board::create_fdt(&fdt_cfg)?;
        mem.write(GuestAddress(plan.fdt_addr), &fdt)?;
        Ok(BootRegs::Aarch64 { pc: k.entry, x: [plan.fdt_addr, 0, 0, 0] })
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

    let fdt_devices = Vec::new();

    let mpidrs: Vec<u64> = (0..config.cpus).map(mpidr_for).collect();
    let boot = match &config.boot {
        Boot::Kernel { kernel, initrd, cmdline } => {
            let raw = read_file(kernel, "kernel", 1 << 30)?;
            let kernel = bulwark_arch::decompress_if_needed(raw)?;
            let initrd = match initrd {
                Some(p) => Some(read_file(p, "initrd", ram_size / 2)?),
                None => None,
            };
            BootImage {
                kernel,
                initrd,
                ram: (layout::RAM_BASE, ram_size),
                fdt_cfg: board::FdtConfig {
                    cpu_mpidrs: mpidrs.clone(),
                    ram: vec![(layout::RAM_BASE, ram_size)],
                    cmdline: cmdline.clone(),
                    initrd: None,
                    gic_dist_size: info.dist_size,
                    gic_redist_size: info.redist_size_per_cpu,
                    gic_msi: Some((layout::GIC_MSI_BASE, info.msi_size)),
                    devices: fdt_devices,
                },
            }
        }
        Boot::Uefi { .. } => return Err(Error::Setup("UEFI boot not wired up yet".into())),
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

    Ok(Machine { vm, mem, shared, vcpus, devices: reg.stateful, uart, boot })
}

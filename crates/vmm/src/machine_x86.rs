//! Narrow direct-kernel PC profile. No firmware, PCI, disks or guest agent.

use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{mpsc::Sender, Arc, Mutex, OnceLock};

use vroom_arch::x86_64::{prepare_boot, BootConfig, PreparedBoot};
use vroom_config::{Boot, Display, Serial, Transport, VmConfig};
use vroom_devices::{uart16550::Uart16550, BusBuilder, IrqLine};
use vroom_hv::{Arch, Hypervisor, IrqChipConfig, Vm, VmParams};
use vroom_memory::{GuestAddress, GuestMemory};

use crate::agent::Agent;
use crate::lifecycle::LifecyclePolicy;
use crate::machine::{BootImage, Machine};
use crate::pio::{PcPioBus, PioError, PioRegion};
use crate::psci::CpuTable;
use crate::serial::SerialInput;
use crate::vcpu::{self, VcpuCmd, VcpuShared, VmEvent};
use crate::{Error, Result, VmmOptions};

// The same legacy PIC/PIT profile used by the proven native Linux harness.
pub(crate) const CMDLINE: &str = "console=ttyS0,115200 earlycon=uart8250,io,0x3f8,115200n8 rdinit=/init panic=-1 nokaslr acpi=off pci=off noapic";
const NO_AGENT: &str = "guest agent is unavailable on the x64 reference board";
const NO_INPUT: &str = "serial receive is unavailable on the x64 reference board";

fn unsupported(why: &str) -> Error {
    Error::Setup(format!("x64 reference profile: {why}"))
}

/// This entire phase runs before VMM state/device/helper side effects.
pub(crate) fn prepare(config: &VmConfig, hv: &dyn Hypervisor, opts: &VmmOptions) -> Result<PreparedBoot> {
    if matches!(config.boot, Boot::Uefi { .. }) {
        // Own and validate inputs, but do not enable a firmware board. In
        // particular, never pass these paths to the ARM VARS/disk setup.
        let _prepared = crate::firmware_x86::prepare(config, hv, opts)?;
        return Err(Error::Unavailable(crate::firmware_x86::EXECUTION_DISABLED));
    }
    if hv.arch() != Arch::X86_64 || hv.name() != "kvm" {
        return Err(unsupported("requires the x86_64 KVM backend"));
    }
    if cfg!(feature = "research") {
        return Err(unsupported("only the default VMM feature set is supported"));
    }
    if config.cpus != 1 || config.memory_mib != 512 {
        return Err(unsupported("requires exactly one vCPU and 512 MiB RAM"));
    }
    if config.nested || !config.disks.is_empty() || !config.nets.is_empty()
        || !config.shares.is_empty() || !config.usb.is_empty() || config.tpm
        || config.balloon || config.camera || config.audio != Default::default()
    {
        return Err(unsupported("nested virtualization, disks, networks, shares, USB, TPM, balloon, camera and audio are unavailable"));
    }
    if config.display != Display::default() || opts.ui.is_some() || opts.clipboard.is_some()
        || config.clipboard.host_to_guest || config.clipboard.guest_to_host
        || config.clipboard.max_bytes != vroom_config::Clipboard::default().max_bytes
    {
        return Err(unsupported("requires default headless display settings, no viewer and clipboard disabled"));
    }
    if config.virtio_transport != Transport::Auto || config.identity != Default::default()
        || config.serial != Serial::Stdio || !config.guest_os().is_linux()
    {
        return Err(unsupported("requires a Linux guest, default identity/transport and stdio serial output"));
    }
    let Boot::Kernel { kernel, initrd: Some(initrd), cmdline } = &config.boot else {
        return Err(unsupported("requires direct Linux kernel boot with an initramfs; firmware is unavailable"));
    };
    if cmdline != CMDLINE {
        return Err(unsupported("requires the documented reference kernel command line"));
    }
    let kernel = read_bounded(kernel, 128 << 20)?;
    let initrd = read_bounded(initrd, 8 << 20)?;
    Ok(prepare_boot(kernel, Some(initrd), cmdline, BootConfig::default())?)
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    // Opening a FIFO must not block before we can check its file type.
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW).open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > limit {
        return Err(unsupported("boot input must be a bounded, nonempty regular file"));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != metadata.len() || bytes.len() as u64 > limit {
        return Err(unsupported("boot input size changed while reading"));
    }
    Ok(bytes)
}

fn latch(error: &PioError, why: String) {
    let mut error = error.lock().unwrap_or_else(|e| e.into_inner());
    if error.is_none() { *error = Some(why); }
}

struct SerialIrq {
    vm: Arc<dyn Vm>,
    error: PioError,
}
impl IrqLine for SerialIrq {
    fn set_level(&self, level: bool) {
        if let Err(error) = self.vm.set_irq_line(4, level) {
            latch(&self.error, format!("COM1 IRQ4 failed: {error}"));
        }
    }
}

struct SerialOutput {
    output: Box<dyn Write + Send>,
    error: PioError,
}
impl Write for SerialOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let result = self.output.write(bytes);
        if let Err(error) = &result { latch(&self.error, format!("COM1 output failed: {error}")); }
        if matches!(result, Ok(0)) && !bytes.is_empty() {
            latch(&self.error, "COM1 output made no progress".into());
        }
        result
    }
    fn flush(&mut self) -> io::Result<()> {
        let result = self.output.flush();
        if let Err(error) = &result { latch(&self.error, format!("COM1 output flush failed: {error}")); }
        result
    }
}

pub(crate) fn build(config: &VmConfig, hv: &dyn Hypervisor, output: Box<dyn Write + Send>, events: Sender<VmEvent>, boot: PreparedBoot) -> Result<Machine> {
    let vm = hv.create_vm(&VmParams {
        vcpus: 1, irqchip: IrqChipConfig::Ioapic, nested: false,
        small_pages: false, helper_page: 0,
    })?;
    let mem = GuestMemory::new(&[(GuestAddress(0), 512 << 20)])?;
    vm.map_memory(&mem)?;
    let error: PioError = Default::default();
    let uart = Arc::new(Mutex::new(Uart16550::new(
        Box::new(SerialOutput { output, error: error.clone() }),
        Arc::new(SerialIrq { vm: vm.clone(), error: error.clone() }),
    )));
    // There is no host receive path or periodic RX timeout service. Output
    // and transmit interrupts are synchronous inside the device callback.
    let pio = PcPioBus::new(vec![PioRegion::new(0x3f8, 8, &[1], uart)?])?.with_error_latch(error);
    let shared = Arc::new(VcpuShared {
        vm: vm.clone(), mmio: BusBuilder::new().build(), pio: Some(pio),
        cpus: Mutex::new(CpuTable::new(vec![0])), events, peers: OnceLock::new(),
        device_exits: Default::default(), run_calls: Default::default(),
        halt_on_stop: Default::default(), halting: Default::default(),
        parked: Default::default(), halted_at: Default::default(),
    });
    let mut vcpu = vcpu::spawn(0, shared.clone(), LifecyclePolicy::ColdBootOnly)?;
    // A failed finalization must not leave the newly created worker behind.
    if let Err(error) = vm.finalize() {
        let _ = vcpu.peer.send(VcpuCmd::Stop);
        vcpu.join();
        return Err(error.into());
    }
    let _ = shared.peers.set(vec![vcpu.peer.clone()]);
    let net_ports = crate::net::NetPorts::new(Vec::new(), None, Default::default(), config.identity.clone(), false);
    Ok(Machine {
        vm, mem, shared, vcpus: vec![vcpu], devices: Vec::new(),
        serial_input: SerialInput::Unavailable(NO_INPUT), agent: Agent::unavailable(NO_AGENT),
        gpus: Vec::new(), viewer: None, display: None, screens: Vec::new(),
        boot: BootImage::X86Kernel(boot), nets: Vec::new(), net_ports, hotplug: None,
    })
}

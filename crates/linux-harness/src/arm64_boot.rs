//! Fixed, direct-kernel ARM64 profile. Fresh memory only; no firmware or saved state.
use std::{io::Write, sync::Arc};
use vroom_arch::aarch64::{self as board, layout};
use vroom_devices::{BusDevice, IrqLine, pl011::Pl011, pl031::Pl031};
use vroom_hv::BootRegs;
use vroom_memory::{GuestAddress, GuestMemory};

pub const RAM_SIZE: u64 = 512 << 20;
pub const COMMAND_LINE: &str = "console=ttyAMA0,115200 earlycon=pl011,mmio32,0x09000000 rdinit=/init panic=-1 nokaslr acpi=off pci=off";

/// Exactly the two userspace devices the existing minimal FDT advertises.
#[derive(Debug)]
pub struct ArmDevices {
    uart: Pl011,
    rtc: Pl031,
}

impl ArmDevices {
    pub fn new(
        output: Box<dyn Write + Send>,
        uart_irq: Arc<dyn IrqLine>,
        rtc_irq: Arc<dyn IrqLine>,
    ) -> Self {
        Self {
            uart: Pl011::new(layout::UART_BASE, output, uart_irq),
            rtc: Pl031::new(layout::RTC_BASE, rtc_irq),
        }
    }

    fn device(&mut self, addr: u64, len: usize) -> Result<(&mut dyn BusDevice, u64), String> {
        if !matches!(len, 1 | 2 | 4 | 8) {
            return Err("unsupported ARM MMIO width".into());
        }
        for (base, uart) in [(layout::UART_BASE, true), (layout::RTC_BASE, false)] {
            if let Some(offset) = addr
                .checked_sub(base)
                .filter(|off| *off <= layout::MMIO_DEV_SIZE - len as u64)
            {
                let device: &mut dyn BusDevice = if uart { &mut self.uart } else { &mut self.rtc };
                return Ok((device, offset));
            }
        }
        Err(format!("unmapped ARM MMIO address {addr:#x}"))
    }

    pub fn read(&mut self, addr: u64, data: &mut [u8]) -> Result<(), String> {
        let (device, offset) = self.device(addr, data.len())?;
        device.read(offset, data);
        Ok(())
    }

    pub fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), String> {
        let (device, offset) = self.device(addr, data.len())?;
        device.write(offset, data);
        Ok(())
    }
}

#[derive(Debug)]
pub struct ArmBoot {
    kernel: Vec<u8>,
    initramfs: Vec<u8>,
}

impl ArmBoot {
    pub fn new(kernel: Vec<u8>, initramfs: Vec<u8>) -> Result<Self, String> {
        if kernel.is_empty()
            || kernel.len() > 128 << 20
            || initramfs.is_empty()
            || initramfs.len() > 8 << 20
        {
            return Err("ARM kernel/initramfs input exceeds the fixed profile bounds".into());
        }
        let kernel = vroom_arch::decompress_if_needed(kernel).map_err(|e| e.to_string())?;
        Ok(Self { kernel, initramfs })
    }

    pub fn load(&self, memory: &GuestMemory, mpidr: u64) -> Result<BootRegs, String> {
        let mut regions = memory.regions();
        let ram = regions.next().ok_or("missing ARM RAM")?;
        if ram.start() != GuestAddress(layout::RAM_BASE)
            || ram.len() != RAM_SIZE
            || regions.next().is_some()
        {
            return Err("ARM harness requires one 512MiB RAM region at 0x40000000".into());
        }
        // Only the one boot CPU is described. Its MPIDR comes from the actual
        // vCPU; the existing FDT encodes Aff0..Aff2 in one cell.
        if mpidr & 0xff00_0000_00 != 0 {
            return Err("ARM harness cannot encode a nonzero MPIDR Aff3".into());
        }
        let kernel = board::load_kernel(memory, layout::RAM_BASE, &self.kernel)
            .map_err(|e| e.to_string())?;
        let plan = board::place_boot_blobs(
            (layout::RAM_BASE, RAM_SIZE),
            kernel.end,
            Some(self.initramfs.len() as u64),
        )
        .map_err(|e| e.to_string())?;
        let initrd = plan.initrd_addr.ok_or("missing ARM initramfs address")?;
        let cfg = board::FdtConfig {
            cpu_mpidrs: vec![mpidr],
            ram: vec![(layout::RAM_BASE, RAM_SIZE)],
            cmdline: COMMAND_LINE.into(),
            initrd: Some((initrd, self.initramfs.len() as u64)),
            gic_dist_size: 0x1_0000,
            gic_redist_size: 0x2_0000,
            gic_msi: None,
            devices: Vec::new(),
            psci_smc: false,
            model: "linux,dummy-virt".into(),
        };
        let fdt = board::create_fdt(&cfg).map_err(|e| e.to_string())?;
        if fdt.len() > board::fdt::FDT_MAX_SIZE {
            return Err("ARM FDT exceeds reserved region".into());
        }
        board::load_initrd(memory, initrd, &self.initramfs).map_err(|e| e.to_string())?;
        memory
            .write(GuestAddress(plan.fdt_addr), &fdt)
            .map_err(|e| e.to_string())?;
        Ok(BootRegs::Aarch64 {
            pc: kernel.entry,
            x: [plan.fdt_addr, 0, 0, 0],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> Vec<u8> {
        let mut image = vec![0; 4096];
        image[16..24].copy_from_slice(&(2u64 << 20).to_le_bytes());
        image[56..60].copy_from_slice(&0x644d_5241u32.to_le_bytes());
        image
    }

    #[test]
    fn direct_boot_populates_fdt_and_preserves_reserved_registers() {
        let memory = GuestMemory::new(&[(GuestAddress(layout::RAM_BASE), RAM_SIZE)]).unwrap();
        let regs = ArmBoot::new(image(), b"test initramfs".to_vec())
            .unwrap()
            .load(&memory, 0)
            .unwrap();
        let BootRegs::Aarch64 { pc, x } = regs else {
            panic!("wrong boot architecture")
        };
        assert_eq!(pc, layout::RAM_BASE);
        assert_eq!(&x[1..], &[0; 3]);
        assert_eq!(x[0], layout::RAM_BASE + RAM_SIZE - (2 << 20));
        let mut header = [0; 8];
        memory.read(GuestAddress(x[0]), &mut header).unwrap();
        assert_eq!(&header[..4], &[0xd0, 0x0d, 0xfe, 0xed]);
        let mut fdt = vec![0; u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize];
        memory.read(GuestAddress(x[0]), &mut fdt).unwrap();
        for name in [
            b"pl011@9000000\0".as_slice(),
            b"pl031@9010000\0",
            b"arm,gic-v3\0",
            b"hvc\0",
            b"linux,initrd-start\0",
            COMMAND_LINE.as_bytes(),
        ] {
            assert!(fdt.windows(name.len()).any(|bytes| bytes == name));
        }
        assert!(!fdt.windows(11).any(|bytes| bytes == b"pci-host-ec"));
        let initrd = x[0] - 4096;
        let mut contents = [0; 14];
        memory.read(GuestAddress(initrd), &mut contents).unwrap();
        assert_eq!(&contents, b"test initramfs");
    }

    #[test]
    fn malformed_images_wrong_memory_and_affinity_are_rejected() {
        let memory = GuestMemory::new(&[(GuestAddress(layout::RAM_BASE), RAM_SIZE)]).unwrap();
        for kernel in [vec![0; 8], vec![0; 4096]] {
            assert!(
                ArmBoot::new(kernel, vec![1])
                    .unwrap()
                    .load(&memory, 0)
                    .is_err()
            );
        }
        let boot = ArmBoot::new(image(), vec![1]).unwrap();
        assert!(boot.load(&memory, 1 << 32).is_err());
        let wrong =
            GuestMemory::new(&[(GuestAddress(0), vroom_memory::page_size() as u64)]).unwrap();
        assert!(boot.load(&wrong, 0).is_err());
        assert!(ArmBoot::new(image(), Vec::new()).is_err());
    }

    #[test]
    fn mmio_routes_real_device_ids_and_rejects_cross_boundary_or_unknown_access() {
        let irq = Arc::new(vroom_devices::NoIrq);
        let mut devices = ArmDevices::new(Box::new(std::io::sink()), irq.clone(), irq);
        let mut bytes = [0; 4];
        devices.read(layout::UART_BASE + 0xfe0, &mut bytes).unwrap();
        assert_eq!(u32::from_le_bytes(bytes), 0x11);
        devices.read(layout::RTC_BASE + 0xfe0, &mut bytes).unwrap();
        assert_eq!(u32::from_le_bytes(bytes), 0x31);
        assert!(
            devices
                .read(layout::UART_BASE + layout::MMIO_DEV_SIZE - 1, &mut bytes)
                .is_err()
        );
        assert!(devices.write(layout::FW_CFG_BASE, &[0; 4]).is_err());
        assert!(devices.read(layout::UART_BASE, &mut []).is_err());
    }
}

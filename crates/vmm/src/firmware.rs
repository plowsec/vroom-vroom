//! UEFI firmware boot: EDK2 (ArmVirtQemu build) in flash, per-VM NVRAM,
//! ACPI tables and ramfb through fw_cfg.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use bulwark_arch::aarch64::{acpi, layout};
use bulwark_arch::acpi::{LOADER_FILE, RSDP_FILE, TABLES_FILE};
use bulwark_devices::fw_cfg::FwCfg;
use bulwark_devices::pflash::PFlash;
use bulwark_hv::{IrqChipInfo, Vm};
use bulwark_memory::{GuestMemory, MmapRegion};

use crate::display::FbSink;
use crate::machine::DeviceRegistry;
use crate::{ramfb, Error, Result};

/// Where distros / Homebrew put the AArch64 EDK2 build.
const CODE_SEARCH: &[&str] = &[
    "/opt/homebrew/share/qemu/edk2-aarch64-code.fd",
    "/usr/local/share/qemu/edk2-aarch64-code.fd",
    "/usr/share/AAVMF/AAVMF_CODE.fd",
    "/usr/share/qemu-efi-aarch64/QEMU_EFI.fd",
    "/usr/share/edk2/aarch64/QEMU_EFI.fd",
];
const VARS_SEARCH: &[&str] = &[
    "/opt/homebrew/share/qemu/edk2-arm-vars.fd",
    "/usr/local/share/qemu/edk2-arm-vars.fd",
    "/usr/share/AAVMF/AAVMF_VARS.fd",
];

#[derive(Debug, Clone)]
pub(crate) struct Firmware {
    pub code: Option<PathBuf>,
    pub vars: PathBuf,
}

fn find(list: &[&str]) -> Option<PathBuf> {
    list.iter().map(PathBuf::from).find(|p| p.is_file())
}

/// Create the per-VM variable store from the distro template (or blank).
fn ensure_vars(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let mut data = match find(VARS_SEARCH) {
        Some(t) => std::fs::read(&t).map_err(|e| Error::Setup(format!("vars template {}: {e}", t.display())))?,
        None => Vec::new(),
    };
    data.resize(layout::FLASH_SIZE as usize, 0xff);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    std::io::Write::write_all(&mut o.open(path)?, &data)?;
    Ok(())
}

impl Firmware {
    #[allow(clippy::too_many_arguments)]
    pub fn setup(
        &self,
        vm: &Arc<dyn Vm>,
        mem: &GuestMemory,
        reg: &mut DeviceRegistry,
        cpus: u32,
        mpidrs: &[u64],
        info: &IrqChipInfo,
        pci: bool,
        psci_smc: bool,
        virtio_mmio: Vec<(u64, u64, u32)>,
        screen: Option<Weak<FbSink>>,
        smbios: &bulwark_arch::smbios::SmbiosConfig,
    ) -> Result<()> {
        // Code: read-only, mapped straight into the guest.
        let code_path = match &self.code {
            Some(p) => p.clone(),
            None => find(CODE_SEARCH).ok_or_else(|| {
                Error::Setup("no UEFI firmware found; install qemu (for edk2-aarch64-code.fd) or set boot.firmware".into())
            })?,
        };
        let code = std::fs::read(&code_path).map_err(|e| Error::Setup(format!("{}: {e}", code_path.display())))?;
        if code.is_empty() || code.len() as u64 > layout::FLASH_SIZE {
            return Err(Error::Setup(format!("{}: bad firmware size", code_path.display())));
        }
        let region = Arc::new(MmapRegion::new(layout::FLASH_SIZE as usize)?);
        region.load(&code)?;
        vm.map_readonly(layout::FLASH0_BASE, region)?;

        // Variables: emulated CFI flash, persisted per VM.
        ensure_vars(&self.vars)?;
        let file = std::fs::OpenOptions::new().read(true).write(true).open(&self.vars)?;
        let mut contents = std::fs::read(&self.vars)?;
        contents.truncate(layout::FLASH_SIZE as usize);
        let flash = PFlash::new(layout::FLASH1_BASE, layout::FLASH_SIZE, contents, Some(file)).map_err(Error::Setup)?;
        reg.add(layout::FLASH1_BASE, layout::FLASH_SIZE, Arc::new(Mutex::new(flash)))?;

        // fw_cfg: ACPI + ramfb.
        let ram = mem.total_size();
        let mut fw = FwCfg::new(layout::FW_CFG_BASE, mem.clone(), cpus as u16, ram);
        let tables = acpi::build(&acpi::AcpiConfig {
            cpu_mpidrs: mpidrs.to_vec(),
            gic_dist_size: info.dist_size,
            gic_redist_size: info.redist_size_per_cpu,
            gic_msi: Some((layout::GIC_MSI_BASE, info.msi_size)),
            pci,
            virtio_mmio,
            psci_smc,
        });
        fw.add_file(RSDP_FILE, tables.rsdp, None).map_err(Error::Setup)?;
        fw.add_file(TABLES_FILE, tables.tables, None).map_err(Error::Setup)?;
        fw.add_file(LOADER_FILE, tables.loader, None).map_err(Error::Setup)?;
        let sm = bulwark_arch::smbios::build(smbios);
        fw.add_file(bulwark_arch::smbios::TABLES_FILE, sm.tables, None).map_err(Error::Setup)?;
        fw.add_file(bulwark_arch::smbios::ANCHOR_FILE, sm.anchor, None).map_err(Error::Setup)?;
        if let Some(fb) = screen {
            let cfg = Arc::new(Mutex::new(None));
            let c2 = cfg.clone();
            let m2 = mem.clone();
            let hook = Box::new(move |d: &[u8]| {
                let parsed = ramfb::parse(d, &m2);
                if parsed.is_none() && d.iter().any(|&b| b != 0) {
                    log::warn!("ramfb: guest config rejected: {:02x?}", d);
                }
                *c2.lock().unwrap_or_else(|e| e.into_inner()) = parsed;
            });
            fw.add_file(ramfb::FILE, vec![0; ramfb::CONFIG_LEN], Some(hook)).map_err(Error::Setup)?;
            ramfb::spawn(mem.clone(), cfg, fb)?;
        }
        reg.add(layout::FW_CFG_BASE, layout::MMIO_DEV_SIZE, Arc::new(Mutex::new(fw)))?;
        Ok(())
    }
}

//! UEFI firmware boot: EDK2 (ArmVirtQemu build) in flash, per-VM NVRAM,
//! ACPI tables and ramfb through fw_cfg. Secure Boot uses Debian's build
//! (scripts/make-firmware.sh), whose NVRAM template has Microsoft's keys.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use vroom_arch::aarch64::{acpi, layout};
use vroom_arch::acpi::{LOADER_FILE, RSDP_FILE, TABLES_FILE};
use vroom_devices::fw_cfg::FwCfg;
use vroom_devices::pflash::PFlash;
use vroom_hv::{IrqChipInfo, Vm};
use vroom_memory::{GuestMemory, MmapRegion};

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
/// Debian's names; vroom.app has them in Resources/firmware.
const SECURE_CODE: &str = "AAVMF_CODE.secboot.fd";
const SECURE_VARS: &str = "AAVMF_VARS.ms.fd";

#[derive(Debug, Clone)]
pub(crate) struct Firmware {
    pub code: Option<PathBuf>,
    pub vars: PathBuf,
    pub secure_boot: bool,
    /// Resuming a suspended VM: its NVRAM stays as it is.
    pub resuming: bool,
}

fn find(list: &[&str]) -> Option<PathBuf> {
    list.iter().map(PathBuf::from).find(|p| p.is_file())
}

/// A Secure Boot file: next to the running program (vroom.app's
/// Resources, or a build's target/ folder), else the distro's.
fn find_secure(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok();
    let here = exe.as_deref().and_then(Path::parent);
    let mut dirs: Vec<PathBuf> = here
        .map(|h| ["../Resources/firmware", "firmware", "../firmware", "../../firmware"].iter().map(|d| h.join(d)).collect())
        .unwrap_or_default();
    dirs.push("/usr/share/AAVMF".into());
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

/// Create the per-VM variable store from `template` (or blank).
fn ensure_vars(path: &Path, template: Option<PathBuf>) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let mut data = match template {
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

const GLOBAL_VARIABLE: [u8; 16] = [0x61, 0xdf, 0xe4, 0x8b, 0xca, 0x93, 0xd2, 0x11, 0xaa, 0x0d, 0x00, 0xe0, 0x98, 0x03, 0x2b, 0x8c];
const AUTH_VARIABLE_STORE: [u8; 16] = [0x78, 0x2c, 0xf3, 0xaa, 0x7b, 0x94, 0x9a, 0x43, 0xa1, 0x80, 0x2e, 0x14, 0x4e, 0xc3, 0x77, 0x92];

/// Whether an EDK2 variable store holds a platform key (`PK`), so
/// Secure Boot is in user mode rather than setup mode.
fn has_platform_key(store: &[u8]) -> bool {
    let u16_at = |o: usize| store.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |o: usize| store.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    // Firmware volume header, then the variable store header.
    let Some(vs) = u16_at(0x30).map(usize::from) else { return false };
    if store.get(vs..vs + 16) != Some(&AUTH_VARIABLE_STORE[..]) {
        return false;
    }
    let Some(end) = u32_at(vs + 16).map(|s| vs + s as usize) else { return false };
    let mut o = vs + 28;
    // Authenticated headers: StartId, State, Attributes, count, time,
    // key index, NameSize at 36, DataSize at 40, VendorGuid at 44.
    while o + 60 <= end.min(store.len()) && u16_at(o) == Some(0x55aa) {
        let (Some(ns), Some(ds)) = (u32_at(o + 36), u32_at(o + 40)) else { return false };
        let (ns, ds) = (ns as usize, ds as usize);
        // VAR_ADDED, or added and then marked in-delete-transition.
        let live = matches!(store[o + 2], 0x3f | 0x3e);
        if live && store[o + 44..o + 60] == GLOBAL_VARIABLE && store.get(o + 60..o + 60 + ns) == Some(&b"P\0K\0\0\0"[..]) {
            return true;
        }
        o = (o + 60 + ns + ds + 3) & !3;
    }
    false
}

impl Firmware {
    /// Standard OVMF CODE/VARS pair, ending at the x86 reset vector.
    #[allow(clippy::too_many_arguments)]
    pub fn setup_x86(
        &self, vm: &Arc<dyn Vm>, mem: &GuestMemory, reg: &mut DeviceRegistry,
        cpus: u32, ram: u64, tpm: bool, screen: Option<Weak<FbSink>>,
        smbios: &vroom_arch::smbios::SmbiosConfig, ids: &vroom_arch::acpi::AcpiIds,
    ) -> Result<()> {
        use vroom_arch::x86_64::{acpi as pc_acpi, pc_layout as pc};
        if self.secure_boot { return Err(Error::Unavailable("Secure Boot is unavailable on the experimental PC board")); }
        // Prefer the current 4 MiB format, matching the vars template.
        const PAIRS: &[(&str, &str)] = &[
            ("/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_VARS_4M.fd"),
            ("/usr/share/OVMF/x64/OVMF_CODE.4m.fd", "/usr/share/OVMF/x64/OVMF_VARS.4m.fd"),
            ("/usr/share/edk2/x64/OVMF_CODE.4m.fd", "/usr/share/edk2/x64/OVMF_VARS.4m.fd"),
            ("/usr/share/edk2/ovmf/OVMF_CODE.fd", "/usr/share/edk2/ovmf/OVMF_VARS.fd"),
            ("/usr/share/OVMF/OVMF_CODE.fd", "/usr/share/OVMF/OVMF_VARS.fd"),
        ];
        let code_path = self.code.clone().or_else(|| PAIRS.iter().map(|p| PathBuf::from(p.0)).find(|p| p.is_file()))
            .ok_or_else(|| Error::Setup("no OVMF firmware; install ovmf/edk2-ovmf or set boot.firmware".into()))?;
        let code = std::fs::read(&code_path)?;
        if code.is_empty() || code.len() > 16 << 20 || !code.len().is_multiple_of(4096) {
            return Err(Error::Setup("OVMF CODE must be a nonempty page-aligned image <=16 MiB".into()));
        }
        if !self.vars.exists() {
            let template = PAIRS.iter().find(|(c, _)| Path::new(c) == code_path).map(|(_, v)| PathBuf::from(v))
                .or_else(|| {
                    let name = code_path.file_name()?.to_str()?.replace("CODE", "VARS");
                    Some(code_path.with_file_name(name))
                }).filter(|p| p.is_file())
                .ok_or_else(|| Error::Setup("OVMF needs a matching initialized VARS template; supply boot.vars or place OVMF_VARS beside CODE".into()))?;
            let contents = std::fs::read(template)?;
            if let Some(dir) = self.vars.parent() { std::fs::create_dir_all(dir)?; }
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let file = options.open(&self.vars)?;
            std::io::Write::write_all(&mut &file, &contents)?;
        }
        let vars = std::fs::read(&self.vars)?;
        let total = code.len() + vars.len();
        if vars.is_empty() || !vars.len().is_multiple_of(4096) || ![2 << 20, 4 << 20].contains(&total) {
            return Err(Error::Setup("OVMF CODE and VARS must form a matching 2 or 4 MiB flash image".into()));
        }
        let code_base = (1u64 << 32) - code.len() as u64;
        let region = Arc::new(MmapRegion::new(code.len())?); region.load(&code)?;
        vm.map_readonly(code_base, region)?;
        let vars_base = (1u64 << 32) - total as u64;
        let file = std::fs::OpenOptions::new().read(true).write(true).open(&self.vars)?;
        let flash = PFlash::new_pc(vars_base, vars, Some(file)).map_err(Error::Setup)?;
        reg.add(vars_base, total as u64 - code.len() as u64, Arc::new(Mutex::new(flash)))?;
        let mut fw = FwCfg::new_pio(mem.clone(), cpus as u16, ram);
        fw.skip_boot_menu().map_err(Error::Setup)?;
        fw.set(0x8000, 0u32.to_le_bytes().to_vec()); // no legacy ACPI tables
        fw.set(0x8002, 1u32.to_le_bytes().to_vec()); // IRQ0 override
        let tables = pc_acpi::build(cpus, tpm, ids, ram);
        fw.add_file(RSDP_FILE, tables.rsdp, None).map_err(Error::Setup)?;
        fw.add_file(TABLES_FILE, tables.tables, None).map_err(Error::Setup)?;
        fw.add_file(LOADER_FILE, tables.loader, None).map_err(Error::Setup)?;
        fw.add_file("etc/e820", pc::firmware_e820(ram)?, None).map_err(Error::Setup)?;
        // Keep firmware allocation and ACPI's aperture in agreement.
        fw.add_file("opt/ovmf/X-PciMmio64Mb", b"32768".to_vec(), None).map_err(Error::Setup)?;
        fw.add_file("etc/boot-fail-wait", 0u32.to_le_bytes().to_vec(), None).map_err(Error::Setup)?;
        let sm = vroom_arch::smbios::build(smbios);
        fw.add_file(vroom_arch::smbios::TABLES_FILE, sm.tables, None).map_err(Error::Setup)?;
        fw.add_file(vroom_arch::smbios::ANCHOR_FILE, sm.anchor, None).map_err(Error::Setup)?;
        if let Some(fb) = screen {
            let cfg = Arc::new(Mutex::new(None)); let c2 = cfg.clone(); let m2 = mem.clone();
            fw.add_file(ramfb::FILE, vec![0; ramfb::CONFIG_LEN], Some(Box::new(move |d| {
                *c2.lock().unwrap_or_else(|e| e.into_inner()) = ramfb::parse(d, &m2);
            }))).map_err(Error::Setup)?;
            ramfb::spawn(mem.clone(), cfg, fb)?;
        }
        reg.add_pio(pc::FW_CFG_PORT, 12, Arc::new(Mutex::new(fw)))?;
        Ok(())
    }

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
        tpm: Option<u64>,
        smbios: &vroom_arch::smbios::SmbiosConfig,
        acpi_ids: &vroom_arch::acpi::AcpiIds,
    ) -> Result<()> {
        // Code: read-only, mapped straight into the guest.
        let code_path = match &self.code {
            Some(p) => p.clone(),
            None if self.secure_boot => find_secure(SECURE_CODE)
                .ok_or_else(|| Error::Setup(format!("no Secure Boot firmware ({SECURE_CODE}): scripts/make-firmware.sh")))?,
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

        // Variables: emulated CFI flash, persisted per VM. Secure Boot
        // wants Microsoft's keys: a store without them starts over.
        let template = if self.secure_boot {
            let t = find_secure(SECURE_VARS)
                .ok_or_else(|| Error::Setup(format!("no Secure Boot NVRAM template ({SECURE_VARS}): scripts/make-firmware.sh")))?;
            if !self.resuming && self.vars.exists() && !has_platform_key(&std::fs::read(&self.vars)?) {
                let mut bak = self.vars.clone().into_os_string();
                bak.push(".bak");
                std::fs::rename(&self.vars, &bak)?;
                log::info!("secure boot: new NVRAM with Microsoft's keys; the old one is {}", Path::new(&bak).display());
            }
            Some(t)
        } else {
            find(VARS_SEARCH)
        };
        ensure_vars(&self.vars, template)?;
        let file = std::fs::OpenOptions::new().read(true).write(true).open(&self.vars)?;
        let mut contents = std::fs::read(&self.vars)?;
        contents.truncate(layout::FLASH_SIZE as usize);
        let flash = PFlash::new(layout::FLASH1_BASE, layout::FLASH_SIZE, contents, Some(file)).map_err(Error::Setup)?;
        reg.add(layout::FLASH1_BASE, layout::FLASH_SIZE, Arc::new(Mutex::new(flash)))?;

        // fw_cfg: ACPI + ramfb.
        let ram = mem.total_size();
        let mut fw = FwCfg::new(layout::FW_CFG_BASE, mem.clone(), cpus as u16, ram);
        // HVF doesn't deliver EDK2's virtual timer interrupt at EL2.
        // Avoid waiting forever in the optional boot-menu countdown.
        if cfg!(target_os = "macos") && psci_smc {
            fw.skip_boot_menu().map_err(Error::Setup)?;
        }
        let tables = acpi::build(&acpi::AcpiConfig {
            cpu_mpidrs: mpidrs.to_vec(),
            gic_dist_size: info.dist_size,
            gic_redist_size: info.redist_size_per_cpu,
            gic_msi: Some((layout::GIC_MSI_BASE, info.msi_size)),
            pci,
            virtio_mmio,
            psci_smc,
            tpm,
            ids: acpi_ids.clone(),
        });
        fw.add_file(RSDP_FILE, tables.rsdp, None).map_err(Error::Setup)?;
        fw.add_file(TABLES_FILE, tables.tables, None).map_err(Error::Setup)?;
        fw.add_file(LOADER_FILE, tables.loader, None).map_err(Error::Setup)?;
        let sm = vroom_arch::smbios::build(smbios);
        fw.add_file(vroom_arch::smbios::TABLES_FILE, sm.tables, None).map_err(Error::Setup)?;
        fw.add_file(vroom_arch::smbios::ANCHOR_FILE, sm.anchor, None).map_err(Error::Setup)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A store as EDK2 lays it out, with one variable.
    fn store(name: &str, guid: [u8; 16], state: u8) -> Vec<u8> {
        let mut s = vec![0u8; 0x48];
        s[0x30..0x32].copy_from_slice(&0x48u16.to_le_bytes());
        s.extend_from_slice(&AUTH_VARIABLE_STORE);
        s.extend_from_slice(&0x1000u32.to_le_bytes());
        s.extend_from_slice(&[0x5a, 0xfe, 0, 0, 0, 0, 0, 0]);
        // A variable before it, so the walk has to step over one.
        for (n, g, st) in [("Lang", [9u8; 16], 0x3f), (name, guid, state)] {
            let n: Vec<u8> = n.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
            let mut h = vec![0u8; 60];
            h[0..2].copy_from_slice(&0x55aau16.to_le_bytes());
            h[2] = st;
            h[36..40].copy_from_slice(&(n.len() as u32).to_le_bytes());
            h[40..44].copy_from_slice(&3u32.to_le_bytes());
            h[44..60].copy_from_slice(&g);
            s.extend(h);
            s.extend(n);
            s.extend([1, 2, 3]);
            s.resize((s.len() + 3) & !3, 0xff);
        }
        s.resize(0x48 + 0x1000, 0xff);
        s
    }

    #[test]
    fn platform_key() {
        assert!(has_platform_key(&store("PK", GLOBAL_VARIABLE, 0x3f)));
        assert!(!has_platform_key(&store("PK", GLOBAL_VARIABLE, 0x3c)));
        assert!(!has_platform_key(&store("PK", [0; 16], 0x3f)));
        assert!(!has_platform_key(&store("PKx", GLOBAL_VARIABLE, 0x3f)));
        assert!(!has_platform_key(&[]));
        assert!(!has_platform_key(&[0xff; 0x2000]));
    }
}

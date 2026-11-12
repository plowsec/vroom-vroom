//! Windows 11 install sticks for ready-made VMs (what
//! scripts/make-windows-stick.sh does by hand): a raw FAT32 disk with an
//! MBR, holding the ISO's files, the virtio drivers and guest tools from
//! the guest tools' `windows` folder, and an answer file for this VM
//! (user, password, OpenSSH). Windows Setup on NVMe can't read the ISO
//! itself (2 KiB sectors); a GPT disk's EFI partition confuses it.
//!
//! macOS uses hdiutil; Linux uses 7z and mtools without mounting. An
//! `install.wim` of 4 GiB or more is split (FAT32's file size limit),
//! which needs wimlib (`brew install wimlib`).

use std::io::{Read, Seek, SeekFrom, Write};
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::ready::Account;

const TEMPLATE: &str = include_str!("autounattend.xml");

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// A Windows computer name from a VM name: 15 letters, digits or '-'.
pub fn computer_name(name: &str) -> String {
    let c: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(15).collect::<String>().to_ascii_uppercase();
    let c = c.trim_matches('-').to_string();
    if c.is_empty() || c.chars().all(|c| c.is_ascii_digit()) {
        "VROOM-WIN".into()
    } else {
        c
    }
}

/// A Windows user name: no characters Windows refuses, not too long.
pub fn user_ok(u: &str) -> bool {
    !u.is_empty() && u.len() <= 20 && !u.ends_with('.') && !u.chars().any(|c| c.is_control() || "\"/\\[]:;|=,+*?<>@".contains(c))
}

/// The answer file for this VM. `index`: the image in install.wim (Pro).
pub fn unattend(acct: &Account, computer: &str, index: u32) -> String {
    unattend_arch(acct, computer, index, cfg!(target_arch = "x86_64"))
}

fn unattend_arch(acct: &Account, computer: &str, index: u32, x64: bool) -> String {
    let ssh = if acct.ssh_key.is_some() {
        "        <SynchronousCommand wcm:action=\"add\">\n          <Order>2</Order>\n          \
         <CommandLine>cmd /c for %d in (D E F G H I J K) do if exist %d:\\vroom-ssh.ps1 powershell -NoProfile -ExecutionPolicy Bypass -File %d:\\vroom-ssh.ps1</CommandLine>\n          \
         <Description>OpenSSH server</Description>\n        </SynchronousCommand>\n"
    } else {
        ""
    };
    TEMPLATE.replace("processorArchitecture=\"arm64\"", if x64 { "processorArchitecture=\"amd64\"" } else { "processorArchitecture=\"arm64\"" })
        .replace("\\ARM64", if x64 { "\\amd64" } else { "\\ARM64" })
        .replace("@USER@", &xml(&acct.user))
        .replace("@PASSWORD@", &xml(&acct.password))
        .replace("@COMPUTER@", &xml(computer))
        .replace("@INDEX@", &index.to_string())
        .replace("@SSH@", ssh)
}

/// Turns on Windows' OpenSSH server for `key` (an administrator's key).
/// The server is a Windows feature it downloads: it waits for the network.
pub fn ssh_script() -> &'static str {
    "$ErrorActionPreference = 'Continue'\r\n\
     $key = Get-Content -Raw (Join-Path $PSScriptRoot 'vroom-ssh.pub')\r\n\
     for ($i = 0; $i -lt 60; $i++) { if (Test-Connection -Count 1 -Quiet www.microsoft.com) { break }; Start-Sleep 5 }\r\n\
     Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0\r\n\
     Set-Service sshd -StartupType Automatic\r\n\
     $f = Join-Path $env:ProgramData 'ssh\\administrators_authorized_keys'\r\n\
     New-Item -ItemType Directory -Force (Split-Path $f) | Out-Null\r\n\
     Set-Content -Path $f -Value $key -Encoding ascii\r\n\
     icacls $f /inheritance:r /grant 'Administrators:F' /grant 'SYSTEM:F' | Out-Null\r\n\
     Start-Service sshd\r\n\
     # Windows' own rule is for private networks only; the VM's NAT network is public.\r\n\
     if (Get-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -ErrorAction SilentlyContinue) {\r\n  \
       Set-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -Profile Any -Enabled True\r\n\
     } else {\r\n  \
       New-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -DisplayName 'OpenSSH Server (sshd)' -Profile Any -Enabled True -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22 | Out-Null\r\n\
     }\r\n\
     'done' | Out-File C:\\vroom-ssh.done\r\n"
}

/// The index of the Pro edition in a WIM (its XML lists the editions),
/// or the only one there is. Errors as None (the caller takes 1).
pub fn pro_index(wim: &Path) -> Option<u32> {
    let mut f = std::fs::File::open(wim).ok()?;
    let mut h = [0u8; 96];
    f.read_exact(&mut h).ok()?;
    if &h[..5] != b"MSWIM" {
        return None;
    }
    let size = u64::from_le_bytes(h[72..80].try_into().ok()?) & 0x00ff_ffff_ffff_ffff;
    let off = u64::from_le_bytes(h[80..88].try_into().ok()?);
    if size == 0 || size > 16 << 20 {
        return None;
    }
    f.seek(SeekFrom::Start(off)).ok()?;
    let mut b = vec![0u8; size as usize];
    f.read_exact(&mut b).ok()?;
    let w: Vec<u16> = (0..b.len() / 2).map(|i| u16::from_le_bytes([b[2 * i], b[2 * i + 1]])).collect();
    let x = String::from_utf16_lossy(&w);
    let mut found = None;
    let mut count = 0;
    for part in x.split("<IMAGE INDEX=\"").skip(1) {
        count += 1;
        let n: u32 = part.split('"').next()?.parse().ok()?;
        if part.split("</IMAGE>").next().is_some_and(|img| img.contains("<EDITIONID>Professional</EDITIONID>")) {
            found = Some(n);
        }
    }
    found.or((count == 1).then_some(1))
}

fn run(c: &mut Command) -> Result<(), String> {
    let o = c.output().map_err(|e| format!("{:?}: {e}", c.get_program()))?;
    if o.status.success() {
        Ok(())
    } else {
        let why = if o.stderr.is_empty() { &o.stdout } else { &o.stderr };
        Err(format!("{:?} {}: {}", c.get_program(), o.status, String::from_utf8_lossy(why).trim()))
    }
}

/// Attach `image` at `at`; returns its device (/dev/diskN), which is what
/// gets detached (a mount point can be gone with the device still there).
fn attach(image: &Path, at: &Path, readonly: bool) -> Result<String, String> {
    let mut c = Command::new("hdiutil");
    c.args(["attach", "-nobrowse", "-mountpoint"]).arg(at);
    if readonly {
        c.arg("-readonly");
    }
    let o = c.arg(image).output().map_err(|e| format!("hdiutil: {e}"))?;
    if !o.status.success() {
        return Err(format!("hdiutil attach {}: {}", image.display(), String::from_utf8_lossy(&o.stderr).trim()));
    }
    String::from_utf8_lossy(&o.stdout)
        .split_whitespace()
        // The whole disk comes first: "/dev/disk6  FDisk_partition_scheme".
        .find(|w| w.starts_with("/dev/disk"))
        .map(str::to_string)
        .ok_or_else(|| format!("hdiutil attach {}: no device", image.display()))
}

/// Detach a disk we wrote: macOS (Spotlight) may still hold it for a
/// moment, so a few tries, then by force (our writes are on it by then).
fn detach(dev: &str) -> Result<(), String> {
    let _ = Command::new("sync").output();
    for _ in 0..5 {
        if run(Command::new("hdiutil").args(["detach", "-quiet", dev])).is_ok() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    let r = run(Command::new("hdiutil").args(["detach", "-quiet", "-force", dev]));
    if r.is_err() {
        std::thread::sleep(std::time::Duration::from_secs(2));
        return run(Command::new("hdiutil").args(["detach", "-quiet", "-force", dev]));
    }
    r
}

/// Detaches what it attached, removes its scratch folder.
struct Scratch {
    dir: PathBuf,
    devices: Vec<String>,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for d in self.devices.iter().rev() {
            let _ = detach(d);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn files_size(dir: &Path) -> u64 {
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => n += files_size(&e.path()),
                Ok(_) => n += e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => {}
            }
        }
    }
    n
}

/// Copy a tree, telling `done` the bytes copied so far. `skip`: names
/// (relative paths) left out.
fn copy_tree(from: &Path, to: &Path, rel: &str, skip: &[&str], done: &mut dyn FnMut(u64)) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let rd = std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        if skip.iter().any(|s| s.eq_ignore_ascii_case(&r)) || name.starts_with("._") {
            continue;
        }
        let t = e.file_type().map_err(|e| e.to_string())?;
        if t.is_dir() {
            copy_tree(&e.path(), &to.join(&name), &r, skip, done)?;
        } else if t.is_file() {
            // In pieces, so a big file (install.wim) shows progress.
            let mut src = std::fs::File::open(e.path()).map_err(|e| format!("{r}: {e}"))?;
            let mut dst = std::fs::File::create(to.join(&name)).map_err(|e| format!("{r}: {e}"))?;
            let mut buf = vec![0u8; 8 << 20];
            loop {
                let n = src.read(&mut buf).map_err(|e| format!("{r}: {e}"))?;
                if n == 0 {
                    break;
                }
                dst.write_all(&buf[..n]).map_err(|e| format!("{r}: {e}"))?;
                done(n as u64);
            }
        }
    }
    Ok(())
}

/// Counts copied bytes into `copied`; says the percentage every 5%.
fn ticker<'a>(copied: &'a mut u64, last: &'a mut u64, total: u64, progress: &'a mut dyn FnMut(&str)) -> impl FnMut(u64) + 'a {
    move |n| {
        *copied += n;
        let pct = (*copied * 100 / total.max(1)).min(99);
        if pct >= *last + 5 {
            *last = pct;
            progress(&format!("Copying Windows: {pct}%"));
        }
    }
}

/// Build the stick at `out` from the Windows ISO `iso`, with the guest
/// tools' `windows` folder `tools` and this VM's account. `progress` gets
/// short lines ("Copying Windows: 40%").
pub fn build(iso: &Path, tools: &Path, acct: &Account, computer: &str, out: &Path, progress: &mut dyn FnMut(&str)) -> Result<(), String> {
    if cfg!(target_os = "linux") { return build_linux(iso, tools, acct, computer, out, progress); }
    if !cfg!(target_os = "macos") {
        return Err("making a Windows install stick needs macOS (hdiutil)".into());
    }
    if !tools.join("NetKVM").is_dir() {
        return Err("the guest tools have no virtio drivers (build them with the virtio-win ISO)".into());
    }
    let parent = out.parent().ok_or("no folder for the stick")?;
    let mut s = Scratch { dir: parent.join(".winstick"), devices: Vec::new() };
    let _ = std::fs::remove_dir_all(&s.dir);
    std::fs::create_dir_all(&s.dir).map_err(|e| e.to_string())?;
    let (isom, usbm) = (s.dir.join("iso"), s.dir.join("usb"));
    std::fs::create_dir_all(&isom).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&usbm).map_err(|e| e.to_string())?;
    progress("Opening the Windows ISO");
    s.devices.push(attach(iso, &isom, true)?);
    if !isom.join("sources/boot.wim").is_file() {
        return Err("not a Windows install ISO (no sources/boot.wim)".into());
    }
    let efi = if cfg!(target_arch = "x86_64") { "efi/boot/bootx64.efi" } else { "efi/boot/bootaa64.efi" };
    if !isom.join(efi).is_file() {
        return Err(format!("Windows ISO has the wrong architecture (missing {efi})"));
    }
    let wim = isom.join("sources/install.wim");
    let esd = isom.join("sources/install.esd");
    let image = if wim.is_file() { wim.clone() } else { esd.clone() };
    let big = std::fs::metadata(&image).map(|m| m.len() >= 4 << 30).unwrap_or(false);
    if big && Command::new("wimlib-imagex").arg("--version").output().is_err() {
        return Err("this ISO's install image is over 4 GB: FAT32 can't hold it. Install wimlib (brew install wimlib) and try again".into());
    }
    let index = pro_index(&image).unwrap_or(1);
    let total = files_size(&isom) + files_size(tools);
    let mb = total / (1 << 20) + 1024;
    progress("Making the install disk");
    let dmg = s.dir.join("usb.dmg");
    run(Command::new("hdiutil").args(["create", "-quiet", "-size", &format!("{mb}m"), "-fs", "MS-DOS FAT32", "-volname", "WININST", "-layout", "MBRSPUD"]).arg(&dmg))?;
    let usb_dev = attach(&dmg, &usbm, false)?;
    s.devices.push(usb_dev.clone());
    let (mut copied, mut last) = (0u64, 0u64);
    let skip: &[&str] = if big { &["boot.catalog", "sources/install.wim", "sources/install.esd"] } else { &["boot.catalog"] };
    copy_tree(&isom, &usbm, "", skip, &mut ticker(&mut copied, &mut last, total, progress))?;
    if big {
        progress("Splitting the install image");
        run(Command::new("wimlib-imagex").arg("split").arg(&image).arg(usbm.join("sources/install.swm")).arg("3800"))?;
    }
    copy_tree(tools, &usbm, "", &[], &mut ticker(&mut copied, &mut last, total, progress))?;
    std::fs::write(usbm.join("autounattend.xml"), unattend(acct, computer, index)).map_err(|e| e.to_string())?;
    if let Some(k) = &acct.ssh_key {
        std::fs::write(usbm.join("vroom-ssh.ps1"), ssh_script()).map_err(|e| e.to_string())?;
        std::fs::write(usbm.join("vroom-ssh.pub"), format!("{}\r\n", k.trim())).map_err(|e| e.to_string())?;
    }
    progress("Finishing the install disk");
    detach(&usb_dev)?;
    s.devices.pop();
    let cdr = s.dir.join("usb.cdr");
    run(Command::new("hdiutil").args(["convert", "-quiet", "-format", "UDTO", "-o"]).arg(s.dir.join("usb")).arg(&dmg))?;
    let _ = std::fs::remove_file(&dmg);
    // Windows ignores MBR disks whose signature is zero; the partition
    // is marked active.
    let mut f = std::fs::OpenOptions::new().write(true).open(&cdr).map_err(|e| e.to_string())?;
    f.seek(SeekFrom::Start(440)).and_then(|_| f.write_all(b"VRMK")).map_err(|e| e.to_string())?;
    f.seek(SeekFrom::Start(446)).and_then(|_| f.write_all(&[0x80])).map_err(|e| e.to_string())?;
    drop(f);
    std::fs::rename(&cdr, out).map_err(|e| format!("{}: {e}", out.display()))?;
    Ok(())
}

/// A sparse MBR/FAT32 image populated with mtools. No root or loop mounts.
pub fn fat_image(tree: &Path, out: &Path, label: &str) -> Result<(), String> {
    if label.len() > 11 || !label.bytes().all(|b| b.is_ascii_alphanumeric()) { return Err("invalid FAT label".into()); }
    let size = (files_size(tree) + (256 << 20)).next_multiple_of(1 << 20).max(512 << 20);
    let sectors = size / 512 - 2048;
    let sectors = u32::try_from(sectors).map_err(|_| "install disk exceeds MBR capacity")?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600); // answer files may contain a local account password
    }
    let mut file = options.open(out).map_err(|e| e.to_string())?;
    file.set_len(size).map_err(|e| e.to_string())?;
    let mut mbr = [0u8; 512];
    // Windows offlines MBR volumes with duplicate signatures. An install
    // disk and a separate answer/tools disk must not all identify as VRMK.
    let signature = (std::collections::hash_map::RandomState::new().hash_one(out) as u32).max(1);
    mbr[440..444].copy_from_slice(&signature.to_le_bytes()); mbr[446] = 0x80; mbr[450] = 0x0c;
    mbr[447..450].copy_from_slice(&[0xfe, 0xff, 0xff]); mbr[451..454].copy_from_slice(&[0xfe, 0xff, 0xff]);
    mbr[454..458].copy_from_slice(&2048u32.to_le_bytes()); mbr[458..462].copy_from_slice(&sectors.to_le_bytes());
    mbr[510..].copy_from_slice(&[0x55, 0xaa]); file.write_all(&mbr).map_err(|e| e.to_string())?; drop(file);
    let image = format!("{}@@1048576", out.display());
    run(Command::new("mformat").args(["-i", &image, "-F", "-T", &sectors.to_string(), "-v", label, "::"]))?;
    for entry in std::fs::read_dir(tree).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        run(Command::new("mcopy").args(["-s", "-i", &image]).arg(entry.path()).arg("::"))?;
    }
    Ok(())
}

fn build_linux(iso: &Path, tools: &Path, acct: &Account, computer: &str, out: &Path, progress: &mut dyn FnMut(&str)) -> Result<(), String> {
    if !tools.join("NetKVM").is_dir() { return Err("guest tools have no virtio drivers; run scripts/make-guest-tools.sh".into()); }
    let scratch = tempfile::Builder::new().prefix(".winstick-").tempdir_in(out.parent().unwrap_or(Path::new("."))).map_err(|e| e.to_string())?;
    let isom = scratch.path().join("iso"); let usbm = scratch.path().join("usb");
    std::fs::create_dir(&isom).map_err(|e| e.to_string())?;
    progress("Opening the Windows ISO (needs 7z, mformat and mcopy)");
    run(Command::new("7z").args(["x", "-y"]).arg(format!("-o{}", isom.display())).arg(iso))?;
    let efi = if cfg!(target_arch = "x86_64") { "efi/boot/bootx64.efi" } else { "efi/boot/bootaa64.efi" };
    if !isom.join("sources/boot.wim").is_file() || !isom.join(efi).is_file() { return Err(format!("not a native Windows install ISO (need sources/boot.wim and {efi})")); }
    let wim = isom.join("sources/install.wim");
    let image = if wim.is_file() { wim } else { isom.join("sources/install.esd") };
    let big = std::fs::metadata(&image).map_err(|e| e.to_string())?.len() >= 4 << 30;
    let index = pro_index(&image).unwrap_or(1);
    let total = files_size(&isom) + files_size(tools); let (mut copied, mut last) = (0, 0);
    let skip: &[&str] = if big { &["boot.catalog", "boot.catalogue", "sources/install.wim", "sources/install.esd", "[BOOT]"] } else { &["boot.catalog", "boot.catalogue", "[BOOT]"] };
    copy_tree(&isom, &usbm, "", skip, &mut ticker(&mut copied, &mut last, total, progress))?;
    if big {
        progress("Splitting the install image (needs wimlib-imagex)");
        run(Command::new("wimlib-imagex").arg("split").arg(&image).arg(usbm.join("sources/install.swm")).arg("3800"))?;
    }
    copy_tree(tools, &usbm, "", &[], &mut ticker(&mut copied, &mut last, total, progress))?;
    std::fs::write(usbm.join("autounattend.xml"), unattend(acct, computer, index)).map_err(|e| e.to_string())?;
    if let Some(key) = &acct.ssh_key {
        std::fs::write(usbm.join("vroom-ssh.ps1"), ssh_script()).map_err(|e| e.to_string())?;
        std::fs::write(usbm.join("vroom-ssh.pub"), format!("{}\r\n", key.trim())).map_err(|e| e.to_string())?;
    }
    progress("Configuring unattended WinPE startup (needs wimlib-imagex)");
    unattended_boot_wim(&usbm.join("sources/boot.wim"), &usbm.join("autounattend.xml"), scratch.path())?;
    progress("Making the install disk");
    let image = scratch.path().join("install.img"); fat_image(&usbm, &image, "WININST")?;
    std::fs::rename(image, out).map_err(|e| e.to_string())
}

/// New Windows Setup builds do not reliably discover an answer file on
/// fixed NVMe media. Start Setup with it explicitly, before its GUI opens.
fn unattended_boot_wim(boot: &Path, answer: &Path, scratch: &Path) -> Result<(), String> {
    let mut header = [0u8; 128];
    std::fs::File::open(boot).and_then(|mut f| f.read_exact(&mut header)).map_err(|e| format!("boot.wim header: {e}"))?;
    let count = u32::from_le_bytes(header[44..48].try_into().unwrap());
    let index = u32::from_le_bytes(header[120..124].try_into().unwrap());
    if &header[..5] != b"MSWIM" || index == 0 || index > count {
        return Err("boot.wim has no valid bootable image".into());
    }
    let overlay = scratch.join("boot-overlay");
    std::fs::create_dir_all(overlay.join("Windows/System32")).map_err(|e| e.to_string())?;
    std::fs::write(overlay.join("Windows/System32/winpeshl.ini"),
        "[LaunchApps]\r\n%SYSTEMROOT%\\System32\\wpeinit.exe\r\n%SYSTEMROOT%\\System32\\cmd.exe, /c %SYSTEMDRIVE%\\vroom-start.cmd\r\n").map_err(|e| e.to_string())?;
    std::fs::write(overlay.join("vroom-start.cmd"),
        "@echo off\r\n%SYSTEMDRIVE%\\sources\\setup.exe /unattend:%SYSTEMDRIVE%\\vroom-unattend.xml\r\n").map_err(|e| e.to_string())?;
    std::fs::copy(answer, overlay.join("vroom-unattend.xml")).map_err(|e| e.to_string())?;
    let boot = boot.canonicalize().map_err(|e| e.to_string())?;
    run(Command::new("wimlib-imagex").arg("update").arg(boot).arg(index.to_string())
        .arg("--command=add boot-overlay /").current_dir(scratch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_architecture_and_driver_paths_match() {
        let acct = Account { user: "arm64".into(), password: "ARM64".into(), ssh_key: None };
        let x64 = unattend_arch(&acct, "X64", 1, true);
        assert!(x64.contains("processorArchitecture=\"amd64\""));
        assert!(x64.contains("\\w11\\amd64"));
        assert!(x64.contains("<Name>arm64</Name>"));
        let arm = unattend_arch(&acct, "ARM", 1, false);
        assert!(arm.contains("processorArchitecture=\"arm64\""));
        assert!(arm.contains("\\w11\\ARM64"));
    }

    #[test]
    #[ignore = "requires genisoimage, 7z, mtools and wimlib-imagex"]
    fn linux_install_stick_end_to_end() {
        let t = tempfile::tempdir().unwrap(); let root = t.path();
        let source = root.join("source"); let tools = root.join("tools");
        std::fs::create_dir_all(source.join("sources")).unwrap();
        std::fs::create_dir_all(source.join("efi/boot")).unwrap();
        std::fs::create_dir_all(tools.join("NetKVM")).unwrap();
        let winpe = root.join("winpe");
        std::fs::create_dir_all(winpe.join("Windows/System32")).unwrap();
        std::fs::write(winpe.join("Windows/System32/winpeshl.ini"), b"original startup").unwrap();
        run(Command::new("wimlib-imagex").arg("capture").arg(winpe).arg(source.join("sources/boot.wim")).arg("--boot")).unwrap();
        std::fs::write(source.join("sources/install.wim"), b"fake install image").unwrap();
        let efi = if cfg!(target_arch = "x86_64") { "bootx64.efi" } else { "bootaa64.efi" };
        std::fs::write(source.join("efi/boot").join(efi), b"fake UEFI loader").unwrap();
        std::fs::write(tools.join("vroom-setup.cmd"), b"echo test").unwrap();
        let iso = root.join("windows.iso");
        run(Command::new("genisoimage").args(["-quiet", "-r", "-J", "-o"]).arg(&iso).arg(&source)).unwrap();
        let out = root.join("install.img");
        let acct = Account { user: "test".into(), password: "secret".into(), ssh_key: Some("ssh-ed25519 AAAA test".into()) };
        build_linux(&iso, &tools, &acct, "TEST", &out, &mut |_| {}).unwrap();
        let mut file = std::fs::File::open(&out).unwrap(); let mut mbr = [0; 512]; file.read_exact(&mut mbr).unwrap();
        assert_eq!(&mbr[510..], &[0x55, 0xaa]); assert_eq!(mbr[450], 0x0c);
        assert_eq!(u32::from_le_bytes(mbr[454..458].try_into().unwrap()), 2048);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&out).unwrap().permissions().mode() & 0o777, 0o600);
        assert_ne!(&mbr[440..444], &[0, 0, 0, 0]);
        // A separately attached answer/tools disk must stay online in
        // Windows alongside the installer, rather than sharing its ID.
        let seed = root.join("seed.img");
        fat_image(&tools, &seed, "SEED").unwrap();
        let mut seed_mbr = [0u8; 512];
        std::fs::File::open(seed).unwrap().read_exact(&mut seed_mbr).unwrap();
        assert_ne!(&mbr[440..444], &seed_mbr[440..444]);
        let contents = Command::new("mtype").args(["-i", &format!("{}@@1048576", out.display()), "::/autounattend.xml"]).output().unwrap();
        assert!(contents.status.success());
        let x = String::from_utf8(contents.stdout).unwrap();
        assert!(x.contains("<Name>test</Name>")); assert!(x.contains("vroom-ssh.ps1"));
        assert!(x.contains(if cfg!(target_arch = "x86_64") { "processorArchitecture=\"amd64\"" } else { "processorArchitecture=\"arm64\"" }));
        let boot = root.join("packed-boot.wim");
        run(Command::new("mcopy").args(["-i", &format!("{}@@1048576", out.display()), "::/sources/boot.wim"]).arg(&boot)).unwrap();
        let extracted = root.join("boot-check");
        run(Command::new("wimlib-imagex").arg("extract").arg(boot).arg("1")
            .args(["Windows/System32/winpeshl.ini", "vroom-start.cmd", "vroom-unattend.xml"])
            .arg(format!("--dest-dir={}", extracted.display()))).unwrap();
        assert!(std::fs::read_to_string(extracted.join("winpeshl.ini")).unwrap().contains("vroom-start.cmd"));
        assert!(std::fs::read_to_string(extracted.join("vroom-start.cmd")).unwrap().contains("/unattend:"));
        assert_eq!(std::fs::read_to_string(extracted.join("vroom-unattend.xml")).unwrap(), x);
    }

    #[test]
    fn answers_are_escaped_and_complete() {
        let a = Account { user: "Ann & Co".into(), password: "p<w>\"".into(), ssh_key: Some("ssh-ed25519 AAAA x".into()) };
        let x = unattend(&a, "WEB-1", 6);
        assert!(x.contains("<Name>Ann &amp; Co</Name>"));
        assert!(x.contains("<Value>p&lt;w&gt;&quot;</Value>"));
        assert!(x.contains("<Value>6</Value>") && x.contains("<ComputerName>WEB-1</ComputerName>"));
        assert!(x.contains("vroom-ssh.ps1") && !x.contains('@'), "every placeholder filled");
        let x = unattend(&Account { ssh_key: None, ..a }, "WEB-1", 1);
        assert!(!x.contains("vroom-ssh.ps1"));
        // Well formed enough for Setup: every tag closed.
        assert_eq!(x.matches("<SynchronousCommand").count(), x.matches("</SynchronousCommand>").count());
    }

    #[test]
    fn names() {
        assert_eq!(computer_name("my win 11 vm, long name"), "MYWIN11VMLONGNA");
        assert_eq!(computer_name("123"), "VROOM-WIN");
        assert!(user_ok("vladimir") && !user_ok("a/b") && !user_ok("") && !user_ok("x."));
    }

    #[test]
    fn the_pro_edition_is_found() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("install.wim");
        let xml = "<WIM><IMAGE INDEX=\"1\"><WINDOWS><EDITIONID>Core</EDITIONID></WINDOWS></IMAGE>\
                   <IMAGE INDEX=\"6\"><WINDOWS><EDITIONID>Professional</EDITIONID></WINDOWS></IMAGE></WIM>";
        let x: Vec<u8> = [0xfeff_u16].into_iter().chain(xml.encode_utf16()).flat_map(u16::to_le_bytes).collect();
        let mut h = vec![0u8; 208];
        h[..8].copy_from_slice(b"MSWIM\0\0\0");
        h[72..80].copy_from_slice(&(x.len() as u64).to_le_bytes());
        h[80..88].copy_from_slice(&208u64.to_le_bytes());
        h.extend_from_slice(&x);
        std::fs::write(&p, &h).unwrap();
        assert_eq!(pro_index(&p), Some(6));
        std::fs::write(&p, b"not a wim").unwrap();
        assert_eq!(pro_index(&p), None);
    }
}

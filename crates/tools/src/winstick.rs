//! Windows 11 ARM64 install sticks for ready-made VMs (what
//! scripts/make-windows-stick.sh does by hand): a raw FAT32 disk with an
//! MBR, holding the ISO's files, the virtio drivers and guest tools from
//! the guest tools' `windows` folder, and an answer file for this VM
//! (user, password, OpenSSH). Windows Setup on NVMe can't read the ISO
//! itself (2 KiB sectors); a GPT disk's EFI partition confuses it.
//!
//! macOS only: it mounts the ISO and makes the disk with hdiutil. An
//! `install.wim` of 4 GiB or more is split (FAT32's file size limit),
//! which needs wimlib (`brew install wimlib`).

use std::io::{Read, Seek, SeekFrom, Write};
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
    let ssh = if acct.ssh_key.is_some() {
        "        <SynchronousCommand wcm:action=\"add\">\n          <Order>2</Order>\n          \
         <CommandLine>cmd /c for %d in (D E F G H I J K) do if exist %d:\\vroom-ssh.ps1 powershell -NoProfile -ExecutionPolicy Bypass -File %d:\\vroom-ssh.ps1</CommandLine>\n          \
         <Description>OpenSSH server</Description>\n        </SynchronousCommand>\n"
    } else {
        ""
    };
    TEMPLATE
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
        Err(format!("{:?}: {}", c.get_program(), String::from_utf8_lossy(&o.stderr).trim()))
    }
}

/// Unmounts what it mounted, removes its scratch folder.
struct Scratch {
    dir: PathBuf,
    mounts: Vec<PathBuf>,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for m in self.mounts.iter().rev() {
            let _ = Command::new("hdiutil").args(["detach", "-quiet", "-force"]).arg(m).output();
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
    if !cfg!(target_os = "macos") {
        return Err("making a Windows install stick needs macOS (hdiutil)".into());
    }
    if !tools.join("NetKVM").is_dir() {
        return Err("the guest tools have no virtio drivers (build them with the virtio-win ISO)".into());
    }
    let parent = out.parent().ok_or("no folder for the stick")?;
    let mut s = Scratch { dir: parent.join(".winstick"), mounts: Vec::new() };
    let _ = std::fs::remove_dir_all(&s.dir);
    std::fs::create_dir_all(&s.dir).map_err(|e| e.to_string())?;
    let (isom, usbm) = (s.dir.join("iso"), s.dir.join("usb"));
    std::fs::create_dir_all(&isom).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&usbm).map_err(|e| e.to_string())?;
    progress("Opening the Windows ISO");
    run(Command::new("hdiutil").args(["attach", "-quiet", "-nobrowse", "-readonly", "-mountpoint"]).arg(&isom).arg(iso))?;
    s.mounts.push(isom.clone());
    if !isom.join("sources/boot.wim").is_file() {
        return Err("not a Windows install ISO (no sources/boot.wim)".into());
    }
    if !isom.join("efi/boot/bootaa64.efi").is_file() {
        return Err("not an ARM64 Windows ISO (no efi/boot/bootaa64.efi)".into());
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
    run(Command::new("hdiutil").args(["attach", "-quiet", "-nobrowse", "-mountpoint"]).arg(&usbm).arg(&dmg))?;
    s.mounts.push(usbm.clone());
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
    run(Command::new("hdiutil").args(["detach", "-quiet"]).arg(&usbm))?;
    s.mounts.pop();
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

#[cfg(test)]
mod tests {
    use super::*;

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

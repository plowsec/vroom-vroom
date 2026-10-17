//! cloud-init seeds (NoCloud) for ready-made Ubuntu VMs: the user, its
//! password (hashed) and SSH key, the guest tools and what installs them,
//! optionally a desktop. On an ISO labelled `cidata`, which cloud-init
//! reads at first boot.

use std::path::Path;
use std::process::Command;

use sha2::{Digest, Sha512};

/// What goes into a seed.
#[derive(Debug, Clone)]
pub struct Seed<'a> {
    pub hostname: &'a str,
    pub user: &'a str,
    pub password: &'a str,
    /// An OpenSSH public key line ("ssh-ed25519 AAAA... me@mac"), if SSH.
    pub ssh_key: Option<&'a str>,
    /// Install Ubuntu's desktop (GNOME) and start in it.
    pub desktop: bool,
    /// Random, so this seed is a new instance for cloud-init.
    pub instance: [u8; 8],
}

/// A user name Ubuntu takes.
pub fn user_ok(u: &str) -> bool {
    let mut c = u.chars();
    matches!(c.next(), Some('a'..='z' | '_')) && u.len() <= 32 && c.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-'))
}

/// A host name: letters, digits and '-', not first or last.
pub fn hostname(name: &str) -> String {
    let h: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).take(63).collect();
    let h = h.trim_matches('-').to_string();
    if h.is_empty() {
        "vroom".into()
    } else {
        h
    }
}

/// One line of an OpenSSH public key, checked enough to go into YAML.
pub fn ssh_key_ok(k: &str) -> bool {
    let k = k.trim();
    let mut w = k.split_whitespace();
    let ty = w.next().unwrap_or("");
    let body = w.next().unwrap_or("");
    (ty.starts_with("ssh-") || ty.starts_with("ecdsa-") || ty.starts_with("sk-"))
        && body.len() > 16
        && body.bytes().all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b))
        && !k.chars().any(char::is_control)
        && k.len() < 16 << 10
}

/// A YAML double-quoted string.
fn yq(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if c.is_control() => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

impl Seed<'_> {
    pub fn meta_data(&self) -> String {
        let id: String = self.instance.iter().map(|b| format!("{b:02x}")).collect();
        format!("instance-id: iid-vroom-{id}\nlocal-hostname: {}\n", self.hostname)
    }

    pub fn user_data(&self) -> String {
        let salt: String = self.instance.iter().map(|b| SALT_CHARS[*b as usize % 64] as char).collect();
        let hash = sha512_crypt(self.password.as_bytes(), salt.as_bytes());
        let mut s = String::from("#cloud-config\n");
        s += &format!("hostname: {}\n", self.hostname);
        s += "users:\n";
        s += &format!("  - name: {}\n", self.user);
        s += "    groups: [adm, sudo]\n    shell: /bin/bash\n    sudo: \"ALL=(ALL) ALL\"\n    lock_passwd: false\n";
        s += &format!("    passwd: {}\n", yq(&hash));
        if let Some(k) = self.ssh_key {
            s += &format!("    ssh_authorized_keys:\n      - {}\n", yq(k.trim()));
        }
        // Keys only, never passwords, over SSH.
        s += "ssh_pwauth: false\n";
        if self.desktop {
            s += "package_update: true\npackages:\n  - ubuntu-desktop-minimal\n";
        }
        s += "runcmd:\n";
        // The guest tools are on this seed: mount it and install them.
        s += "  - [sh, -c, 'd=$(mktemp -d); for l in cidata CIDATA; do mount -o ro LABEL=$l \"$d\" 2>/dev/null && break; done; sh \"$d/vroom/linux/install-agent.sh\"; umount \"$d\"']\n";
        if self.ssh_key.is_none() {
            s += "  - [sh, -c, 'systemctl disable --now ssh.socket ssh.service 2>/dev/null || true']\n";
        }
        if self.desktop {
            s += "  - [systemctl, set-default, graphical.target]\n";
            // NetworkManager takes the network over from networkd: its
            // wait-online would hold every boot for 2 minutes.
            s += "  - [systemctl, mask, systemd-networkd-wait-online.service]\n";
            // No welcome wizard: the account is set up.
            let u = self.user;
            s += &format!(
                "  - [sh, -c, 'install -d -o {u} -g {u} /home/{u}/.config && echo yes > /home/{u}/.config/gnome-initial-setup-done && chown {u}:{u} /home/{u}/.config/gnome-initial-setup-done']\n"
            );
            // The desktop starts after a restart, set up.
            s += "power_state:\n  mode: reboot\n  condition: true\n";
        }
        s
    }

    /// Write the seed ISO to `out`, with the Linux guest tools from `tools`
    /// (the `linux` folder of the guest tools) on it.
    pub fn write(&self, out: &Path, tools: &Path) -> Result<(), String> {
        let tmp = out.with_extension("seed-tmp");
        let _ = std::fs::remove_dir_all(&tmp);
        let r = (|| {
            std::fs::create_dir_all(tmp.join("vroom")).map_err(|e| e.to_string())?;
            std::fs::write(tmp.join("user-data"), self.user_data()).map_err(|e| e.to_string())?;
            std::fs::write(tmp.join("meta-data"), self.meta_data()).map_err(|e| e.to_string())?;
            copy_tree(tools, &tmp.join("vroom/linux")).map_err(|e| format!("guest tools: {e}"))?;
            make_iso(&tmp, out, "cidata")
        })();
        let _ = std::fs::remove_dir_all(&tmp);
        r
    }
}

pub(crate) fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let name = e.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let t = e.file_type()?;
        if t.is_dir() {
            copy_tree(&e.path(), &to.join(&name))?;
        } else if t.is_file() {
            std::fs::copy(e.path(), to.join(&name))?;
        }
    }
    Ok(())
}

/// An ISO 9660 image (with Joliet names) of `dir`, labelled `label`.
pub fn make_iso(dir: &Path, out: &Path, label: &str) -> Result<(), String> {
    let _ = std::fs::remove_file(out);
    let mut tries: Vec<Command> = Vec::new();
    if cfg!(target_os = "macos") {
        let mut c = Command::new("hdiutil");
        c.args(["makehybrid", "-quiet", "-iso", "-joliet", "-default-volume-name", label, "-o"]).arg(out).arg(dir);
        tries.push(c);
    }
    for tool in ["genisoimage", "mkisofs"] {
        let mut c = Command::new(tool);
        c.args(["-quiet", "-J", "-R", "-V", label, "-o"]).arg(out).arg(dir);
        tries.push(c);
    }
    let mut c = Command::new("xorriso");
    c.args(["-as", "mkisofs", "-quiet", "-J", "-R", "-V", label, "-o"]).arg(out).arg(dir);
    tries.push(c);
    let mut last = String::new();
    for mut c in tries {
        match c.output() {
            Ok(o) if o.status.success() && out.is_file() => return Ok(()),
            Ok(o) => last = String::from_utf8_lossy(&o.stderr).trim().to_string(),
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!("can't make {}: {last} (needs hdiutil, genisoimage, mkisofs or xorriso)", out.display()))
}

const SALT_CHARS: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// SHA-512 crypt (`$6$`, 5000 rounds), as /etc/shadow keeps passwords.
/// Ulrich Drepper's "Unix crypt using SHA-256 and SHA-512".
pub fn sha512_crypt(pw: &[u8], salt: &[u8]) -> String {
    let salt = &salt[..salt.len().min(16)];
    let b = Sha512::new().chain_update(pw).chain_update(salt).chain_update(pw).finalize();
    let mut a = Sha512::new().chain_update(pw).chain_update(salt);
    let mut n = pw.len();
    while n > 64 {
        a.update(b);
        n -= 64;
    }
    a.update(&b[..n]);
    let mut n = pw.len();
    while n > 0 {
        if n & 1 != 0 {
            a.update(b);
        } else {
            a.update(pw);
        }
        n >>= 1;
    }
    let a = a.finalize();
    let mut dp = Sha512::new();
    for _ in 0..pw.len() {
        dp.update(pw);
    }
    let dp = dp.finalize();
    let p: Vec<u8> = dp.iter().copied().cycle().take(pw.len()).collect();
    let mut ds = Sha512::new();
    for _ in 0..16 + a[0] as usize {
        ds.update(salt);
    }
    let ds = ds.finalize();
    let s: Vec<u8> = ds.iter().copied().cycle().take(salt.len()).collect();
    let mut c = a;
    for i in 0..5000 {
        let mut h = Sha512::new();
        if i & 1 != 0 {
            h.update(&p);
        } else {
            h.update(c);
        }
        if i % 3 != 0 {
            h.update(&s);
        }
        if i % 7 != 0 {
            h.update(&p);
        }
        if i & 1 != 0 {
            h.update(c);
        } else {
            h.update(&p);
        }
        c = h.finalize();
    }
    const ORDER: [(usize, usize, usize); 21] = [
        (0, 21, 42),
        (22, 43, 1),
        (44, 2, 23),
        (3, 24, 45),
        (25, 46, 4),
        (47, 5, 26),
        (6, 27, 48),
        (28, 49, 7),
        (50, 8, 29),
        (9, 30, 51),
        (31, 52, 10),
        (53, 11, 32),
        (12, 33, 54),
        (34, 55, 13),
        (56, 14, 35),
        (15, 36, 57),
        (37, 58, 16),
        (59, 17, 38),
        (18, 39, 60),
        (40, 61, 19),
        (62, 20, 41),
    ];
    let mut out = format!("$6${}$", String::from_utf8_lossy(salt));
    let mut put = |w: u32, n: usize| {
        let mut w = w;
        for _ in 0..n {
            out.push(SALT_CHARS[(w & 0x3f) as usize] as char);
            w >>= 6;
        }
    };
    for (x, y, z) in ORDER {
        put((c[x] as u32) << 16 | (c[y] as u32) << 8 | c[z] as u32, 4);
    }
    put(c[63] as u32, 2);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_hash_like_glibc() {
        // The specification's example; the others from `openssl passwd -6`.
        assert_eq!(
            sha512_crypt(b"Hello world!", b"saltstring"),
            "$6$saltstring$svn8UoSVapNtMuq1ukKS4tPQd8iKwSMHWjl/O817G3uBnIFNjnQJuesI68u4OTLiBFdcbYEdFCoEOfaS35inz1"
        );
        assert_eq!(
            sha512_crypt(b"we have a short salt string but not a short password", b"short"),
            "$6$short$qmfj2meTBr5G2EAGIJ4vjX7RpefsD4JzpEyTAeEUJdzdxlBS6pe8gdMHm5zFftaFSj/2p2bjBwyVS9ZhWpLZt."
        );
        assert_eq!(
            sha512_crypt(&[b'x'; 100], b"abcdefghijklmnop"),
            "$6$abcdefghijklmnop$r/HtRGuCzMNedONV3I8Xz8Qm4Qy85pQ/RigX0agmbAzWsa3BCc1B564NfCD/Giyliby36KbTMkrVLsmFYtbTu1"
        );
    }

    #[test]
    fn a_seed_has_what_was_asked() {
        let s = Seed { hostname: "web-1", user: "me", password: "pw", ssh_key: Some("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA me@mac"), desktop: true, instance: [1; 8] };
        let u = s.user_data();
        assert!(u.starts_with("#cloud-config\n"));
        assert!(u.contains("name: me\n") && u.contains("passwd: \"$6$"));
        assert!(!u.contains("\"pw\""), "never the password itself");
        assert!(u.contains("- \"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA me@mac\""));
        assert!(u.contains("ubuntu-desktop-minimal") && u.contains("mode: reboot"));
        assert!(!u.contains("disable --now ssh"));
        let s = Seed { ssh_key: None, desktop: false, ..s };
        let u = s.user_data();
        assert!(u.contains("disable --now ssh") && !u.contains("packages"));
        assert!(s.meta_data().contains("instance-id: iid-vroom-0101010101010101"));
    }

    #[test]
    fn names_and_keys_are_checked() {
        assert!(user_ok("vladimir") && user_ok("_x-1") && !user_ok("Root") && !user_ok("1a") && !user_ok("a b") && !user_ok(""));
        assert_eq!(hostname("My VM_2"), "my-vm-2");
        assert_eq!(hostname("--"), "vroom");
        assert!(ssh_key_ok("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBaB me@mac\n"));
        assert!(!ssh_key_ok("ssh-ed25519 AAAA\"; rm -rf /"));
        assert!(!ssh_key_ok("not a key"));
        assert!(!ssh_key_ok("ssh-rsa AAAAB3NzaC1yc2EAAAADAQAB\nssh-rsa second"));
    }
}

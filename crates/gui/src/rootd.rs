//! vroom-netd, the helper that runs as root (bridged and host-only
//! networking, USB devices macOS holds): is it there, set it up, take it
//! away.
//!
//! Setting up asks for an admin password with macOS's own prompt
//! (`osascript ... with administrator privileges`) and runs one short
//! script as root: copy vroom-netd and a copy of libusb into a root-owned
//! directory, write its config and launchd file, start it. The app itself
//! never runs as root, and neither does a VM.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use vroom_net::netd::{self, NetdError, Status};

pub const LABEL: &str = "org.vroom.netd";
pub const PLIST: &str = "/Library/LaunchDaemons/org.vroom.netd.plist";
pub const DIR: &str = "/usr/local/libexec/vroom";
pub const CONF: &str = "/etc/vroom/netd.toml";
/// The launchd file and the config it starts with: the same as in
/// `packaging/`, for people who install by hand.
const PLIST_TEXT: &str = include_str!("../../../packaging/org.vroom.netd.plist");
const CONF_TEXT: &str = include_str!("../../../packaging/netd.toml");
/// Added to a config from before USB devices were served.
const USB_SECTION: &str = "
# USB devices macOS holds (keyboards, disks, webcams, serial chips): the
# helper takes them from macOS for a VM. Only members of this group may ask.
[usb]
enabled = true
group = \"admin\"
";

/// What the app knows about the helper.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Helper {
    /// Not checked yet.
    #[default]
    Unknown,
    Ready(Status),
    /// Installed, but from before USB devices (it doesn't know the
    /// status question).
    Old,
    /// Installed, not answering.
    Stopped,
    Missing,
}

impl Helper {
    /// For the settings page: "ready", "old", "stopped", "missing" or "".
    pub fn word(&self) -> &'static str {
        match self {
            Helper::Unknown => "",
            Helper::Ready(_) => "ready",
            Helper::Old => "old",
            Helper::Stopped => "stopped",
            Helper::Missing => "missing",
        }
    }

    /// May this user have USB devices macOS holds? Err: why not.
    pub fn usb(&self) -> Result<(), String> {
        match self {
            Helper::Ready(s) if s.usb => Ok(()),
            Helper::Ready(s) => Err(s.usb_why.clone()),
            Helper::Old => Err("vroom-netd is from an older vroom: set it up again".into()),
            _ => Err("vroom-netd isn't set up".into()),
        }
    }
}

/// Ask the helper (blocks: a local socket, a few seconds at most).
pub fn check() -> Helper {
    match netd::status(Path::new(netd::DEFAULT_SOCKET)) {
        Ok(s) => Helper::Ready(s),
        // An older netd refuses the question.
        Err(NetdError::Refused(_) | NetdError::Proto(_)) => Helper::Old,
        Err(NetdError::Connect(..)) if Path::new(PLIST).exists() => Helper::Stopped,
        Err(NetdError::Connect(..)) => Helper::Missing,
    }
}

struct Cache {
    helper: Helper,
    at: Option<Instant>,
    checking: bool,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache { helper: Helper::Unknown, at: None, checking: false });
const STALE: Duration = Duration::from_secs(10);

fn cache() -> std::sync::MutexGuard<'static, Cache> {
    CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

/// What we last knew; checks again in the background when that is old.
pub fn state() -> Helper {
    let mut c = cache();
    if !c.checking && c.at.is_none_or(|t| t.elapsed() > STALE) {
        c.checking = true;
        std::thread::spawn(|| {
            let h = check();
            let mut c = cache();
            (c.helper, c.at, c.checking) = (h, Some(Instant::now()), false);
        });
    }
    c.helper.clone()
}

/// Check now, and remember it.
pub fn state_now() -> Helper {
    let h = check();
    let mut c = cache();
    (c.helper, c.at) = (h.clone(), Some(Instant::now()));
    h
}

/// What gets installed: vroom-netd from next to the app, libusb from
/// Homebrew (the real file, not its link).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sources {
    pub netd: PathBuf,
    pub libusb: Option<PathBuf>,
}

pub fn sources() -> Result<Sources, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let netd = exe.with_file_name("vroom-netd");
    if !netd.is_file() {
        return Err(format!("{} is missing (scripts/build.sh builds it)", netd.display()));
    }
    let libusb = ["/opt/homebrew/lib/libusb-1.0.0.dylib", "/usr/local/lib/libusb-1.0.0.dylib"]
        .iter()
        .find_map(|p| std::fs::canonicalize(p).ok().filter(|p| p.is_file()));
    Ok(Sources { netd, libusb })
}

/// `s` as one sh word.
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn path_word(p: &Path) -> Result<String, String> {
    p.to_str().map(sq).ok_or_else(|| format!("{} isn't a plain path", p.display()))
}

/// The script that installs and starts the helper, run as root.
pub fn install_script(src: &Sources) -> Result<String, String> {
    let mut s = String::from("set -e\n");
    // A root daemon must sit where only root can change it: nothing above
    // it may be someone else's or writable by others.
    s += "for p in /usr/local /usr/local/libexec; do\n";
    s += "  if [ -e \"$p\" ]; then\n";
    s += "    [ -d \"$p\" ] && [ ! -L \"$p\" ] && [ \"$(stat -f %u \"$p\")\" = 0 ] && [ $(( 0$(stat -f %Lp \"$p\") & 022 )) = 0 ] || { echo \"$p isn't owned by root only: vroom-netd can't go under it\" >&2; exit 3; }\n";
    s += "  else mkdir -m 755 \"$p\"; chown root:wheel \"$p\"; fi\n";
    s += "done\n";
    s += &format!("D={}\n", sq(DIR));
    s += "rm -rf \"$D.new\"; mkdir -m 755 \"$D.new\"; chown root:wheel \"$D.new\"\n";
    s += &format!("install -o root -g wheel -m 755 {} \"$D.new/vroom-netd\"\n", path_word(&src.netd)?);
    if let Some(l) = &src.libusb {
        s += &format!("install -o root -g wheel -m 644 {} \"$D.new/libusb-1.0.0.dylib\"\n", path_word(l)?);
    }
    s += "rm -rf \"$D.old\"; [ -e \"$D\" ] && mv \"$D\" \"$D.old\"; mv \"$D.new\" \"$D\"; rm -rf \"$D.old\"\n";
    s += "mkdir -p /etc/vroom; chown root:wheel /etc/vroom; chmod 755 /etc/vroom\n";
    s += &format!("[ -f {c} ] || printf '%s' {} > {c}\n", sq(CONF_TEXT), c = sq(CONF));
    s += &format!("grep -q '^\\[usb\\]' {c} || printf '%s' {} >> {c}\n", sq(USB_SECTION), c = sq(CONF));
    s += &format!("chown root:wheel {c}; chmod 644 {c}\n", c = sq(CONF));
    s += &format!("printf '%s' {} > {p}; chown root:wheel {p}; chmod 644 {p}\n", sq(PLIST_TEXT), p = sq(PLIST));
    s += &format!("launchctl bootout system/{LABEL} 2>/dev/null || true\n");
    s += &format!("for i in 1 2 3 4 5 6 7 8 9 10; do launchctl print system/{LABEL} >/dev/null 2>&1 || break; sleep 0.5; done\n");
    s += &format!("launchctl enable system/{LABEL}\n");
    s += &format!("launchctl bootstrap system {}\n", sq(PLIST));
    s += &format!("for i in 1 2 3 4 5 6 7 8 9 10; do [ -S {} ] && break; sleep 0.5; done\n", sq(netd::DEFAULT_SOCKET));
    Ok(s)
}

/// The script that stops and removes it.
pub fn uninstall_script() -> String {
    let mut s = String::new();
    s += &format!("launchctl bootout system/{LABEL} 2>/dev/null || true\n");
    s += &format!("rm -f {}\n", sq(PLIST));
    s += &format!("rm -rf {}\n", sq(DIR));
    // Where the hand-install instructions used to put it.
    s += "rm -f /usr/local/libexec/vroom-netd\n";
    s += &format!("rm -f {}; rmdir /var/run/vroom 2>/dev/null || true\n", sq(netd::DEFAULT_SOCKET));
    s += &format!("rm -f {}; rmdir /etc/vroom 2>/dev/null || true\n", sq(CONF));
    s
}

/// Run `script` as root, after macOS's password prompt.
fn as_admin(script: &str, why: &str) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("on Linux, install vroom-netd by hand: see docs/GUIDE.md".into());
    }
    let out = std::process::Command::new("/usr/bin/osascript")
        .args(["-e", "on run argv", "-e", "do shell script (item 1 of argv) with prompt (item 2 of argv) with administrator privileges", "-e", "end run"])
        .arg(script)
        .arg(why)
        .output()
        .map_err(|e| format!("osascript: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    if err.contains("-128") {
        return Err("cancelled".into());
    }
    // "execution error: <what the script said> (1)"
    let msg = err.trim().trim_start_matches("0:").trim();
    let msg = msg.split_once("execution error: ").map_or(msg, |(_, m)| m);
    Err(msg.rsplit_once(" (").map_or(msg, |(m, _)| m).chars().take(300).collect())
}

/// Install and start the helper. Blocks while the password prompt is up.
pub fn install() -> Result<(), String> {
    let src = sources()?;
    as_admin(&install_script(&src)?, "vroom wants to set up its helper (vroom-netd) for bridged networking and USB devices.")?;
    match state_now() {
        Helper::Ready(_) => Ok(()),
        other => Err(format!("set up, but it doesn't answer yet ({}): see /var/log/vroom-netd.log", other.word())),
    }
}

pub fn uninstall() -> Result<(), String> {
    as_admin(&uninstall_script(), "vroom wants to remove its helper (vroom-netd).")?;
    state_now();
    Ok(())
}

static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Setting up or removing is underway (the password prompt may be up).
pub fn busy() -> bool {
    BUSY.load(std::sync::atomic::Ordering::Acquire)
}

/// Run `f` (install or uninstall) on a thread, one at a time; `done`
/// gets its result on the UI thread.
pub fn in_background(f: fn() -> Result<(), String>, done: impl FnOnce(Result<(), String>) + Send + 'static) {
    if BUSY.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(move || {
        let r = f();
        BUSY.store(false, std::sync::atomic::Ordering::Release);
        let _ = slint::invoke_from_event_loop(move || done(r));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(sq("a b"), "'a b'");
        assert_eq!(sq("it's"), "'it'\\''s'");
        let out = std::process::Command::new("/bin/sh").arg("-c").arg(format!("printf '%s' {}", sq("x'\"$HOME`id`\\"))).output().unwrap();
        assert_eq!(out.stdout, b"x'\"$HOME`id`\\");
    }

    #[test]
    fn the_scripts() {
        let src = Sources { netd: "/Users/a b/vroom/target/release/vroom-netd".into(), libusb: Some("/opt/homebrew/Cellar/libusb/1.0.30/lib/libusb-1.0.0.dylib".into()) };
        let s = install_script(&src).unwrap();
        assert!(s.starts_with("set -e\n"));
        assert!(s.contains("install -o root -g wheel -m 755 '/Users/a b/vroom/target/release/vroom-netd' \"$D.new/vroom-netd\""));
        assert!(s.contains("libusb-1.0.0.dylib"));
        assert!(s.contains("launchctl bootstrap system '/Library/LaunchDaemons/org.vroom.netd.plist'"));
        // The config is written only if there is none; [usb] added once.
        assert!(s.contains("[ -f '/etc/vroom/netd.toml' ] || printf"));
        assert!(s.contains("grep -q '^\\[usb\\]'"));
        // It parses as sh.
        let out = std::process::Command::new("/bin/sh").arg("-n").arg("-c").arg(&s).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let out = std::process::Command::new("/bin/sh").arg("-n").arg("-c").arg(uninstall_script()).output().unwrap();
        assert!(out.status.success());
        assert!(install_script(&Sources { netd: PathBuf::from(std::ffi::OsString::from_vec(vec![0xff])), libusb: None }).is_err());
    }

    #[test]
    fn packaged_files_agree() {
        // The config the app writes is one netd takes, with USB on, and
        // the launchd file starts the binary where the script puts it.
        assert!(CONF_TEXT.contains("[usb]") && CONF_TEXT.contains("enabled = true"));
        assert!(PLIST_TEXT.contains(&format!("<string>{DIR}/vroom-netd</string>")));
        assert!(PLIST_TEXT.contains(LABEL));
        assert!(CONF_TEXT.contains(netd::DEFAULT_SOCKET));
    }

    use std::os::unix::ffi::OsStringExt;
}

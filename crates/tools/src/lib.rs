//! Host side of the guest tools (the agent and what installs it).
//!
//! `scripts/make-guest-tools.sh` builds them into a folder:
//! ```text
//! guest-tools/
//!   linux/      vroom-agent, install-agent.sh, its systemd units, the
//!               GNOME Shell extension: what a Linux guest installs
//!   windows/    vroom-agent.exe, vroom-setup.cmd, the virtio-win drivers
//!               (ARM64), the OpenGL driver if built
//!   vroom-tools.img   both, on a FAT32 disk image (MBR) labelled VRTOOLS:
//!               for guests without an agent (a USB stick, or a disk)
//! ```
//! vroom.app carries it in Contents/Resources; a build from source finds
//! it in target/.
//!
//! Here: finding that folder, the version of the agent in it, the update
//! a running agent takes (`bundle`), and ready-made VMs (`ready`).
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

pub use vroom_proto::version::Version;

pub mod bundle;
pub mod ready;
pub mod seed;
pub mod winstick;

/// The guest systems the tools are for, by their folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    Windows,
}

impl Os {
    pub fn dir_name(self) -> &'static str {
        match self {
            Os::Linux => "linux",
            Os::Windows => "windows",
        }
    }

    pub fn agent_file(self) -> &'static str {
        match self {
            Os::Linux => "vroom-agent",
            Os::Windows => "vroom-agent.exe",
        }
    }

    /// From an agent's hello ("windows", "linux").
    pub fn from_name(s: &str) -> Option<Os> {
        match s {
            "linux" => Some(Os::Linux),
            "windows" => Some(Os::Windows),
            _ => None,
        }
    }
}

/// The guest tools folder: `VROOM_GUEST_TOOLS`, else next to the running
/// program (vroom.app's Resources, or a build's target/ folder).
pub fn dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("VROOM_GUEST_TOOLS") {
        return Some(PathBuf::from(d)).filter(|d| looks_right(d));
    }
    let exe = std::env::current_exe().ok()?;
    let here = exe.parent()?;
    [here.join("../Resources/guest-tools"), here.join("guest-tools"), here.join("../guest-tools")]
        .into_iter()
        .find(|d| looks_right(d))
        .map(|d| d.canonicalize().unwrap_or(d))
}

fn looks_right(d: &Path) -> bool {
    d.join("linux").is_dir() || d.join("windows").is_dir() || d.join(IMAGE).is_file()
}

/// The disk image holding both systems' tools.
pub const IMAGE: &str = "vroom-tools.img";
/// Its volume label.
pub const LABEL: &str = "VRTOOLS";

/// The folder of one system's tools, if they were built.
pub fn os_dir(os: Os) -> Option<PathBuf> {
    let d = dir()?.join(os.dir_name());
    d.join(os.agent_file()).is_file().then_some(d)
}

/// The tools image, if it was built.
pub fn image() -> Option<PathBuf> {
    dir().map(|d| d.join(IMAGE)).filter(|p| p.is_file())
}

/// The version of the agent in a file (build.rs puts it in, after
/// `\0VROOM_AGENT_VERSION=`).
pub fn agent_version(file: &Path) -> Option<Version> {
    const TAG: &[u8] = b"\0VROOM_AGENT_VERSION=";
    let b = std::fs::read(file).ok()?;
    let at = b.windows(TAG.len()).position(|w| w == TAG)? + TAG.len();
    let end = b[at..].iter().take(80).position(|&c| c == 0)? + at;
    Version::parse(std::str::from_utf8(&b[at..end]).ok()?)
}

/// The version of the tools for `os`, if they were built.
pub fn version(os: Os) -> Option<Version> {
    agent_version(&os_dir(os)?.join(os.agent_file()))
}

/// What a running guest's agent should get, from its hello text and
/// features: `Some(true)` an update it can install itself, `Some(false)`
/// newer tools it has to be given by hand (too old to take updates),
/// `None` nothing newer here.
pub fn update_for(agent: &str, features: u64, tools: &Version) -> Option<bool> {
    let theirs = Version::from_hello(agent)?;
    theirs.older_than(tools).then_some(features & vroom_proto::features::UPDATE != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_found_in_the_binary() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("vroom-agent");
        let mut b = vec![7u8; 1000];
        b.extend_from_slice(b"\0VROOM_AGENT_VERSION=0.1.0+12.abcdef\0more");
        std::fs::write(&p, &b).unwrap();
        assert_eq!(agent_version(&p), Version::parse("0.1.0+12.abcdef"));
        std::fs::write(&p, b"no tag here").unwrap();
        assert_eq!(agent_version(&p), None);
    }

    #[test]
    fn who_gets_an_update() {
        let tools = Version::parse("0.1.0+20.ab").unwrap();
        let up = vroom_proto::features::UPDATE;
        assert_eq!(update_for("vroom-agent 0.1.0+19.cd linux", up, &tools), Some(true));
        assert_eq!(update_for("vroom-agent 0.1.0", 0, &tools), Some(false), "too old to update itself");
        assert_eq!(update_for("vroom-agent 0.1.0+20.ef linux", up, &tools), None);
        assert_eq!(update_for("vroom-agent 0.1.0+21.ef linux", up, &tools), None, "newer than ours");
        assert_eq!(update_for("someone else", up, &tools), None);
    }
}

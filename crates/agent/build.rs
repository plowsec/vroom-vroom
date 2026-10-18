//! The agent's version: the crate's, plus the commit it was built from, as
//! `0.1.0+348.e314084680` (commits on the branch, then the hash). The host
//! compares versions by the crate version, then the commit count
//! (`vroom_proto::version`). `VROOM_AGENT_VERSION` overrides it (builds
//! without git).

use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let o = Command::new("git").args(args).output().ok()?;
    if !o.status.success() {
        return None;
    }
    let s = String::from_utf8(o.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn main() {
    println!("cargo:rerun-if-env-changed=VROOM_AGENT_VERSION");
    let crate_ver = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let ver = match std::env::var("VROOM_AGENT_VERSION") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => match (git(&["rev-list", "--count", "HEAD"]), git(&["rev-parse", "--short=10", "HEAD"])) {
            (Some(n), Some(h)) => format!("{crate_ver}+{n}.{h}"),
            _ => crate_ver,
        },
    };
    println!("cargo:rustc-env=VROOM_AGENT_VERSION={ver}");
    // A new commit changes the version: watch HEAD and the branch it
    // names. Only files that exist (cargo reruns every time otherwise).
    let mut watch = vec![git(&["rev-parse", "--git-path", "HEAD"]), git(&["rev-parse", "--git-path", "packed-refs"])];
    if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watch.push(git(&["rev-parse", "--git-path", &r]));
    }
    for p in watch.into_iter().flatten() {
        if Path::new(&p).exists() {
            println!("cargo:rerun-if-changed={p}");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}

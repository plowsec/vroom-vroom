//! One-CPU native Linux harness. No firmware, disks, network or RPC server.
#![forbid(unsafe_code)]

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux;

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
mod linux_arm64;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn main() {
    linux::main();
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn main() {
    linux_arm64::main();
}

#[cfg(not(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))))]
fn main() {
    println!("{{\"status\":\"unknown\",\"scope\":\"native_linux_harness\",\"error\":\"requires Linux x86_64 or aarch64 with KVM; no guest was run\"}}");
    std::process::exit(2);
}

fn main() {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if os == "macos" && arch == "aarch64" {
        println!("cargo:rerun-if-changed=csrc/hvf_shim.c");
        cc::Build::new().file("csrc/hvf_shim.c").warnings(true).compile("bw_hvf_shim");
    }
}

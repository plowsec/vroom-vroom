fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rerun-if-changed=csrc/vmnet_shim.c");
        cc::Build::new().file("csrc/vmnet_shim.c").flag("-fblocks").compile("bw_vmnet");
        println!("cargo:rustc-link-lib=framework=vmnet");
    }
}

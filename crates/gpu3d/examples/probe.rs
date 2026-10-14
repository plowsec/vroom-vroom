//! Can this host do Venus? Loads virglrenderer, prints capsets, makes a
//! context.
use bulwark_virtio::devices::gpu3d::Renderer3d;

fn main() {
    env_logger_lite();
    if let Some(i) = std::env::args().position(|a| a == "--helper-profile") {
        let a: Vec<String> = std::env::args().collect();
        print!("{}", bulwark_sandbox::helper_profile(a[i + 1].as_ref(), a[i + 2].as_ref()).expect("profile"));
        return;
    }
    if std::env::args().any(|a| a == "--print-profile") {
        let p = bulwark_sandbox::Policy { gpu: true, ..Default::default() };
        print!("{}", bulwark_sandbox::profile_text(&p).expect("profile"));
        return;
    }
    if !bulwark_gpu3d::load() {
        eprintln!("virglrenderer not found");
        std::process::exit(1);
    }
    let Some(mut r) = bulwark_gpu3d::Virgl::new() else {
        eprintln!("init failed");
        std::process::exit(1);
    };
    // Same order as the VMM: load + init, then lock down, then use.
    if std::env::args().any(|a| a == "--sandbox") {
        let p = bulwark_sandbox::Policy { gpu: true, ..Default::default() };
        bulwark_sandbox::apply(&p).expect("sandbox");
        eprintln!("sandboxed");
    }
    for (id, ver, size) in r.capsets() {
        let caps = r.capset(id, ver).unwrap_or_default();
        println!("capset {id} v{ver} {size} bytes, head {:02x?}", &caps[..caps.len().min(16)]);
    }
    println!("ctx create (venus): {}", r.context_create(1, 4, b"probe"));
    r.context_destroy(1);
}

fn env_logger_lite() {}

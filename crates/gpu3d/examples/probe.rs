//! Can this host do Venus? Loads virglrenderer, prints capsets, makes a
//! context.
use bulwark_virtio::devices::gpu3d::Renderer3d;

fn main() {
    env_logger_lite();
    if !bulwark_gpu3d::load() {
        eprintln!("virglrenderer not found");
        std::process::exit(1);
    }
    let Some(mut r) = bulwark_gpu3d::Virgl::new() else {
        eprintln!("init failed");
        std::process::exit(1);
    };
    for (id, ver, size) in r.capsets() {
        let caps = r.capset(id, ver).unwrap_or_default();
        println!("capset {id} v{ver} {size} bytes, head {:02x?}", &caps[..caps.len().min(16)]);
    }
    println!("ctx create (venus): {}", r.context_create(1, 4, b"probe"));
    r.context_destroy(1);
}

fn env_logger_lite() {}

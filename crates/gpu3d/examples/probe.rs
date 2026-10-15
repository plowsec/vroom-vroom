//! Can this host do Venus? Loads virglrenderer, prints capsets, makes a
//! context.
use vroom_virtio::devices::gpu3d::Renderer3d;

fn main() {
    env_logger_lite();
    if let Some(i) = std::env::args().position(|a| a == "--helper-profile") {
        let a: Vec<String> = std::env::args().collect();
        print!("{}", vroom_sandbox::helper_profile(a[i + 1].as_ref(), a[i + 2].as_ref()).expect("profile"));
        return;
    }
    if std::env::args().any(|a| a == "--print-profile") {
        let p = vroom_sandbox::Policy { gpu: true, ..Default::default() };
        print!("{}", vroom_sandbox::profile_text(&p).expect("profile"));
        return;
    }
    // `--apple`: Apple's OpenGL instead of ANGLE.
    let host = if std::env::args().any(|a| a == "--apple") { vroom_gpu3d::GlHost::Apple } else { vroom_gpu3d::GlHost::Angle };
    if !vroom_gpu3d::load(Some(host)) {
        eprintln!("virglrenderer not found");
        std::process::exit(1);
    }
    let Some(mut r) = vroom_gpu3d::Virgl::new(None) else {
        eprintln!("init failed");
        std::process::exit(1);
    };
    // Same order as the VMM: load + init, then lock down, then use.
    if std::env::args().any(|a| a == "--sandbox") {
        let p = vroom_sandbox::Policy { gpu: true, ..Default::default() };
        vroom_sandbox::apply(&p).expect("sandbox");
        eprintln!("sandboxed");
    }
    for (id, ver, size) in r.capsets() {
        let caps = r.capset(id, ver).unwrap_or_default();
        println!("capset {id} v{ver} {size} bytes, head {:02x?}", &caps[..caps.len().min(16)]);
    }
    println!("ctx create (venus): {}", r.context_create(1, 4, b"probe"));
    r.context_destroy(1);
    if let Some(caps) = r.capset(2, 2) {
        let name = caps.get(696..760).unwrap_or_default();
        let name = String::from_utf8_lossy(name.split(|&b| b == 0).next().unwrap_or_default()).into_owned();
        println!("gl renderer: {name}");
        println!("ctx create (virgl): {}", r.context_create(2, 2, b"probe-gl"));
        r.context_destroy(2);
    }
}

fn env_logger_lite() {}

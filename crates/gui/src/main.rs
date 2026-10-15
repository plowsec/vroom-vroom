//! vroom-gui: the app.
//!
//! `vroom-gui` opens the VM library. `vroom-gui vm.toml [run args]` opens
//! just that VM's window (the library is one click away in its toolbar).

mod app;
mod console;
mod edit;
mod fmt;
mod host;
mod keymap;
mod library;
mod msg;
mod paths;
mod shot;
mod vm;

#[allow(missing_debug_implementations)]
mod ui {
    slint::include_modules!();
}
pub use ui::*;

use vroom_config::GuestOs;

/// The OS name the UI's icons and colors know.
pub fn os_id(os: GuestOs) -> &'static str {
    match os {
        GuestOs::Windows => "windows",
        GuestOs::Ubuntu => "ubuntu",
        GuestOs::Debian => "debian",
        GuestOs::Fedora => "fedora",
        GuestOs::Alpine => "alpine",
        GuestOs::Linux => "linux",
        GuestOs::Other => "other",
    }
}

/// Messages with no library window to handle them (single-VM mode).
pub fn fallback(m: msg::Msg) {
    use msg::Msg;
    match m {
        Msg::ShowLibrary => {
            if let Err(e) = app::launch() {
                eprintln!("vroom-gui: {e}");
            }
        }
        Msg::ConsoleGone { error, .. } => {
            if let Some(e) = error {
                eprintln!("vroom-gui: {e}");
            }
            if console::count() == 0 {
                let _ = slint::quit_event_loop();
            }
        }
        Msg::Toast { text, .. } => eprintln!("vroom-gui: {text}"),
        _ => {}
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    if let Err(e) = slint::BackendSelector::new().backend_name("winit".into()).select() {
        eprintln!("vroom-gui: {e}");
        std::process::exit(1);
    }
    let mut args = std::env::args().skip(1);
    let r = match args.next() {
        None => app::launch(),
        Some(a) if a == "-h" || a == "--help" => {
            println!("usage: vroom-gui                                  the VM library");
            println!("       vroom-gui <vm.toml> [args for `vroom run`]  one VM's window");
            return;
        }
        Some(config) => {
            let extra: Vec<String> = args.collect();
            let p = std::path::PathBuf::from(config);
            vroom_config::VmConfig::load(&p).map_err(|e| e.to_string()).and_then(|cfg| console::start(&p, &cfg, &extra).map(|_| ()))
        }
    };
    if let Err(e) = r {
        eprintln!("vroom-gui: {e}");
        std::process::exit(1);
    }
    if let Some(shot) = std::env::var_os("VROOM_SHOT") {
        shot::schedule(std::path::PathBuf::from(shot));
    }
    if let Err(e) = slint::run_event_loop_until_quit() {
        eprintln!("vroom-gui: {e}");
    }
    // Quitting with VMs open: they suspend (they also do on their own
    // when this process goes away).
    console::suspend_all();
}

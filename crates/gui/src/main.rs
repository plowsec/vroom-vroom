//! vroom-gui: the app.
//!
//! `vroom-gui` opens the VM library. `vroom-gui vm.toml [run args]` opens
//! just that VM's window (the library is one click away in its toolbar).

mod app;
mod console;
mod dnd;
mod dragout;
#[cfg(target_os = "macos")]
mod dragout_mac;
mod edit;
mod fmt;
mod host;
mod hostusb;
mod keymap;
mod library;
mod mac;
mod msg;
mod paths;
mod rootd;
mod shot;
mod vm;

#[allow(missing_debug_implementations, clippy::all, clippy::unwrap_used, clippy::undocumented_unsafe_blocks)]
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
        Msg::ShowLibrary(key) => match app::launch() {
            // The library handles messages now: let it pick the VM.
            Ok(()) => msg::send(Msg::ShowLibrary(key)),
            Err(e) => eprintln!("vroom-gui: {e}"),
        },
        Msg::ConsoleGone { error, .. } => {
            if let Some(e) = error {
                eprintln!("vroom-gui: {e}");
            }
            if console::count() == 0 {
                let _ = slint::quit_event_loop();
            }
        }
        Msg::Open(p) => match app::launch() {
            Ok(()) => msg::send(Msg::Open(p)),
            Err(e) => eprintln!("vroom-gui: {e}"),
        },
        Msg::Toast { text, .. } => eprintln!("vroom-gui: {text}"),
        _ => {}
    }
}

fn main() {
    mac::log_to_file_if_detached();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    if let Err(e) = slint::BackendSelector::new().backend_name("winit".into()).select() {
        eprintln!("vroom-gui: {e}");
        std::process::exit(1);
    }
    let mut args = std::env::args().skip(1).filter(|a| !mac::is_finder_arg(a));
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
            // The same name the library uses for it, and in the library:
            // a VM opened by its file shows in the list, and the window's
            // settings button finds its page.
            let p = std::fs::canonicalize(&p).unwrap_or(p);
            let mut lib = library::Library::load(&paths::library_file());
            if lib.add(&p) {
                if let Err(e) = lib.save() {
                    eprintln!("vroom-gui: library: {e}");
                }
            }
            vroom_config::VmConfig::load(&p).map_err(|e| e.to_string()).and_then(|cfg| {
                // Running without a window (the app crashed): show it again.
                let dir = cfg.default_state_dir(&p);
                match console::viewer_socket(&dir) {
                    Some(sock) if vroom_state::StateLock::in_use(&dir) => {
                        let pid = vroom_state::StateLock::holder(&dir).unwrap_or(0);
                        console::attach(&p, &cfg, &sock, pid).map(|_| ())
                    }
                    _ => console::start(&p, &cfg, &extra).map(|_| ()),
                }
            })
        }
    };
    if let Err(e) = r {
        eprintln!("vroom-gui: {e}");
        std::process::exit(1);
    }
    // Files opened from Finder and the Dock icon.
    mac::install();
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

/// The controls show their owner's value, whatever the user did with them
/// before (one settings page serves every VM).
#[cfg(test)]
#[allow(clippy::all, clippy::unwrap_used, missing_debug_implementations)]
mod controls_tests {
    use slint::platform::{PointerEventButton, WindowEvent};
    use slint::{ComponentHandle, LogicalPosition};

    slint::slint! {
        import { Field, Toggle, Slider } from "ui/controls.slint";
        export component Controls inherits Window {
            width: 400px;
            height: 200px;
            in property <string> value: "1024";
            in property <bool> on;
            in property <float> amount: 4;
            out property <string> last-edit;
            out property <int> toggled;
            out property <bool> toggle-shows: t.checked;
            out property <float> slider-shows: sl.shown;
            public function focus-field() { f.focus-input(); }
            f := Field {
                x: 0;
                y: 0;
                width: 200px;
                text: root.value;
                edited(s) => { root.last-edit = s; }
            }
            t := Toggle {
                x: 0;
                y: 50px;
                checked: root.on;
                toggled(v) => { root.toggled += 1; }
            }
            sl := Slider {
                x: 0;
                y: 100px;
                width: 200px;
                minimum: 0;
                maximum: 10;
                value: root.amount;
            }
        }
    }

    fn click(w: &Controls, x: f32, y: f32) {
        let p = LogicalPosition::new(x, y);
        let win = w.window();
        win.dispatch_event(WindowEvent::PointerMoved { position: p });
        win.dispatch_event(WindowEvent::PointerPressed { position: p, button: PointerEventButton::Left });
        win.dispatch_event(WindowEvent::PointerReleased { position: p, button: PointerEventButton::Left });
    }

    /// Type a char; what the box holds then, without it.
    fn typed(w: &Controls, c: &str) -> String {
        w.window().dispatch_event(WindowEvent::KeyPressed { text: c.into() });
        w.window().dispatch_event(WindowEvent::KeyReleased { text: c.into() });
        slint::platform::update_timers_and_animations();
        w.get_last_edit().replacen(c, "", 1)
    }

    #[test]
    fn controls_follow_their_owner() {
        i_slint_backend_testing::init_no_event_loop();
        let w = Controls::new().unwrap();
        w.show().unwrap();

        w.invoke_focus_field();
        assert_eq!(typed(&w, "9"), "1024");
        // The owner's value changes (another VM): the box shows it, not
        // what was typed for the old one.
        w.set_value("800".into());
        slint::platform::update_timers_and_animations();
        assert_eq!(typed(&w, "7"), "800");

        click(&w, 10.0, 60.0);
        assert_eq!(w.get_toggled(), 1);
        assert!(!w.get_toggle_shows(), "flipped itself instead of asking");
        w.set_on(true);
        assert!(w.get_toggle_shows());
        click(&w, 10.0, 60.0);
        w.set_on(false);
        assert!(!w.get_toggle_shows());

        // Drag the knob: it follows the pointer, then the owner's value.
        let win = w.window();
        win.dispatch_event(WindowEvent::PointerMoved { position: LogicalPosition::new(190.0, 110.0) });
        win.dispatch_event(WindowEvent::PointerPressed { position: LogicalPosition::new(190.0, 110.0), button: PointerEventButton::Left });
        assert_eq!(w.get_slider_shows(), 10.0);
        win.dispatch_event(WindowEvent::PointerReleased { position: LogicalPosition::new(190.0, 110.0), button: PointerEventButton::Left });
        w.set_amount(2.0);
        assert_eq!(w.get_slider_shows(), 2.0);
    }
}

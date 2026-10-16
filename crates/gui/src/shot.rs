//! `VROOM_SHOT=out.png vroom-gui`: render the library window to a PNG and
//! quit. `VROOM_SHOT_DO="act;act"` runs UI actions first ("newvm",
//! "palette", "search:net", "select:1", "set:cpus=2", "wait:30" seconds,
//! "console:suspend" to a VM window, "monshot:2:out.png" a picture of a
//! VM's second monitor window, "vmkey:57" a key to the guest (evdev
//! code; "vmkey:125+19" a combination), "vmtype:notepad" text,
//! "cohclick:X,Y[,title]" a click on a Coherence window, "vmmove:X,Y"
//! the mouse to a VM window's point, "shot:out.png" a
//! picture now, "barshot:out.png" one of the Coherence bar, "cohstart:Calculator" a
//! name in its app field). `VROOM_SHOT_VM=1`: picture the VM window instead. Used
//! for the README pictures and
//! to check the UI without a person at the screen.

use std::path::PathBuf;
use std::time::Duration;

use slint::ComponentHandle;

use crate::msg::{self, Msg};

thread_local! {
    pub static TARGET: std::cell::RefCell<Option<crate::AppWindow>> = const { std::cell::RefCell::new(None) };
}

pub fn schedule(out: PathBuf) {
    let acts = std::env::var("VROOM_SHOT_DO").unwrap_or_default();
    // Seconds to wait before the picture (a VM window needs its guest up).
    let delay = std::env::var("VROOM_SHOT_DELAY").ok().and_then(|d| d.parse::<f64>().ok()).unwrap_or(1.4);
    let acts: Vec<String> = acts.split(';').map(str::trim).filter(|a| !a.is_empty()).map(String::from).collect();
    slint::Timer::single_shot(Duration::from_millis(600), move || run(acts, 0, out, delay));
}

fn picture(out: &std::path::Path) {
    let library = || TARGET.with(|t| t.borrow().as_ref().map(|ui| ui.window().take_snapshot()));
    let r = if std::env::var_os("VROOM_SHOT_VM").is_some() {
        crate::console::snapshot_any().or_else(library)
    } else {
        library().or_else(crate::console::snapshot_any)
    };
    match r {
        Some(Ok(buf)) => {
            let (w, h) = (buf.width(), buf.height());
            if let Err(e) = crate::console::write_png(out, &buf, w, h) {
                eprintln!("shot: {e}");
            }
        }
        Some(Err(e)) => eprintln!("shot: {e}"),
        None => eprintln!("shot: no window"),
    }
}

/// Actions from `i` on, then the picture; `wait:N` pauses the list.
fn run(acts: Vec<String>, i: usize, out: PathBuf, delay: f64) {
    for (n, a) in acts.iter().enumerate().skip(i) {
        let a = a.as_str();
        if let Some(secs) = a.strip_prefix("wait:").and_then(|s| s.parse::<f64>().ok()) {
            slint::Timer::single_shot(Duration::from_secs_f64(secs.max(0.0)), move || run(acts, n + 1, out, delay));
            return;
        } else if let Some(c) = a.strip_prefix("console:") {
            crate::console::command_any(c);
        } else if let Some(k) = a.strip_prefix("vmkey:") {
            let codes: Vec<u16> = k.split('+').filter_map(|c| c.parse().ok()).collect();
            crate::console::keys_any(&codes);
        } else if let Some(p) = a.strip_prefix("cohclick:") {
            // cohclick:X,Y[,title]
            let v: Vec<&str> = p.splitn(3, ',').collect();
            let n = |i: usize| v.get(i).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            crate::console::coh_click_any(n(0), n(1), v.get(2).copied().unwrap_or(""));
        } else if let Some(t) = a.strip_prefix("vmtype:") {
            crate::console::type_any(t);
        } else if let Some((x, y)) = a.strip_prefix("vmmove:").and_then(|p| p.split_once(',')) {
            crate::console::move_any(x.parse().unwrap_or(0.0), y.parse().unwrap_or(0.0));
        } else if let Some(p) = a.strip_prefix("shot:") {
            picture(&PathBuf::from(p));
        } else if let Some((n, p)) = a.strip_prefix("monshot:").and_then(|s| s.split_once(':')) {
            // Another monitor's window (2, 3...).
            match crate::console::snapshot_monitor(n.parse().unwrap_or(0)) {
                Some(Ok(buf)) => {
                    if let Err(e) = crate::console::write_png(std::path::Path::new(p), &buf, buf.width(), buf.height()) {
                        eprintln!("shot: {e}");
                    }
                }
                _ => eprintln!("shot: no window for monitor {n}"),
            }
        } else if let Some(n) = a.strip_prefix("cohstart:") {
            crate::console::coh_start_any(n);
        } else if let Some(p) = a.strip_prefix("barshot:") {
            if let Some(Ok(buf)) = crate::console::snapshot_bar() {
                let _ = crate::console::write_png(std::path::Path::new(p), &buf, buf.width(), buf.height());
            }
        } else if let Some(i) = a.strip_prefix("select:") {
            msg::send(Msg::Select(i.parse().unwrap_or(0)));
        } else if let Some((k, v)) = a.strip_prefix("set:").and_then(|kv| kv.split_once('=')) {
            msg::send(Msg::Set(k.to_string(), v.to_string()));
        } else if let Some(p) = a.strip_prefix("drop:") {
            msg::send(Msg::Drop(PathBuf::from(p)));
        } else if let Some(q) = a.strip_prefix("type:") {
            msg::send(Msg::Search(format!("palette:{q}")));
        } else {
            msg::send(Msg::Act(a.to_string()));
        }
    }
    slint::Timer::single_shot(Duration::from_secs_f64(delay), move || {
        picture(&out);
        if std::env::var_os("VROOM_SHOT_KEEP").is_none() {
            crate::console::suspend_all();
            let _ = slint::quit_event_loop();
        }
    });
}

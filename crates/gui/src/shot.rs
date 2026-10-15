//! `VROOM_SHOT=out.png vroom-gui`: render the library window to a PNG and
//! quit. `VROOM_SHOT_DO="act;act"` runs UI actions first ("newvm",
//! "palette", "search:net", "select:1"). Used for the README pictures and
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
    slint::Timer::single_shot(Duration::from_millis(600), move || {
        for a in acts.split(';').map(str::trim).filter(|a| !a.is_empty()) {
            if let Some(i) = a.strip_prefix("select:") {
                msg::send(Msg::Select(i.parse().unwrap_or(0)));
            } else if let Some(q) = a.strip_prefix("type:") {
                msg::send(Msg::Search(format!("palette:{q}")));
            } else {
                msg::send(Msg::Act(a.to_string()));
            }
        }
        slint::Timer::single_shot(Duration::from_secs_f64(delay), move || {
            let r = TARGET
                .with(|t| t.borrow().as_ref().map(|ui| ui.window().take_snapshot()))
                .or_else(crate::console::snapshot_any);
            match r {
                Some(Ok(buf)) => {
                    let (w, h) = (buf.width(), buf.height());
                    if let Err(e) = crate::console::write_png(&out, &buf, w, h) {
                        eprintln!("shot: {e}");
                    }
                }
                Some(Err(e)) => eprintln!("shot: {e}"),
                None => eprintln!("shot: no window"),
            }
            if std::env::var_os("VROOM_SHOT_KEEP").is_none() {
                crate::console::suspend_all();
                let _ = slint::quit_event_loop();
            }
        });
    });
}

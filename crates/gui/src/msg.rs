//! One queue for everything that changes app state: UI intents, VM
//! window events, results from background threads. Handled one at a time
//! on the UI thread, so a callback fired while state is being updated
//! (Slint does that, e.g. focus changes when a list is rebuilt) just
//! queues instead of re-entering.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::PathBuf;

use crate::VmState;

#[derive(Debug)]
pub enum Msg {
    // From the library window.
    Select(usize),
    Set(String, String),
    Act(String),
    Search(String),
    Copy(String),
    Drop(PathBuf),
    DropHover(bool),
    PaletteKey(PaletteKey),
    Tick,
    // From VM windows.
    ConsoleState { key: String, state: VmState },
    Thumb { key: String, image: slint::Image },
    SnapshotTaken { key: String },
    ConsoleGone { key: String, error: Option<String> },
    /// The library window, on this VM (its key) if given.
    ShowLibrary(Option<String>),
    /// A file opened from Finder or dropped on the Dock icon.
    Open(PathBuf),
    // From background jobs.
    Toast { text: String, kind: i32, action: String },
    Refresh,
    Created(Result<PathBuf, String>, bool),
    /// A ready-made VM being made: what it does now.
    NewVmProgress(String),
    /// VMware import: percent done, then the result.
    ImportProgress(u32),
    Imported(Result<PathBuf, String>),
}

#[derive(Debug, Clone, Copy)]
pub enum PaletteKey {
    Up,
    Down,
    Enter,
    Escape,
}

type Handler = Box<dyn FnMut(Msg)>;

thread_local! {
    static QUEUE: RefCell<VecDeque<Msg>> = const { RefCell::new(VecDeque::new()) };
    static HANDLER: RefCell<Option<Handler>> = const { RefCell::new(None) };
    static SCHEDULED: Cell<bool> = const { Cell::new(false) };
    // A handler is running (a modal file dialog spins the event loop).
    static BUSY: Cell<bool> = const { Cell::new(false) };
}

pub fn set_handler(h: Handler) {
    HANDLER.with(|s| *s.borrow_mut() = Some(h));
}

/// Queue a message (UI thread).
pub fn send(m: Msg) {
    QUEUE.with(|q| q.borrow_mut().push_back(m));
    if !SCHEDULED.with(|s| s.replace(true)) {
        // From the event loop, not a timer: a handler may open a modal file
        // dialog, whose own loop runs Slint's timers (a VM window's tick),
        // and Slint aborts on timers run from inside a timer.
        if slint::invoke_from_event_loop(drain).is_err() {
            slint::Timer::single_shot(std::time::Duration::ZERO, drain);
        }
    }
}

/// Queue a message from another thread.
pub fn post(f: impl FnOnce() -> Msg + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || send(f()));
}

pub fn toast(text: impl Into<String>, kind: i32) {
    send(Msg::Toast { text: text.into(), kind, action: String::new() });
}

fn drain() {
    SCHEDULED.with(|s| s.set(false));
    loop {
        // The outer drain picks the rest up when the handler returns.
        if BUSY.with(Cell::get) {
            return;
        }
        let Some(m) = QUEUE.with(|q| q.borrow_mut().pop_front()) else { break };
        let h = HANDLER.with(|s| s.borrow_mut().take());
        match h {
            Some(mut h) => {
                BUSY.with(|b| b.set(true));
                h(m);
                BUSY.with(|b| b.set(false));
                // Put it back unless it was replaced meanwhile.
                HANDLER.with(|s| {
                    let mut s = s.borrow_mut();
                    if s.is_none() {
                        *s = Some(h);
                    }
                });
            }
            None => crate::fallback(m),
        }
    }
}

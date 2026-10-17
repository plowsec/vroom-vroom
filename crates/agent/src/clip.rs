//! Guest clipboard backends.

/// Text clipboard.
pub trait Clip {
    fn get(&mut self) -> Option<String>;
    fn set(&mut self, text: &str);
}

/// Clipboard disabled.
pub struct NoClip;

impl Clip for NoClip {
    fn get(&mut self) -> Option<String> {
        None
    }
    fn set(&mut self, _text: &str) {}
}

/// A plain file as clipboard (headless guests, tests).
pub struct FileClip {
    path: String,
}

impl FileClip {
    pub fn new(path: &str) -> Self {
        FileClip { path: path.to_string() }
    }
}

impl Clip for FileClip {
    fn get(&mut self) -> Option<String> {
        std::fs::read_to_string(&self.path).ok()
    }
    fn set(&mut self, text: &str) {
        if let Err(e) = std::fs::write(&self.path, text) {
            log::warn!("clipboard file: {e}");
        }
    }
}

struct SystemClip(arboard::Clipboard);

impl Clip for SystemClip {
    fn get(&mut self) -> Option<String> {
        self.0.get_text().ok()
    }
    fn set(&mut self, text: &str) {
        if let Err(e) = self.0.set_text(text.to_string()) {
            log::warn!("clipboard set: {e}");
        }
    }
}

/// The desktop's clipboard, if there is one now. Linux: X11's, also in
/// Wayland sessions (Xwayland: GNOME, KDE and wlroots pass the clipboard
/// between X11 and Wayland apps both ways).
pub fn system() -> Option<Box<dyn Clip>> {
    match arboard::Clipboard::new() {
        Ok(c) => Some(Box::new(SystemClip(c))),
        Err(e) => {
            log::info!("no desktop clipboard yet ({e})");
            None
        }
    }
}

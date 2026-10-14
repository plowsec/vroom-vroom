//! Host clipboard providers for headless (serial) mode.

use std::path::PathBuf;

use bulwark_vmm::agent::ClipboardProvider;

/// System clipboard through arboard.
struct HostClipboard(arboard::Clipboard);

impl ClipboardProvider for HostClipboard {
    fn get_text(&mut self) -> Option<String> {
        self.0.get_text().ok()
    }
    fn set_text(&mut self, text: &str) {
        let _ = self.0.set_text(text.to_string());
    }
}

/// A file as clipboard (headless hosts, tests).
struct FileClipboard(PathBuf);

impl ClipboardProvider for FileClipboard {
    fn get_text(&mut self) -> Option<String> {
        std::fs::read_to_string(&self.0).ok()
    }
    fn set_text(&mut self, text: &str) {
        let _ = std::fs::write(&self.0, text);
    }
}

pub fn provider(file: Option<PathBuf>) -> Option<Box<dyn ClipboardProvider>> {
    if let Some(p) = file {
        return Some(Box::new(FileClipboard(p)));
    }
    match arboard::Clipboard::new() {
        Ok(c) => Some(Box::new(HostClipboard(c))),
        Err(e) => {
            log::info!("no host clipboard: {e}");
            None
        }
    }
}

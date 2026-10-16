//! USB passthrough, outside the VMM.
//!
//! The VMM's xHCI controller never touches the host's USB stack. Each
//! passed-through device gets a helper process (`vroom usb-host`) that
//! opens that one device with libusb and runs its transfers; the two
//! talk over a socketpair with a small protocol ([`proto`]). The helper
//! runs in its own sandbox and treats the VMM as hostile: it checks every
//! request against the device's descriptors and caps sizes and counts.

#[forbid(unsafe_code)]
pub mod desc;
#[forbid(unsafe_code)]
pub mod fake;
#[forbid(unsafe_code)]
pub mod helper;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod libusb;
pub mod list;
#[forbid(unsafe_code)]
pub mod proto;

use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Which device a helper serves (from the VM's `[[usb]]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    pub vendor: u16,
    pub product: u16,
    pub serial: Option<String>,
}

/// The helper: serve the VMM on `sock` (its stdin) for the device `sel`.
/// `VROOM_USB_FAKE=acm` stands in a pretend serial port for tests,
/// `VROOM_USB_FAKE=audio` a pretend sound card
/// (`VROOM_USB_FAKE_UNPLUG=<secs>`: pull it out once, after that long).
/// `lockdown` runs once the device library is loaded, before the first
/// request is read (Linux: the helper sandboxes itself there).
pub fn helper_main(sock: UnixStream, sel: Selector, lockdown: impl FnOnce() -> std::io::Result<()>) -> std::io::Result<()> {
    let opener: Box<dyn helper::Opener> = match std::env::var("VROOM_USB_FAKE").ok().as_deref() {
        Some(k @ ("acm" | "audio")) => {
            let unplug = std::env::var("VROOM_USB_FAKE_UNPLUG").ok().and_then(|s| s.parse().ok()).map(Duration::from_secs);
            let (kind, what) = if k == "acm" { (fake::FakeKind::Acm, "serial port") } else { (fake::FakeKind::Audio, "sound card") };
            log::warn!("usb: test mode, a pretend {what} as {:04x}:{:04x}", sel.vendor, sel.product);
            Box::new(fake::FakeOpener::new(kind, sel.vendor, sel.product, unplug))
        }
        Some(other) => return Err(std::io::Error::other(format!("VROOM_USB_FAKE={other}: \"acm\" or \"audio\""))),
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        None => Box::new(libusb::LibusbOpener::new(sel).map_err(std::io::Error::other)?),
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        None => return Err(std::io::Error::other("USB passthrough isn't supported on this host")),
    };
    lockdown()?;
    helper::serve(sock, opener)
}

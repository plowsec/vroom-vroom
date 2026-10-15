//! USB passthrough: one helper process (`vroom usb-host`) per `[[usb]]`
//! device, started before the VMM's own sandbox, in its own sandbox on
//! macOS (the IOKit USB user clients and libusb, nothing else). The VMM
//! never opens a host device; it sends the helper transfer requests over
//! a socketpair and checks what comes back (xHCI side).
//!
//! The vCPUs never block on a helper: requests go through a queue to a
//! writer thread, and a reader thread hands answers to the controller.

use std::io::{BufReader, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vroom_config::Usb;
use vroom_devices::usb::xhci::Xhci;
use vroom_usb::proto::{self, Event, Request};

use crate::{Error, Result};

/// A started helper, not yet wired to the controller.
pub(crate) struct UsbHelper {
    name: String,
    sock: UnixStream,
    child: Child,
    rx: Receiver<Request>,
}

impl std::fmt::Debug for UsbHelper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsbHelper").field("name", &self.name).finish()
    }
}

/// Start the helper for `dev`. Returns it and where the controller sends
/// its requests.
pub(crate) fn spawn(dev: &Usb) -> Result<(UsbHelper, Sender<Request>)> {
    let exe = std::env::current_exe().map_err(|e| Error::Setup(format!("usb: {e}")))?;
    let (ours, theirs) = UnixStream::pair()?;
    let mut cmd = command(&exe)?;
    cmd.arg("usb-host").arg("--id").arg(&dev.id);
    if let Some(s) = &dev.serial {
        cmd.arg("--serial").arg(s);
    }
    let child = cmd
        .stdin(Stdio::from(OwnedFd::from(theirs)))
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| Error::Setup(format!("usb helper: {e}")))?;
    log::info!("usb: helper {} for {}", child.id(), dev.id);
    crate::note_helper(child.id());
    let (tx, rx) = mpsc::channel();
    Ok((UsbHelper { name: dev.name.clone().unwrap_or_else(|| dev.id.clone()), sock: ours, child, rx }, tx))
}

fn command(exe: &Path) -> Result<Command> {
    #[cfg(target_os = "macos")]
    {
        let prof = vroom_sandbox::usb_profile(exe).map_err(|e| Error::Setup(format!("usb sandbox: {e}")))?;
        let mut c = Command::new("/usr/bin/sandbox-exec");
        c.arg("-p").arg(prof).arg(exe);
        Ok(c)
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Sandboxes itself once libusb is loaded.
        Ok(Command::new(exe))
    }
}

impl UsbHelper {
    /// Wire the helper to device `index` of `xhci`: a writer thread for
    /// requests, a reader thread for answers. The threads own the helper
    /// from now on; it exits when its socket closes (with us).
    pub(crate) fn run(self, index: usize, xhci: Arc<Mutex<Xhci>>) -> Result<()> {
        let UsbHelper { name, sock, child, rx } = self;
        let mut w = sock.try_clone()?;
        std::thread::Builder::new().name(format!("usb{index}-out")).spawn(move || {
            for r in rx {
                if w.write_all(&r.encode()).is_err() {
                    break;
                }
            }
            let _ = w.shutdown(std::net::Shutdown::Both);
        })?;
        std::thread::Builder::new().name(format!("usb{index}-in")).spawn(move || {
            let mut child = child;
            let mut r = BufReader::new(sock);
            loop {
                match proto::read_event(&mut r) {
                    Ok(Some(ev)) => xhci.lock().unwrap_or_else(|e| e.into_inner()).usb_event(index, ev),
                    Ok(None) => {
                        log::warn!("usb: helper for {name} exited");
                        break;
                    }
                    Err(e) => {
                        // A helper talking nonsense is cut off.
                        log::warn!("usb: helper for {name}: {e}");
                        break;
                    }
                }
            }
            xhci.lock().unwrap_or_else(|e| e.into_inner()).usb_event(index, Event::Detached);
            let _ = r.get_ref().shutdown(std::net::Shutdown::Both);
            let _ = child.kill();
            for _ in 0..50 {
                if !matches!(child.try_wait(), Ok(None)) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })?;
        Ok(())
    }
}

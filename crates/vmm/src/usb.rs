//! USB passthrough: one helper process (`vroom usb-host`) per host
//! device. The VM file's `[[usb]]` devices get theirs here, before the
//! VMM's own sandbox, in its own sandbox on macOS (the IOKit USB user
//! clients and libusb, nothing else). Devices plugged in while the VM
//! runs come from the app: it starts the helper (or has the root helper
//! start it) and hands us its socket over the viewer connection. Either
//! way the VMM never opens a host device; it sends the helper transfer
//! requests and checks what comes back (xHCI side).
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
use vroom_display::{ToViewer, UsbDev, UsbPort};
use vroom_usb::proto::{self, Event, Request};

use crate::display::ViewerTx;
use crate::{Error, Result};

/// A started helper, not yet wired to the controller.
pub(crate) struct UsbHelper {
    dev: UsbDev,
    sock: UnixStream,
    child: Child,
    rx: Receiver<Request>,
}

impl std::fmt::Debug for UsbHelper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsbHelper").field("dev", &self.dev).finish()
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
    let d = UsbDev { id: dev.id.clone(), serial: dev.serial.clone().unwrap_or_default(), name: dev.name.clone().unwrap_or_default() };
    Ok((UsbHelper { dev: d, sock: ours, child, rx }, tx))
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

/// One device on a port pair.
struct Entry {
    dev: UsbDev,
    gen: u64,
    /// Its helper has it open.
    here: bool,
    /// From the VM file.
    always: bool,
    /// To end the helper: shutting this down closes its socket.
    sock: UnixStream,
}

/// The xHCI's host-device port pairs, and what is plugged into them.
pub(crate) struct UsbPorts {
    xhci: Arc<Mutex<Xhci>>,
    ports: Mutex<Vec<Option<Entry>>>,
    viewer: Option<ViewerTx>,
}

/// Port pairs we keep track of: no more than the viewer's list holds
/// (the xHCI has fewer).
const MAX_PORTS: usize = vroom_display::USB_PORTS_MAX;

impl UsbPorts {
    /// Wire the VM file's devices (already on pairs 0..n of `xhci`).
    pub(crate) fn new(xhci: Arc<Mutex<Xhci>>, helpers: Vec<UsbHelper>, viewer: Option<ViewerTx>) -> Result<Arc<UsbPorts>> {
        let me = Arc::new(UsbPorts { xhci, ports: Mutex::new((0..MAX_PORTS).map(|_| None).collect()), viewer });
        for (i, h) in helpers.into_iter().enumerate() {
            let UsbHelper { dev, sock, child, rx } = h;
            me.lock()[i] = Some(Entry { dev, gen: 0, here: false, always: true, sock: sock.try_clone()? });
            me.clone().run(i, 0, sock, rx, Some(child))?;
        }
        me.tell();
        Ok(me)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Option<Entry>>> {
        self.ports.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn xhci(&self) -> std::sync::MutexGuard<'_, Xhci> {
        self.xhci.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The viewer's list, now.
    fn tell(&self) {
        let Some(v) = &self.viewer else { return };
        let list = self
            .lock()
            .iter()
            .enumerate()
            .filter_map(|(i, e)| e.as_ref().map(|e| UsbPort { port: i as u8, dev: e.dev.clone(), here: e.here, always: e.always }))
            .collect();
        v.send(&ToViewer::Usb(list));
    }

    /// A device the app plugs in: `sock` is its helper's socket.
    pub(crate) fn attach(self: &Arc<Self>, dev: UsbDev, sock: UnixStream) -> std::result::Result<u8, String> {
        if !dev.valid() {
            return Err("not a USB device id".into());
        }
        let (tx, rx) = mpsc::channel();
        let (i, gen) = {
            let mut ports = self.lock();
            let mut x = self.xhci();
            let i = x.pass_free().filter(|&i| i < ports.len()).ok_or("all USB ports of this VM are in use")?;
            let gen = x.pass_plug(i, tx).ok_or("all USB ports of this VM are in use")?;
            let keep = sock.try_clone().map_err(|e| e.to_string())?;
            ports[i] = Some(Entry { dev: dev.clone(), gen, here: false, always: false, sock: keep });
            (i, gen)
        };
        log::info!("usb: {} ({}) plugged into port pair {i}", dev.id, dev.name);
        if let Err(e) = self.clone().run(i, gen, sock, rx, None) {
            self.gone(i, gen);
            return Err(e.to_string());
        }
        self.tell();
        Ok(i as u8)
    }

    /// Pull out the device on pair `i`: the guest sees it go, its helper
    /// sees its socket close and ends.
    pub(crate) fn detach(&self, i: u8) {
        let Some(e) = self.lock().get_mut(i as usize).and_then(Option::take) else { return };
        self.xhci().pass_unplug(i as usize, e.gen);
        let _ = e.sock.shutdown(std::net::Shutdown::Both);
        log::info!("usb: {} unplugged from port pair {i}", e.dev.id);
        self.tell();
    }

    /// The helper of generation `gen` ended (or was told to).
    fn gone(&self, i: usize, gen: u64) {
        let was = {
            let mut ports = self.lock();
            match ports.get_mut(i) {
                Some(slot) if slot.as_ref().is_some_and(|e| e.gen == gen) => slot.take(),
                _ => None,
            }
        };
        if was.is_some() {
            self.xhci().pass_unplug(i, gen);
            self.tell();
        }
    }

    fn here(&self, i: usize, gen: u64, here: bool) {
        let changed = match self.lock().get_mut(i).and_then(Option::as_mut) {
            Some(e) if e.gen == gen && e.here != here => {
                e.here = here;
                true
            }
            _ => false,
        };
        if changed {
            self.tell();
        }
    }

    /// Wire a helper to pair `i`: a writer thread for requests, a reader
    /// thread for answers. The threads own it from now on; it exits when
    /// its socket closes (with us, or `detach`).
    fn run(self: Arc<Self>, i: usize, gen: u64, sock: UnixStream, rx: Receiver<Request>, child: Option<Child>) -> Result<()> {
        let mut w = sock.try_clone()?;
        std::thread::Builder::new().name(format!("usb{i}-out")).spawn(move || {
            for r in rx {
                if w.write_all(&r.encode()).is_err() {
                    break;
                }
            }
            let _ = w.shutdown(std::net::Shutdown::Both);
        })?;
        std::thread::Builder::new().name(format!("usb{i}-in")).spawn(move || {
            let mut r = BufReader::new(sock);
            loop {
                match proto::read_event(&mut r) {
                    Ok(Some(ev)) => {
                        match ev {
                            Event::Attached { .. } => self.here(i, gen, true),
                            Event::Detached => self.here(i, gen, false),
                            Event::Done(_) | Event::IsoDone(_) => {}
                        }
                        self.xhci().pass_event(i, gen, ev);
                    }
                    Ok(None) => {
                        log::info!("usb: helper on port pair {i} ended");
                        break;
                    }
                    Err(e) => {
                        // A helper talking nonsense is cut off.
                        log::warn!("usb: helper on port pair {i}: {e}");
                        break;
                    }
                }
            }
            let _ = r.get_ref().shutdown(std::net::Shutdown::Both);
            self.gone(i, gen);
            if let Some(mut child) = child {
                let _ = child.kill();
                for _ in 0..50 {
                    if !matches!(child.try_wait(), Ok(None)) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        })?;
        Ok(())
    }
}

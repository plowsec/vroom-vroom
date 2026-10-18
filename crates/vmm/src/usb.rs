//! USB passthrough: one helper process (`vroom usb-host`) per host
//! device. The VM file's `[[usb]]` devices get theirs here, before the
//! VMM's own sandbox, in its own sandbox on macOS (the IOKit USB user
//! clients and libusb, nothing else). Devices plugged in while the VM
//! runs come from the app: it starts the helper (or has the root helper
//! start it) and hands us its socket over the viewer connection. A VM
//! file's device that a macOS driver holds (a USB stick, a serial
//! adapter) needs root to take: we ask vroom-netd's root helper for it,
//! as the app does, when netd is set up. Either
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
use vroom_display::{ToViewer, UsbDev, UsbHow, UsbPort, FILES_PORT0};
use vroom_usb::msd::{Ids, Store, StoreOpener};
use vroom_usb::rawdisk::FdStore;
use vroom_virtio::devices::fs::FsSlot;
use vroom_usb::proto::{self, Event, Request};

use crate::agent::{Agent, AgentCmd};
use crate::display::ViewerTx;
use crate::{Error, Result};

/// A started helper, not yet wired to the controller.
pub(crate) struct UsbHelper {
    dev: UsbDev,
    sock: UnixStream,
    /// None for netd's root helper (not our child).
    child: Option<Child>,
    rx: Receiver<Request>,
}

impl std::fmt::Debug for UsbHelper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsbHelper").field("dev", &self.dev).finish()
    }
}

/// Start the helper for `dev`. Returns it and where the controller sends
/// its requests.
pub(crate) fn spawn(vm: &str, dev: &Usb) -> Result<(UsbHelper, Sender<Request>)> {
    let d = UsbDev { id: dev.id.clone(), serial: dev.serial.clone().unwrap_or_default(), name: dev.name.clone().unwrap_or_default() };
    if let Some(drivers) = held_by_macos(dev) {
        let (tx, rx) = mpsc::channel();
        let sock = via_netd(vm, dev).map_err(|why| {
            Error::Setup(format!(
                "usb {}: held by {}, and only the root helper can take it: {why}. \
                 Set the helper up in the app (Settings, Network or USB)",
                dev.id,
                drivers.join(", ")
            ))
        })?;
        log::info!("usb: {} from the root helper (held by {})", dev.id, drivers.join(", "));
        return Ok((UsbHelper { dev: d, sock, child: None, rx }, tx));
    }
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
    Ok((UsbHelper { dev: d, sock: ours, child: Some(child), rx }, tx))
}

/// Why only root can take `dev`, if so: the macOS drivers holding its
/// interfaces, or an earlier root helper's capture (macOS then lets only
/// root open it, until it is re-enumerated).
fn held_by_macos(dev: &Usb) -> Option<Vec<String>> {
    let h = vroom_usb::list::list().into_iter().find(|h| {
        h.id() == dev.id && dev.serial.as_ref().is_none_or(|s| h.serial.as_deref() == Some(s.as_str()))
    })?;
    if !h.needs_root() {
        return None;
    }
    Some(if h.claimed_by.is_empty() { vec!["an earlier root helper's capture".into()] } else { h.claimed_by })
}

/// netd's root helper for `dev`: its socket.
fn via_netd(vm: &str, dev: &Usb) -> std::result::Result<UnixStream, String> {
    use vroom_net::netd;
    let vm: String = vm.chars().filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c)).take(64).collect();
    let req = netd::UsbRequest {
        version: netd::VERSION,
        vm: if vm.is_empty() { "vm".into() } else { vm },
        usb: netd::UsbWanted { id: dev.id.clone(), serial: dev.serial.clone() },
    };
    netd::usb(Path::new(netd::DEFAULT_SOCKET), &req).map_err(|e| match e {
        netd::NetdError::Refused(m) => m,
        e => e.to_string(),
    })
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

/// For tests without the app (a headless VM, a script):
/// `VROOM_USB_TEST_DISK=<file or /dev/rdiskN>` plugs that in as a USB
/// disk, `VROOM_USB_TEST_FILES=<dir>` shares that directory as a USB
/// disk's files, `VROOM_USB_TEST_AFTER` seconds after the start (15 by
/// default), read-only with `VROOM_USB_TEST_RO=1`; they are taken away
/// again `VROOM_USB_TEST_FOR` seconds later, if set. Opened here, before
/// the sandbox, as the app would open them; handed over the way the
/// app's are, sandbox token included.
pub(crate) fn test_seam(ports: &Arc<UsbPorts>) {
    let disk = std::env::var_os("VROOM_USB_TEST_DISK");
    let files = std::env::var_os("VROOM_USB_TEST_FILES");
    if disk.is_none() && files.is_none() {
        return;
    }
    let secs = |k: &str| std::env::var(k).ok().and_then(|s| s.parse::<u64>().ok());
    let (after, hold) = (secs("VROOM_USB_TEST_AFTER").unwrap_or(15), secs("VROOM_USB_TEST_FOR"));
    let ro = std::env::var("VROOM_USB_TEST_RO").is_ok_and(|v| v == "1");
    let dev = |name: &str| UsbDev { id: "1209:00ab".into(), serial: String::new(), name: name.into() };
    let disk = disk.and_then(|p| {
        let f = std::fs::OpenOptions::new().read(true).write(!ro).open(&p);
        f.map_err(|e| log::warn!("usb test disk {}: {e}", p.to_string_lossy())).ok().map(OwnedFd::from)
    });
    let files = files.and_then(|p| {
        let dir = std::path::PathBuf::from(p);
        let label: String = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let label = if vroom_proto::usb_label_ok(&label) { label } else { "USB".into() };
        let token = if cfg!(target_os = "macos") { vroom_sandbox::issue_extension(&dir, !ro).map_err(|e| log::warn!("usb test files: {e}")).ok()? } else { String::new() };
        let fd = std::fs::File::open(&dir).map_err(|e| log::warn!("usb test files {}: {e}", dir.display())).ok()?;
        Some((OwnedFd::from(fd), label, token))
    });
    let ports = ports.clone();
    let _ = std::thread::Builder::new().name("usb-test".into()).spawn(move || {
        std::thread::sleep(Duration::from_secs(after));
        let mut plugged = Vec::new();
        if let Some(fd) = disk {
            match ports.attach_disk(dev("Test disk"), fd, ro) {
                Ok(p) => plugged.push(p),
                Err(e) => log::warn!("usb test disk: {e}"),
            }
        }
        if let Some((fd, label, token)) = files {
            match ports.attach_files(dev("Test files"), fd, ro, &label, &token) {
                Ok(p) => plugged.push(p),
                Err(e) => log::warn!("usb test files: {e}"),
            }
        }
        if let Some(h) = hold {
            std::thread::sleep(Duration::from_secs(h));
            for p in plugged {
                ports.detach(p);
            }
        }
    });
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
    how: UsbHow,
    readonly: bool,
    /// A disk served here: says when it is flushed and closed.
    done: Option<Receiver<()>>,
}

/// The xHCI's host-device port pairs, and what is plugged into them.
pub(crate) struct UsbPorts {
    xhci: Arc<Mutex<Xhci>>,
    ports: Mutex<Vec<Option<Entry>>>,
    viewer: Option<ViewerTx>,
    /// The removable shares for USB disks' files, once the agent is up.
    files: Mutex<Option<Files>>,
}

/// USB disks' files and folders shared while the VM runs: removable
/// shares, and what is on each.
struct Files {
    slots: Vec<(String, FsSlot, Option<FilesEntry>)>,
    agent: Agent,
}

struct FilesEntry {
    /// The USB disk the files are of; None: a folder shared while the VM
    /// runs (it isn't in the USB list).
    dev: Option<UsbDev>,
    label: String,
    readonly: bool,
    /// The sandbox extension that lets us into the volume.
    ext: Option<i64>,
}

/// Removable shares a VM started on a Mac has for USB disks' files.
pub(crate) const FILE_SLOTS: usize = 2;

/// Port pairs we keep track of: no more than the viewer's list holds
/// (the xHCI has fewer).
const MAX_PORTS: usize = vroom_display::USB_PORTS_MAX - FILE_SLOTS;

/// What a USB disk handed to us says it is: the device the disk is of,
/// as a plain stick of our own (another product id, so guests apply no
/// quirks meant for the real one).
fn disk_ids(dev: &UsbDev) -> Ids {
    let name = if dev.name.is_empty() { "USB disk".to_string() } else { dev.name.clone() };
    let serial = if dev.serial.is_empty() { "0001VROOMDISK".to_string() } else { dev.serial.clone() };
    Ids { vendor: 0x1209, product: 0x00ab, manufacturer: "vroom".into(), name, serial }
}

impl UsbPorts {
    /// Wire the VM file's devices (already on pairs 0..n of `xhci`).
    pub(crate) fn new(xhci: Arc<Mutex<Xhci>>, helpers: Vec<UsbHelper>, viewer: Option<ViewerTx>) -> Result<Arc<UsbPorts>> {
        let me = Arc::new(UsbPorts { xhci, ports: Mutex::new((0..MAX_PORTS).map(|_| None).collect()), viewer, files: Mutex::new(None) });
        for (i, h) in helpers.into_iter().enumerate() {
            let UsbHelper { dev, sock, child, rx } = h;
            me.lock()[i] = Some(Entry { dev, gen: 0, here: false, always: true, sock: sock.try_clone()?, how: UsbHow::Device, readonly: false, done: None });
            me.clone().run(i, 0, sock, rx, child)?;
        }
        me.tell();
        Ok(me)
    }

    /// The removable shares (`tag`, slot) USB disks' files go to, and
    /// the agent that mounts them in the guest.
    pub(crate) fn set_files(&self, slots: Vec<(String, FsSlot)>, agent: Agent) {
        *self.files() = Some(Files { slots: slots.into_iter().map(|(t, s)| (t, s, None)).collect(), agent });
    }

    fn files(&self) -> std::sync::MutexGuard<'_, Option<Files>> {
        self.files.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A disk the app opened (the raw disk, or a file for tests): a USB
    /// stick of ours on a free port pair. Its SCSI is served in a thread
    /// here, as a helper process would; the disk fd is all it has.
    pub(crate) fn attach_disk(self: &Arc<Self>, dev: UsbDev, fd: OwnedFd, readonly: bool) -> std::result::Result<u8, String> {
        if !dev.valid() {
            return Err("not a USB device id".into());
        }
        let store = FdStore::new(std::fs::File::from(fd), readonly).map_err(|e| format!("{}: not a disk we can use ({e})", dev.name))?;
        let ro = store.readonly();
        log::info!("usb: {} as a disk: {} MB{}", dev.id, store.bytes() / 1_000_000, if ro { ", read-only" } else { "" });
        let (ours, theirs) = UnixStream::pair().map_err(|e| e.to_string())?;
        let (done_tx, done_rx) = mpsc::channel();
        let ids = disk_ids(&dev);
        std::thread::Builder::new()
            .name("usb-disk".into())
            .spawn(move || {
                if let Err(e) = vroom_usb::helper::serve(theirs, Box::new(StoreOpener::new(ids, Box::new(store)))) {
                    log::warn!("usb disk: {e}");
                }
                // The disk is flushed and closed by now.
                let _ = done_tx.send(());
            })
            .map_err(|e| e.to_string())?;
        self.plug(dev, ours, UsbHow::Disk, ro, Some(done_rx))
    }

    /// A USB disk's volume the app opened (`fd`, a directory): on a free
    /// removable share. `token` lets us in past our sandbox.
    pub(crate) fn attach_files(&self, dev: UsbDev, fd: OwnedFd, readonly: bool, label: &str, token: &str) -> std::result::Result<u8, String> {
        if !dev.valid() || !vroom_proto::usb_label_ok(label) {
            return Err("not a USB disk's volume".into());
        }
        let i = self.attach_share(Some(dev), fd, readonly, label, token)?;
        Ok(FILES_PORT0 + i as u8)
    }

    /// A folder of the Mac (`fd`, from the app) shared while the VM runs,
    /// on a free removable share; the guest mounts it as `label` (the
    /// share's tag). It becomes an ordinary share when the VM next starts.
    pub(crate) fn attach_folder(&self, fd: OwnedFd, readonly: bool, label: &str, token: &str) -> std::result::Result<(), String> {
        if !vroom_proto::usb_label_ok(label) {
            return Err(format!("{label:?}: not a name the guest can mount"));
        }
        if self.folder_slot(label).is_some() {
            return Err(format!("{label} is shared already"));
        }
        self.attach_share(None, fd, readonly, label, token).map(|_| ())
    }

    /// Stop sharing the folder `attach_folder` shared as `label`.
    pub(crate) fn detach_folder(&self, label: &str) {
        if let Some(i) = self.folder_slot(label) {
            self.detach_files(i);
        }
    }

    fn folder_slot(&self, label: &str) -> Option<usize> {
        self.files()
            .as_ref()?
            .slots
            .iter()
            .position(|(_, _, e)| e.as_ref().is_some_and(|e| e.dev.is_none() && e.label == label))
    }

    /// Serve `fd` on a free removable share and tell the agent to mount it.
    fn attach_share(&self, dev: Option<UsbDev>, fd: OwnedFd, readonly: bool, label: &str, token: &str) -> std::result::Result<usize, String> {
        let mut g = self.files();
        let Some(f) = g.as_mut() else {
            return Err("this VM has no removable shares: it started before vroom had them. Restart it".into());
        };
        let i = f
            .slots
            .iter()
            .position(|s| s.2.is_none())
            .ok_or(format!("all {} live shares in use", f.slots.len()))?;
        let ext = if token.is_empty() {
            None
        } else {
            Some(vroom_sandbox::consume_extension(token).map_err(|e| format!("{label}: {e}"))?)
        };
        let share = match vroom_fs::Share::from_fd(fd, readonly) {
            Ok(s) => s,
            Err(e) => {
                if let Some(h) = ext {
                    vroom_sandbox::release_extension(h);
                }
                return Err(format!("{label}: {e}"));
            }
        };
        let (tag, slot, entry) = &mut f.slots[i];
        slot.set(share);
        match &dev {
            Some(d) => log::info!("usb: {} ({}): its volume {label:?} on share {tag}{}", d.id, d.name, if readonly { ", read-only" } else { "" }),
            None => log::info!("shares: folder {label:?} on share {tag}{}", if readonly { ", read-only" } else { "" }),
        }
        *entry = Some(FilesEntry { dev, label: label.to_string(), readonly, ext });
        f.agent.send(AgentCmd::UsbShare { tag: tag.clone(), label: label.to_string(), on: true });
        drop(g);
        self.tell();
        Ok(i)
    }

    /// Take the files on removable share `i` away.
    fn detach_files(&self, i: usize) {
        let mut g = self.files();
        let Some(f) = g.as_mut() else { return };
        let Some((tag, slot, entry)) = f.slots.get_mut(i) else { return };
        let Some(e) = entry.take() else { return };
        // The guest unmounts first if it can; its next request finds an
        // empty share either way, and our fds on the volume close now.
        f.agent.send(AgentCmd::UsbShare { tag: tag.clone(), label: e.label.clone(), on: false });
        slot.set(vroom_fs::Share::empty());
        if let Some(h) = e.ext {
            vroom_sandbox::release_extension(h);
        }
        match &e.dev {
            Some(d) => log::info!("usb: {} ({}): its files are gone from share {tag}", d.id, d.name),
            None => log::info!("shares: folder {:?} is gone from share {tag}", e.label),
        }
        drop(g);
        self.tell();
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
        let mut list: Vec<UsbPort> = self
            .lock()
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                e.as_ref().map(|e| UsbPort { port: i as u8, dev: e.dev.clone(), here: e.here, always: e.always, how: e.how, readonly: e.readonly })
            })
            .collect();
        if let Some(f) = &*self.files() {
            for (i, (_, _, e)) in f.slots.iter().enumerate() {
                if let Some((e, dev)) = e.as_ref().and_then(|e| Some((e, e.dev.clone()?))) {
                    list.push(UsbPort { port: FILES_PORT0 + i as u8, dev, here: true, always: false, how: UsbHow::Files, readonly: e.readonly });
                }
            }
        }
        v.send(&ToViewer::Usb(list));
    }

    /// A device the app plugs in: `sock` is its helper's socket.
    pub(crate) fn attach(self: &Arc<Self>, dev: UsbDev, sock: UnixStream) -> std::result::Result<u8, String> {
        if !dev.valid() {
            return Err("not a USB device id".into());
        }
        self.plug(dev, sock, UsbHow::Device, false, None)
    }

    fn plug(self: &Arc<Self>, dev: UsbDev, sock: UnixStream, how: UsbHow, readonly: bool, done: Option<Receiver<()>>) -> std::result::Result<u8, String> {
        let (tx, rx) = mpsc::channel();
        let (i, gen) = {
            let mut ports = self.lock();
            let mut x = self.xhci();
            let i = x.pass_free().filter(|&i| i < ports.len()).ok_or("all USB ports of this VM are in use")?;
            let gen = x.pass_plug(i, tx).ok_or("all USB ports of this VM are in use")?;
            let keep = sock.try_clone().map_err(|e| e.to_string())?;
            ports[i] = Some(Entry { dev: dev.clone(), gen, here: false, always: false, sock: keep, how, readonly, done });
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
    /// sees its socket close and ends. A disk served here is flushed and
    /// closed before the viewer hears it is gone (it mounts it again on
    /// the Mac then). From `FILES_PORT0`: a USB disk's files.
    pub(crate) fn detach(&self, i: u8) {
        if i >= FILES_PORT0 {
            return self.detach_files((i - FILES_PORT0) as usize);
        }
        let Some(e) = self.lock().get_mut(i as usize).and_then(Option::take) else { return };
        self.xhci().pass_unplug(i as usize, e.gen);
        let _ = e.sock.shutdown(std::net::Shutdown::Both);
        if let Some(done) = &e.done {
            if done.recv_timeout(Duration::from_secs(10)).is_err() {
                log::warn!("usb: {}: its disk took long to close", e.dev.id);
            }
        }
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
                // It ends by itself once its socket is closed, after giving
                // the device back to the host's drivers (a re-enumeration
                // on macOS takes a moment): killed only if it hangs.
                for _ in 0..500 {
                    if !matches!(child.try_wait(), Ok(None)) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                log::warn!("usb: helper on port pair {i} didn't end, killing it");
                let _ = child.kill();
                let _ = child.wait();
            }
        })?;
        Ok(())
    }
}

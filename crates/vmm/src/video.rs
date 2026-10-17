//! Hardware video decoding: the virtio-media decoder's helper process
//! (`vroom video-helper`, sandboxed: VideoToolbox and nothing else) and
//! the shared memory window the decoder's buffers live in, which the
//! helper maps too: it writes pictures straight into the guest's buffers.

use std::collections::VecDeque;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vroom_hv::Vm;
use vroom_memory::{GuestAddress, GuestMemory};
use vroom_video::proto::{FromHelper, ToHelper};
use vroom_virtio::devices::media::{HelperLink, WindowPlacer, WINDOW};
use vroom_virtio::worker::Poker;

use crate::{Error, Result};

/// Messages from the helper waiting for the device. Pictures only come
/// against credits, so this stays small; the cap is a backstop.
const MAX_QUEUED: usize = 256;

pub(crate) struct Helper {
    sock: Mutex<UnixStream>,
    rx: Mutex<VecDeque<FromHelper>>,
    poker: Mutex<Option<Poker>>,
    alive: AtomicBool,
    child: Mutex<Child>,
}

impl std::fmt::Debug for Helper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoHelper").finish()
    }
}

impl Helper {
    /// Start the helper, hand it the buffer window `window` (shared
    /// memory) and wait for what it decodes: the helper and its codecs.
    pub fn spawn(window: &GuestMemory) -> Result<(Arc<Helper>, u32)> {
        let exe = std::env::current_exe().map_err(|e| Error::Setup(format!("video: {e}")))?;
        let (fds, _) = window.shared_fds().ok_or_else(|| Error::Setup("video: the buffer window isn't shared memory".into()))?;
        let &[(window_fd, _)] = fds.as_slice() else { return Err(Error::Setup("video: buffer window".into())) };
        let (ours, theirs) = UnixStream::pair()?;
        // Unix sockets on macOS buffer 8 KiB: a 3 MB picture would cross
        // in hundreds of steps.
        for s in [&ours, &theirs] {
            socket_buffers(s);
        }
        let child = command(&exe)?
            .arg("video-helper")
            .stdin(Stdio::from(OwnedFd::from(theirs)))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| Error::Setup(format!("video helper: {e}")))?;
        log::info!("video: helper {}", child.id());
        crate::note_helper(child.id());
        // A helper that stops reading (hung in VideoToolbox) must not
        // hang the device: writes give up, and the helper counts as gone.
        ours.set_write_timeout(Some(Duration::from_secs(2)))?;
        vroom_ipc::send_fd(&ours, window_fd).map_err(|e| Error::Setup(format!("video helper: {e}")))?;
        // Its first message says what this Mac decodes. VideoToolbox can
        // take a moment to answer the first time.
        ours.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut r = ours.try_clone()?;
        let codecs = match FromHelper::read_from(&mut r) {
            Ok(FromHelper::Caps { codecs }) => codecs,
            Ok(m) => return Err(Error::Setup(format!("video helper: {m:?} before its codecs"))),
            Err(e) => return Err(Error::Setup(format!("video helper: {e}"))),
        };
        ours.set_read_timeout(None)?;
        log::info!("video: the Mac decodes {codecs:#x}");
        let h = Arc::new(Helper {
            sock: Mutex::new(ours),
            rx: Mutex::new(VecDeque::new()),
            poker: Mutex::new(None),
            alive: AtomicBool::new(true),
            child: Mutex::new(child),
        });
        let me = h.clone();
        std::thread::Builder::new().name("video-rx".into()).spawn(move || {
            loop {
                match FromHelper::read_from(&mut r) {
                    Ok(m) => {
                        let mut q = me.rx.lock().unwrap_or_else(|e| e.into_inner());
                        if q.len() >= MAX_QUEUED {
                            log::warn!("video: the helper sends more than it may");
                            break;
                        }
                        q.push_back(m);
                    }
                    Err(e) => {
                        log::info!("video: helper gone ({e})");
                        break;
                    }
                }
                me.poke();
            }
            me.alive.store(false, Ordering::Release);
            let _ = me.sock.lock().unwrap_or_else(|e| e.into_inner()).shutdown(std::net::Shutdown::Both);
            me.poke();
        })?;
        Ok((h, codecs))
    }

    fn poke(&self) {
        if let Some(p) = &*self.poker.lock().unwrap_or_else(|e| e.into_inner()) {
            p.poke();
        }
    }
}

impl HelperLink for Helper {
    fn send(&self, m: &ToHelper) -> bool {
        if !self.alive.load(Ordering::Acquire) {
            return false;
        }
        if m.write_to(&mut *self.sock.lock().unwrap_or_else(|e| e.into_inner())).is_err() {
            self.alive.store(false, Ordering::Release);
            return false;
        }
        true
    }

    fn try_recv(&self) -> Option<FromHelper> {
        self.rx.lock().unwrap_or_else(|e| e.into_inner()).pop_front()
    }

    fn set_poker(&self, p: Poker) {
        *self.poker.lock().unwrap_or_else(|e| e.into_inner()) = Some(p);
    }

    fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.sock.lock().unwrap_or_else(|e| e.into_inner()).shutdown(std::net::Shutdown::Both);
        let _ = self.child.lock().unwrap_or_else(|e| e.into_inner()).try_wait();
    }
}

/// Ask for 4 MiB socket buffers each way (best effort).
fn socket_buffers(s: &UnixStream) {
    use std::os::fd::AsRawFd;
    let v: libc::c_int = 4 << 20;
    for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        // SAFETY: a valid socket and a c_int option value of its size.
        let r = unsafe {
            libc::setsockopt(s.as_raw_fd(), libc::SOL_SOCKET, opt, &v as *const _ as *const libc::c_void, std::mem::size_of::<libc::c_int>() as libc::socklen_t)
        };
        if r != 0 {
            log::debug!("video: socket buffer: {}", std::io::Error::last_os_error());
        }
    }
}

fn command(exe: &std::path::Path) -> Result<Command> {
    #[cfg(target_os = "macos")]
    {
        let prof = vroom_sandbox::video_profile(exe).map_err(|e| Error::Setup(format!("video sandbox: {e}")))?;
        let mut c = Command::new("/usr/bin/sandbox-exec");
        c.arg("-p").arg(prof).arg(exe);
        Ok(c)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(Command::new(exe))
    }
}

/// The decoder's buffers: [`WINDOW`] bytes of our memory, mapped into
/// the guest wherever it puts the device's shared memory region. Only
/// what the guest's buffers touch becomes real memory.
pub(crate) struct Window {
    vm: Arc<dyn Vm>,
    mem: GuestMemory,
    at: Mutex<Option<u64>>,
}

impl Window {
    pub fn new(vm: Arc<dyn Vm>) -> Result<(Arc<Window>, GuestMemory)> {
        let mem = GuestMemory::new_shared(&[(GuestAddress(0), WINDOW)])?;
        Ok((Arc::new(Window { vm, mem: mem.clone(), at: Mutex::new(None) }), mem))
    }
}

impl WindowPlacer for Window {
    fn place(&self, gpa: u64) {
        // A BAR sits at a multiple of its size; anything else is the
        // firmware sizing it or a move half done.
        if gpa == 0 || !gpa.is_multiple_of(WINDOW) || gpa.checked_add(WINDOW).is_none() {
            return;
        }
        let mut at = self.at.lock().unwrap_or_else(|e| e.into_inner());
        if *at == Some(gpa) {
            return;
        }
        if let Some(old) = at.take() {
            let _ = self.vm.unmap_host(old, WINDOW);
        }
        let Some(r) = self.mem.regions().next() else { return };
        // SAFETY: `mem` is ours and kept (in self) as long as the mapping:
        // it is removed above before a new one, and the VM goes before us.
        match unsafe { self.vm.map_host(gpa, r.host_ptr(), WINDOW) } {
            Ok(()) => *at = Some(gpa),
            Err(e) => log::warn!("video: can't map the buffer window at {gpa:#x}: {e}"),
        }
    }
}

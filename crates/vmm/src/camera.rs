//! The guest's USB camera, fed by the Mac's camera through a helper
//! process (`vroom camera-helper`, sandboxed: the camera and nothing
//! else). The camera runs only while the guest reads frames and the VM's
//! window is open; the window shows when it does.

use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vroom_devices::usb::uvc::CameraFeed;
use vroom_devices::usb::xhci::Xhci;
use vroom_display::ToViewer;

use crate::display::ViewerTx;
use crate::{Error, Result};

/// Biggest frame side the helper may send.
const MAX_SIDE: u16 = 4096;

#[derive(Default)]
struct State {
    /// A window is open (no window: no camera, like the microphone).
    window: bool,
    /// What the helper runs now.
    on: Option<(u16, u16, u32)>,
    viewer: Option<ViewerTx>,
}

pub(crate) struct CameraHelper {
    sock: Mutex<UnixStream>,
    feed: Arc<CameraFeed>,
    state: Mutex<State>,
    child: Mutex<Child>,
}

impl std::fmt::Debug for CameraHelper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CameraHelper").finish()
    }
}

impl CameraHelper {
    /// Start the helper. `test`: color bars instead of the camera.
    pub fn spawn(feed: Arc<CameraFeed>, test: bool) -> Result<Arc<CameraHelper>> {
        let exe = std::env::current_exe().map_err(|e| Error::Setup(format!("camera: {e}")))?;
        let (ours, theirs) = UnixStream::pair()?;
        let mut cmd = command(&exe)?;
        cmd.arg("camera-helper");
        if test {
            cmd.env("VROOM_CAMERA_TEST", "1");
        }
        let child = cmd
            .stdin(Stdio::from(OwnedFd::from(theirs)))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| Error::Setup(format!("camera helper: {e}")))?;
        log::info!("camera: helper {}{}", child.id(), if test { " (test pattern)" } else { "" });
        crate::note_helper(child.id());
        Ok(Arc::new(CameraHelper { sock: Mutex::new(ours), feed, state: Mutex::default(), child: Mutex::new(child) }))
    }

    pub fn set_viewer(&self, tx: ViewerTx) {
        self.lock().viewer = Some(tx);
    }

    /// A VM window is open (true) or gone (false).
    pub fn set_window(&self, open: bool) {
        self.lock().window = open;
        self.update();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run or stop the camera to match what the guest wants and the gate.
    fn update(&self) {
        let want = self.feed.wanted();
        let mut s = self.lock();
        let should = want.filter(|_| s.window);
        if should == s.on {
            return;
        }
        let mut cmd = [0u8; 9];
        if let Some((w, h, fps)) = should {
            cmd[0] = 1;
            cmd[1..3].copy_from_slice(&w.to_le_bytes());
            cmd[3..5].copy_from_slice(&h.to_le_bytes());
            cmd[5..9].copy_from_slice(&fps.to_le_bytes());
            log::info!("camera: on, {w}x{h} at {fps} fps");
        } else {
            log::info!("camera: off");
        }
        s.on = should;
        drop(s);
        let _ = self.sock.lock().unwrap_or_else(|e| e.into_inner()).write_all(&cmd);
    }

    fn news(&self, text: &str) {
        let v = self.lock().viewer.clone();
        let Some(v) = v else { return };
        match text {
            "on" => v.send(&ToViewer::Camera(true)),
            "off" => v.send(&ToViewer::Camera(false)),
            "denied" => v.send(&ToViewer::Status(
                "Camera: not allowed. Allow the app that started vroom (Terminal, iTerm...) in System Settings > Privacy & Security > Camera.".into(),
            )),
            "no camera" => v.send(&ToViewer::Status("Camera: this Mac has none (or another app holds it)".into())),
            _ => {}
        }
    }

    /// Follow the guest and pass frames to the controller.
    pub fn run(self: &Arc<Self>, xhci: Arc<Mutex<Xhci>>) -> Result<()> {
        let me = self.clone();
        let x = xhci.clone();
        std::thread::Builder::new().name("camera-ctl".into()).spawn(move || loop {
            me.feed.wait(Duration::from_millis(500));
            // A guest waiting for a frame doesn't ask again until one comes
            // (the camera may take seconds: macOS asking for permission):
            // look at its queued transfer, which says it still wants them.
            x.lock().unwrap_or_else(|e| e.into_inner()).camera_frame();
            me.update();
        })?;
        let me = self.clone();
        let mut r = self.sock.lock().unwrap_or_else(|e| e.into_inner()).try_clone()?;
        std::thread::Builder::new().name("camera-rx".into()).spawn(move || {
            let mut kind = [0u8; 1];
            while r.read_exact(&mut kind).is_ok() {
                match kind[0] {
                    1 => {
                        let mut h = [0u8; 8];
                        if r.read_exact(&mut h).is_err() {
                            break;
                        }
                        let (w, ht, n) = (u16::from_le_bytes([h[0], h[1]]), u16::from_le_bytes([h[2], h[3]]), u32::from_le_bytes([h[4], h[5], h[6], h[7]]));
                        if w == 0 || ht == 0 || w > MAX_SIDE || ht > MAX_SIDE || n as usize != w as usize * ht as usize * 2 {
                            log::warn!("camera: bad frame from the helper");
                            break;
                        }
                        let mut px = vec![0u8; n as usize];
                        if r.read_exact(&mut px).is_err() {
                            break;
                        }
                        if me.feed.push(w, ht, px) {
                            xhci.lock().unwrap_or_else(|e| e.into_inner()).camera_frame();
                        }
                    }
                    2 => {
                        let mut n = [0u8; 2];
                        if r.read_exact(&mut n).is_err() {
                            break;
                        }
                        let mut t = vec![0u8; u16::from_le_bytes(n).min(256) as usize];
                        if r.read_exact(&mut t).is_err() {
                            break;
                        }
                        me.news(&String::from_utf8_lossy(&t));
                    }
                    _ => break,
                }
            }
            log::info!("camera: helper gone");
            me.news("off");
        })?;
        Ok(())
    }
}

impl Drop for CameraHelper {
    fn drop(&mut self) {
        let _ = self.sock.lock().unwrap_or_else(|e| e.into_inner()).shutdown(std::net::Shutdown::Both);
        let _ = self.child.lock().unwrap_or_else(|e| e.into_inner()).try_wait();
    }
}

fn command(exe: &std::path::Path) -> Result<Command> {
    #[cfg(target_os = "macos")]
    {
        let prof = vroom_sandbox::camera_profile(exe).map_err(|e| Error::Setup(format!("camera sandbox: {e}")))?;
        let mut c = Command::new("/usr/bin/sandbox-exec");
        c.arg("-p").arg(prof).arg(exe);
        Ok(c)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(Command::new(exe))
    }
}

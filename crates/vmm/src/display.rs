//! VMM side of the display channel.
//!
//! Owns the shared framebuffer and the socket to the viewer. Guest frames
//! come in through [`FbSink`] (virtio-gpu), viewer input goes out to the
//! virtio-input devices, resize to the GPU / agent, focus + clipboard to
//! the agent, lifecycle commands to the main loop.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::sync::mpsc::Sender;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use bulwark_display::{send_fd, Decoder, SharedFb, ToViewer, ToVmm};
use bulwark_devices::usb::hid::HidEvent;
use bulwark_devices::usb::xhci::Xhci;
use bulwark_virtio::devices::gpu::{DisplaySink, GpuControl};
use bulwark_virtio::devices::input::{self as ev, event};

use crate::agent::{Agent, AgentCmd, ClipboardProvider};
use crate::vcpu::VmEvent;

/// Sends to the viewer without ever blocking the caller.
///
/// A dedicated thread does the (blocking) socket writes. Damage is
/// coalesced into one "something changed" flag, so a viewer that stops
/// reading costs us at most a handful of queued messages, never a stall.
#[derive(Clone)]
pub(crate) struct ViewerTx {
    q: std::sync::mpsc::Sender<ToViewer>,
    damage_pending: Arc<AtomicBool>,
}

impl ViewerTx {
    fn spawn(mut sock: UnixStream) -> std::io::Result<Self> {
        let (q, rx) = std::sync::mpsc::channel::<ToViewer>();
        let damage_pending = Arc::new(AtomicBool::new(false));
        let dp = damage_pending.clone();
        std::thread::Builder::new().name("viewer-tx".into()).spawn(move || {
            while let Ok(m) = rx.recv() {
                if matches!(m, ToViewer::Damage { .. }) {
                    dp.store(false, Ordering::Release);
                }
                if sock.write_all(&m.encode()).is_err() {
                    break;
                }
            }
        })?;
        Ok(ViewerTx { q, damage_pending })
    }

    pub fn send(&self, m: &ToViewer) {
        if let ToViewer::Damage { .. } = m {
            if self.damage_pending.swap(true, Ordering::AcqRel) {
                return;
            }
            // The viewer redraws the whole frame anyway.
            let _ = self.q.send(ToViewer::Damage { x: 0, y: 0, w: 0, h: 0 });
            return;
        }
        let _ = self.q.send(m.clone());
    }
}

pub(crate) struct FbSink {
    fb: SharedFb,
    mode: Mutex<(u32, u32)>,
    /// virtio-gpu owns the screen once its driver sets a mode; ramfb
    /// (firmware / boot screen) only shows until then.
    gpu_active: AtomicBool,
    /// None when headless: frames still land in the buffer (screendumps).
    tx: Option<ViewerTx>,
}

impl FbSink {
    pub fn headless() -> std::io::Result<Arc<Self>> {
        let fb = SharedFb::create().map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(Arc::new(FbSink { fb, mode: Mutex::new((0, 0)), gpu_active: AtomicBool::new(false), tx: None }))
    }

    /// A full frame from ramfb. Ignored while virtio-gpu is driving.
    pub fn ramfb_frame(&self, w: u32, h: u32, rows: &[u8], stride: usize) {
        if self.gpu_active.load(Ordering::Acquire) {
            return;
        }
        self.apply_mode(w, h);
        self.draw(0, 0, w, h, rows, stride);
    }

    fn apply_mode(&self, width: u32, height: u32) {
        let mut m = self.mode.lock().unwrap_or_else(|e| e.into_inner());
        if *m == (width, height) {
            return;
        }
        *m = (width, height);
        drop(m);
        if let Some(tx) = &self.tx {
            tx.send(&ToViewer::Mode { width, height });
        }
    }

    fn draw(&self, x: u32, y: u32, w: u32, h: u32, rows: &[u8], stride: usize) {
        let (mw, mh) = *self.mode.lock().unwrap_or_else(|e| e.into_inner());
        if x.saturating_add(w) > mw || y.saturating_add(h) > mh {
            return;
        }
        self.fb.write_rect(mw, x, y, w, h, rows, stride);
        if let Some(tx) = &self.tx {
            tx.send(&ToViewer::Damage { x, y, w, h });
        }
    }

    /// Write the current screen as PNG.
    pub fn screendump(&self, path: &std::path::Path) -> std::io::Result<(u32, u32)> {
        let (w, h) = *self.mode.lock().unwrap_or_else(|e| e.into_inner());
        if w == 0 || h == 0 {
            return Err(std::io::Error::other("display is off"));
        }
        let mut px = vec![0u32; w as usize * h as usize];
        self.fb.read_into(w, h, &mut px);
        let rgb: Vec<u8> = px.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8]).collect();
        let f = std::fs::File::create(path)?;
        let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().map_err(std::io::Error::other)?;
        wr.write_image_data(&rgb).map_err(std::io::Error::other)?;
        Ok((w, h))
    }
}

impl DisplaySink for FbSink {
    fn set_mode(&self, width: u32, height: u32) {
        if width != 0 && !SharedFb::mode_ok(width, height) {
            log::warn!("display: mode {width}x{height} too large, ignored");
            return;
        }
        self.gpu_active.store(width != 0, Ordering::Release);
        self.apply_mode(width, height);
    }

    fn flush(&self, x: u32, y: u32, w: u32, h: u32, rows: &[u8], stride: usize) {
        if self.gpu_active.load(Ordering::Acquire) {
            self.draw(x, y, w, h, rows, stride);
        }
    }
}

/// Host clipboard that lives in the viewer process.
struct RemoteClipboard {
    latest: Arc<Mutex<Option<String>>>,
    tx: ViewerTx,
}

impl ClipboardProvider for RemoteClipboard {
    fn get_text(&mut self) -> Option<String> {
        self.latest.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn set_text(&mut self, text: &str) {
        self.tx.send(&ToViewer::ClipboardSet(text.to_string()));
    }
}

/// Built before the machine; finished once devices exist.
pub(crate) struct DisplayServer {
    stream: UnixStream,
    tx: ViewerTx,
    clip_latest: Arc<Mutex<Option<String>>>,
}

pub(crate) struct InputPipes {
    pub kbd: Option<UnixDatagram>,
    pub tablet: Option<UnixDatagram>,
    /// USB HID (xHCI) instead of virtio-input. Exactly one path is used
    /// so guests never see doubled keystrokes.
    pub usb: Option<Arc<Mutex<Xhci>>>,
}

impl InputPipes {
    fn none() -> Self {
        InputPipes { kbd: None, tablet: None, usb: None }
    }
}

fn to_hid(m: &ToVmm) -> Option<HidEvent> {
    Some(match *m {
        ToVmm::Key { code, down } => HidEvent::Key { code, down },
        ToVmm::Pointer { x, y } => HidEvent::Pointer { x, y },
        ToVmm::Button { code, down } => HidEvent::Button { code, down },
        ToVmm::Wheel { dy, .. } => HidEvent::Wheel { dy: dy.clamp(-127, 127) as i8 },
        _ => return None,
    })
}

impl DisplayServer {
    pub fn new(stream: UnixStream) -> std::io::Result<Self> {
        let tx = ViewerTx::spawn(stream.try_clone()?)?;
        Ok(DisplayServer { stream, tx, clip_latest: Arc::new(Mutex::new(None)) })
    }

    /// Create the framebuffer and hand it to the viewer.
    pub fn sink(&self) -> std::io::Result<Arc<FbSink>> {
        let fb = SharedFb::create().map_err(|e| std::io::Error::other(e.to_string()))?;
        send_fd(&self.stream, fb.fd().as_raw_fd()).map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(Arc::new(FbSink {
            fb,
            mode: Mutex::new((0, 0)),
            gpu_active: AtomicBool::new(false),
            tx: Some(self.tx.clone()),
        }))
    }

    pub fn clipboard(&self) -> Box<dyn ClipboardProvider> {
        Box::new(RemoteClipboard { latest: self.clip_latest.clone(), tx: self.tx.clone() })
    }

    pub fn tx(&self) -> ViewerTx {
        self.tx.clone()
    }

    /// Start reading viewer input.
    pub fn run(
        self,
        gpu: Option<GpuControl>,
        input: Option<InputPipes>,
        agent: Agent,
        events: Sender<VmEvent>,
    ) -> std::io::Result<()> {
        let mut stream = self.stream;
        let latest = self.clip_latest;
        std::thread::Builder::new().name("display".into()).spawn(move || {
            let mut dec = Decoder::default();
            let mut buf = vec![0u8; 64 << 10];
            loop {
                let n = match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                dec.push(&buf[..n]);
                loop {
                    match dec.next_to_vmm() {
                        Ok(Some(m)) => handle(m, gpu.as_ref(), input.as_ref(), &agent, &events, &latest),
                        Ok(None) => break,
                        Err(e) => {
                            log::warn!("display: {e}");
                            break;
                        }
                    }
                }
            }
            log::info!("viewer disconnected");
            let _ = events.send(VmEvent::ViewerClosed);
        })?;
        Ok(())
    }
}

fn send_events(sock: &Option<UnixDatagram>, evs: &[[u8; 8]]) {
    let Some(sock) = sock else { return };
    let mut b = Vec::with_capacity(evs.len() * 8);
    for e in evs {
        b.extend_from_slice(e);
    }
    let _ = sock.send(&b);
}

fn handle(
    m: ToVmm,
    gpu: Option<&GpuControl>,
    input: Option<&InputPipes>,
    agent: &Agent,
    events: &Sender<VmEvent>,
    latest: &Arc<Mutex<Option<String>>>,
) {
    let syn = event(ev::EV_SYN, 0, 0);
    let none = InputPipes::none();
    let input = input.unwrap_or(&none);
    if let (Some(usb), Some(ev)) = (&input.usb, to_hid(&m)) {
        usb.lock().unwrap_or_else(|e| e.into_inner()).input(ev);
        return;
    }
    match m {
        ToVmm::Key { code, down } => {
            if code < 249 {
                send_events(&input.kbd, &[event(ev::EV_KEY, code, down as i32), syn]);
            }
        }
        ToVmm::Pointer { x, y } => send_events(
            &input.tablet,
            &[event(ev::EV_ABS, ev::ABS_X, x as i32), event(ev::EV_ABS, ev::ABS_Y, y as i32), syn],
        ),
        ToVmm::Button { code, down } => {
            if matches!(code, ev::BTN_LEFT | ev::BTN_RIGHT | ev::BTN_MIDDLE) {
                send_events(&input.tablet, &[event(ev::EV_KEY, code, down as i32), syn]);
            }
        }
        ToVmm::Wheel { dx, dy } => {
            let mut e = Vec::new();
            if dy != 0 {
                e.push(event(ev::EV_REL, ev::REL_WHEEL, dy as i32));
            }
            if dx != 0 {
                e.push(event(ev::EV_REL, ev::REL_HWHEEL, dx as i32));
            }
            e.push(syn);
            send_events(&input.tablet, &e);
        }
        ToVmm::Resize { width, height } => {
            if let Some(g) = gpu {
                g.resize(width, height);
            }
            agent.send(AgentCmd::SetResolution(width, height));
        }
        ToVmm::Focus(f) => agent.send(AgentCmd::Focus(f)),
        ToVmm::Clipboard(t) => *latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(t),
        ToVmm::Command(c) => {
            let _ = events.send(VmEvent::Ui(c));
        }
    }
}

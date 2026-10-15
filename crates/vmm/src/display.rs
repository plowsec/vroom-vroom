//! VMM side of the display channel.
//!
//! Owns the shared framebuffer and the socket to the viewer. Guest frames
//! come in through [`FbSink`] (virtio-gpu), viewer input goes out to the
//! virtio-input devices, resize, focus + clipboard to the agent (which
//! also drives the GPU's size), lifecycle commands to the main loop.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vroom_display::{send_fd, Decoder, SharedFb, ToViewer, ToVmm};
use vroom_devices::usb::hid::HidEvent;
use vroom_devices::usb::xhci::Xhci;
use vroom_virtio::devices::gpu::DisplaySink;
use vroom_virtio::devices::input::{self as ev, event};

use crate::agent::{Agent, AgentCmd, ClipboardProvider};
use crate::vcpu::VmEvent;

/// Sends to the viewer without ever blocking the caller.
///
/// A dedicated thread does the (blocking) socket writes. Damage is
/// coalesced into one rect (the union of everything since the viewer was
/// last told), so a viewer that stops reading costs us at most a handful
/// of queued messages, never a stall, and one that keeps up only converts
/// what changed.
#[derive(Clone)]
pub(crate) struct ViewerTx {
    q: std::sync::mpsc::Sender<ToViewer>,
    /// x0, y0, x1, y1 not yet sent; Some means a marker is queued.
    damage: Arc<Mutex<Option<[u32; 4]>>>,
}

impl ViewerTx {
    fn spawn(mut sock: UnixStream) -> std::io::Result<Self> {
        let (q, rx) = std::sync::mpsc::channel::<ToViewer>();
        let damage = Arc::new(Mutex::new(None::<[u32; 4]>));
        let pending = damage.clone();
        std::thread::Builder::new().name("viewer-tx".into()).spawn(move || {
            while let Ok(mut m) = rx.recv() {
                if matches!(m, ToViewer::Damage { .. }) {
                    let Some([x0, y0, x1, y1]) = pending.lock().unwrap_or_else(|e| e.into_inner()).take() else { continue };
                    m = ToViewer::Damage { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
                }
                if sock.write_all(&m.encode()).is_err() {
                    break;
                }
            }
        })?;
        Ok(ViewerTx { q, damage })
    }

    pub fn send(&self, m: &ToViewer) {
        if let ToViewer::Damage { x, y, w, h } = *m {
            let r = [x, y, x.saturating_add(w), y.saturating_add(h)];
            let mut d = self.damage.lock().unwrap_or_else(|e| e.into_inner());
            match d.as_mut() {
                Some(u) => *u = union(*u, r),
                None => {
                    *d = Some(r);
                    // The rect is filled in when it goes out.
                    let _ = self.q.send(ToViewer::Damage { x: 0, y: 0, w: 0, h: 0 });
                }
            }
            return;
        }
        let _ = self.q.send(m.clone());
    }
}

fn union(a: [u32; 4], b: [u32; 4]) -> [u32; 4] {
    [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]
}

/// Two sources can feed the screen: ramfb (firmware, boot, Windows before
/// its virtio-gpu driver) and virtio-gpu. Windows with both drivers shows
/// two monitors and may well keep its desktop on the ramfb one, so neither
/// source simply wins: the one that changed most recently does, once the
/// one on screen has been quiet for `HOLD`. GPU updates are partial, so we
/// keep a copy of its picture to redraw from when it takes over again.
#[derive(Default)]
struct Arbiter {
    gpu_on_screen: bool,
    gpu_mode: (u32, u32),
    gpu_shadow: Vec<u8>,
    last_gpu: Option<Instant>,
    last_ramfb: Option<Instant>,
}

const HOLD: Duration = Duration::from_secs(3);

fn quiet(t: Option<Instant>, now: Instant) -> bool {
    t.is_none_or(|t| now.duration_since(t) >= HOLD)
}

pub(crate) struct FbSink {
    fb: SharedFb,
    mode: Mutex<(u32, u32)>,
    arb: Mutex<Arbiter>,
    /// None when headless: frames still land in the buffer (screendumps).
    tx: Option<ViewerTx>,
}

impl FbSink {
    pub fn headless() -> std::io::Result<Arc<Self>> {
        let fb = SharedFb::create().map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(Arc::new(FbSink { fb, mode: Mutex::new((0, 0)), arb: Mutex::default(), tx: None }))
    }

    /// A full frame from ramfb (sent only when it changed).
    pub fn ramfb_frame(&self, w: u32, h: u32, rows: &[u8], stride: usize) {
        self.ramfb_frame_at(Instant::now(), w, h, rows, stride)
    }

    fn ramfb_frame_at(&self, now: Instant, w: u32, h: u32, rows: &[u8], stride: usize) {
        let mut a = self.arb.lock().unwrap_or_else(|e| e.into_inner());
        a.last_ramfb = Some(now);
        if a.gpu_on_screen {
            if !quiet(a.last_gpu, now) {
                return;
            }
            log::info!("display: showing the firmware framebuffer (virtio-gpu is idle)");
            a.gpu_on_screen = false;
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
        log::debug!("display: mode {width}x{height}");
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
        let mut a = self.arb.lock().unwrap_or_else(|e| e.into_inner());
        a.gpu_mode = (width, height);
        a.gpu_shadow = vec![0; width as usize * height as usize * 4];
        a.gpu_on_screen = width != 0;
        a.last_gpu = Some(Instant::now());
        self.apply_mode(width, height);
    }

    fn flush(&self, x: u32, y: u32, w: u32, h: u32, rows: &[u8], stride: usize) {
        self.flush_at(Instant::now(), x, y, w, h, rows, stride)
    }
}

impl FbSink {
    #[allow(clippy::too_many_arguments)]
    fn flush_at(&self, now: Instant, x: u32, y: u32, w: u32, h: u32, rows: &[u8], stride: usize) {
        let mut a = self.arb.lock().unwrap_or_else(|e| e.into_inner());
        let (mw, mh) = a.gpu_mode;
        if x.saturating_add(w) > mw || y.saturating_add(h) > mh {
            return;
        }
        // Keep the GPU picture whether or not it is on screen.
        let row = w as usize * 4;
        for i in 0..h as usize {
            let (Some(src), d) = (rows.get(i * stride..i * stride + row), ((y as usize + i) * mw as usize + x as usize) * 4) else { return };
            a.gpu_shadow[d..d + row].copy_from_slice(src);
        }
        a.last_gpu = Some(now);
        if a.gpu_on_screen {
            self.draw(x, y, w, h, rows, stride);
        } else if quiet(a.last_ramfb, now) {
            log::info!("display: showing virtio-gpu again");
            a.gpu_on_screen = true;
            self.apply_mode(mw, mh);
            self.draw(0, 0, mw, mh, &a.gpu_shadow, mw as usize * 4);
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
            arb: Mutex::default(),
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
                        Ok(Some(m)) => handle(m, input.as_ref(), &agent, &events, &latest),
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
        ToVmm::Resize { width, height, scale } => {
            log::debug!("display: viewer asks for {width}x{height} at {scale}%");
            agent.send(AgentCmd::Display { width, height, scale });
        }
        ToVmm::Focus(f) => agent.send(AgentCmd::Focus(f)),
        ToVmm::Clipboard(t) => *latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(t),
        ToVmm::Command(c) => {
            let _ = events.send(VmEvent::Ui(c));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(sink: &FbSink) -> u32 {
        let (w, h) = *sink.mode.lock().unwrap();
        let mut v = vec![0u32; (w * h) as usize];
        sink.fb.read_into(w, h, &mut v);
        v[0]
    }

    #[test]
    fn damage_is_merged_not_lost() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let tx = ViewerTx::spawn(a).unwrap();
        for (x, y) in [(10, 20), (300, 5), (40, 400)] {
            tx.send(&ToViewer::Damage { x, y, w: 8, h: 8 });
        }
        tx.send(&ToViewer::Closed);
        let mut d = vroom_display::Decoder::default();
        let mut got: Option<[u32; 4]> = None;
        let mut buf = [0u8; 256];
        b.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        'read: loop {
            let n = b.read(&mut buf).unwrap();
            d.push(&buf[..n]);
            while let Some(m) = d.next_to_viewer().unwrap() {
                match m {
                    ToViewer::Damage { x, y, w, h } => {
                        let r = [x, y, x + w, y + h];
                        got = Some(got.map_or(r, |u| union(u, r)));
                    }
                    ToViewer::Closed => break 'read,
                    _ => {}
                }
            }
        }
        assert_eq!(got, Some([10, 5, 308, 408]));
    }

    #[test]
    fn the_source_that_changes_wins_after_a_quiet_spell() {
        let s = FbSink::headless().unwrap();
        let t0 = Instant::now();
        let (ramfb, gpu) = ([0x11u8; 4 * 4 * 2], [0x22u8; 4 * 4 * 2]);
        s.ramfb_frame_at(t0, 4, 2, &ramfb, 16);
        assert_eq!(px(&s), 0x11111111);
        // Driver comes up: GPU takes over at once.
        s.set_mode(4, 2);
        s.flush_at(t0, 0, 0, 4, 2, &gpu, 16);
        assert_eq!(px(&s), 0x22222222);
        // Firmware screen changes while the GPU is busy: stays on GPU.
        s.ramfb_frame_at(t0 + Duration::from_secs(1), 4, 2, &ramfb, 16);
        assert_eq!(px(&s), 0x22222222);
        // GPU quiet for a while, ramfb changes: show ramfb.
        s.ramfb_frame_at(t0 + Duration::from_secs(5), 4, 2, &ramfb, 16);
        assert_eq!(px(&s), 0x11111111);
        // A partial GPU update right after: ramfb is not quiet yet.
        s.flush_at(t0 + Duration::from_secs(6), 0, 0, 1, 1, &[0x33; 4], 4);
        assert_eq!(px(&s), 0x11111111);
        // Later: GPU back, redrawn from its copy including that update.
        s.flush_at(t0 + Duration::from_secs(10), 1, 0, 1, 1, &[0x44; 4], 4);
        assert_eq!(px(&s), 0x33333333);
    }
}

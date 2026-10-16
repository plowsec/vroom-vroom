//! Framed messages: type u8 | len u32 LE | payload.

use crate::{Error, Result};
pub use vroom_proto::{GuestWindow, WindowOp};

/// Absolute pointer range (like a USB tablet).
pub const POINTER_MAX: u16 = 32767;
/// Window scales a resize may carry, in percent (0 = none).
pub const SCALE_MIN: u32 = 100;
pub const SCALE_MAX: u32 = 500;
const MAX_MSG: usize = 16 << 20;
/// Most names a drag may carry.
pub const DRAG_ITEMS_MAX: usize = 4096;
/// Biggest guest pointer picture, in pixels on a side.
pub const CURSOR_MAX: u32 = 256;

/// VMM -> viewer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToViewer {
    /// Scanout mode changed (fb is tightly packed: stride = width*4).
    Mode { width: u32, height: u32 },
    /// Pixels changed in this rect.
    Damage { x: u32, y: u32, w: u32, h: u32 },
    /// Put this on the host clipboard.
    ClipboardSet(String),
    /// Human-readable status ("running", "paused", "guest agent connected").
    Status(String),
    /// VM is gone; viewer should close.
    Closed,
    /// The guest is recording from the host microphone (true) or stopped.
    Mic(bool),
    /// The guest's pointer, for the viewer to show as its own.
    Cursor(GuestCursor),
    /// The guest is ready for files dragged over the window (`DragEnter`).
    /// `live`: it runs a drag of its own under the pointer, which the
    /// viewer drives with the mouse button; else the files go to a folder.
    DragReady { id: u32, live: bool },
    /// The dropped files are in the guest (`ok`), or why not. `place`:
    /// where they went, without a live drag.
    DropDone { id: u32, ok: bool, place: String },
    /// The guest agent said hello: what it can do (`vroom_proto::features`).
    Agent { features: u64 },
    /// Coherence: the guest's windows, front to back (checked like the
    /// agent's message; the viewer still clips them to the screen).
    Windows(Vec<GuestWindow>),
}

/// What the guest's pointer looks like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestCursor {
    /// The guest gives none: it draws its own into the frame, or has none.
    Unset,
    /// It hid it.
    Hidden,
    /// `w` x `h` RGBA (not premultiplied), at most `CURSOR_MAX` on a side;
    /// the hot spot is inside it.
    Image { w: u32, h: u32, hot_x: u32, hot_y: u32, rgba: Vec<u8> },
}

/// Lifecycle requests from the viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Pause,
    Resume,
    Snapshot,
    Reset,
    /// Ask the guest to shut down cleanly (agent / power button).
    Shutdown,
    /// Pull the plug.
    PowerOff,
    Suspend,
}

/// Viewer -> VMM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToVmm {
    /// Linux evdev key code.
    Key { code: u16, down: bool },
    /// Absolute pointer, 0..=POINTER_MAX on both axes.
    Pointer { x: u16, y: u16 },
    /// evdev button code (BTN_LEFT = 0x110 ...).
    Button { code: u16, down: bool },
    Wheel { dx: i16, dy: i16 },
    /// Window content size; guests that can follow will. `scale` 0: in
    /// logical pixels. Else (HiDPI) in physical pixels, and the window's
    /// scale in percent (200 on Retina), for the guest's UI.
    Resize { width: u32, height: u32, scale: u32 },
    Focus(bool),
    /// Host clipboard changed (only sent while focused).
    Clipboard(String),
    Command(Command),
    /// Files are dragged over the window: their top-level names, and
    /// whether each is a folder. Nothing else goes until they are dropped.
    DragEnter { id: u32, items: Vec<(String, bool)> },
    /// The drag left the window, or the drop was given up.
    DragCancel { id: u32 },
    /// Dropped: comes with a pipe (SCM_RIGHTS) the viewer writes the
    /// files into, as guest agent frames (DropEntry, DropData, DropEnd).
    Drop { id: u32 },
    /// Coherence on or off (the guest agent shows and hides its taskbar).
    Coherence(bool),
    /// Coherence: do this to a guest window.
    Window { id: u32, op: WindowOp },
}

fn frame(ty: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + payload.len());
    v.push(ty);
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(payload);
    v
}

fn u32s(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn get_u32(p: &[u8], i: usize) -> Result<u32> {
    let b = p.get(i * 4..i * 4 + 4).ok_or(Error::Proto("short"))?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn text(p: &[u8]) -> Result<String> {
    std::str::from_utf8(p).map(|s| s.to_string()).map_err(|_| Error::Proto("utf-8"))
}

impl ToViewer {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ToViewer::Mode { width, height } => frame(1, &u32s(&[*width, *height])),
            ToViewer::Damage { x, y, w, h } => frame(2, &u32s(&[*x, *y, *w, *h])),
            ToViewer::ClipboardSet(s) => frame(3, s.as_bytes()),
            ToViewer::Status(s) => frame(4, s.as_bytes()),
            ToViewer::Closed => frame(5, &[]),
            ToViewer::Mic(on) => frame(6, &[*on as u8]),
            ToViewer::Cursor(GuestCursor::Unset) => frame(7, &[0]),
            ToViewer::Cursor(GuestCursor::Hidden) => frame(7, &[1]),
            ToViewer::Cursor(GuestCursor::Image { w, h, hot_x, hot_y, rgba }) => {
                let mut p = vec![2];
                p.extend_from_slice(&u32s(&[*w, *h, *hot_x, *hot_y]));
                p.extend_from_slice(rgba);
                frame(7, &p)
            }
            ToViewer::DragReady { id, live } => {
                let mut p = id.to_le_bytes().to_vec();
                p.push(*live as u8);
                frame(8, &p)
            }
            ToViewer::DropDone { id, ok, place } => {
                let mut p = id.to_le_bytes().to_vec();
                p.push(*ok as u8);
                p.extend_from_slice(place.as_bytes());
                frame(9, &p)
            }
            ToViewer::Agent { features } => frame(10, &features.to_le_bytes()),
            ToViewer::Windows(list) => frame(11, &vroom_proto::encode_windows(list)),
        }
    }

    fn decode(ty: u8, p: &[u8]) -> Result<Self> {
        Ok(match ty {
            1 => ToViewer::Mode { width: get_u32(p, 0)?, height: get_u32(p, 1)? },
            2 => ToViewer::Damage { x: get_u32(p, 0)?, y: get_u32(p, 1)?, w: get_u32(p, 2)?, h: get_u32(p, 3)? },
            3 => ToViewer::ClipboardSet(text(p)?),
            4 => ToViewer::Status(text(p)?),
            5 => ToViewer::Closed,
            6 => ToViewer::Mic(*p.first().ok_or(Error::Proto("short"))? != 0),
            7 => ToViewer::Cursor(match p.first() {
                Some(0) => GuestCursor::Unset,
                Some(1) => GuestCursor::Hidden,
                Some(2) => {
                    let q = &p[1..];
                    let (w, h, hot_x, hot_y) = (get_u32(q, 0)?, get_u32(q, 1)?, get_u32(q, 2)?, get_u32(q, 3)?);
                    if w == 0 || h == 0 || w > CURSOR_MAX || h > CURSOR_MAX || hot_x >= w || hot_y >= h {
                        return Err(Error::Proto("cursor size"));
                    }
                    let rgba = &q[16..];
                    if rgba.len() != w as usize * h as usize * 4 {
                        return Err(Error::Proto("cursor size"));
                    }
                    GuestCursor::Image { w, h, hot_x, hot_y, rgba: rgba.to_vec() }
                }
                _ => return Err(Error::Proto("unknown cursor")),
            }),
            8 if p.len() == 5 => ToViewer::DragReady { id: get_u32(p, 0)?, live: p[4] != 0 },
            9 if p.len() >= 5 => ToViewer::DropDone { id: get_u32(p, 0)?, ok: p[4] != 0, place: text(&p[5..])?.chars().take(256).collect() },
            10 if p.len() == 8 => ToViewer::Agent { features: u64::from(get_u32(p, 0)?) | u64::from(get_u32(p, 1)?) << 32 },
            11 => ToViewer::Windows(vroom_proto::decode_windows(p).ok_or(Error::Proto("windows"))?),
            _ => return Err(Error::Proto("unknown viewer message")),
        })
    }
}

impl ToVmm {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ToVmm::Key { code, down } => frame(1, &u32s(&[*code as u32, *down as u32])),
            ToVmm::Pointer { x, y } => frame(2, &u32s(&[*x as u32, *y as u32])),
            ToVmm::Button { code, down } => frame(3, &u32s(&[*code as u32, *down as u32])),
            ToVmm::Wheel { dx, dy } => frame(4, &u32s(&[*dx as u16 as u32, *dy as u16 as u32])),
            ToVmm::Resize { width, height, scale } => frame(5, &u32s(&[*width, *height, *scale])),
            ToVmm::Focus(f) => frame(6, &[*f as u8]),
            ToVmm::Clipboard(s) => frame(7, s.as_bytes()),
            ToVmm::Command(c) => frame(8, &[*c as u8]),
            ToVmm::DragEnter { id, items } => {
                let mut p = id.to_le_bytes().to_vec();
                for (name, dir) in items {
                    p.push(*dir as u8);
                    p.extend_from_slice(&(name.len() as u16).to_le_bytes());
                    p.extend_from_slice(name.as_bytes());
                }
                frame(9, &p)
            }
            ToVmm::DragCancel { id } => frame(10, &id.to_le_bytes()),
            ToVmm::Drop { id } => frame(11, &id.to_le_bytes()),
            ToVmm::Coherence(on) => frame(12, &[*on as u8]),
            ToVmm::Window { id, op } => {
                let mut p = id.to_le_bytes().to_vec();
                p.extend_from_slice(&op.encode());
                frame(13, &p)
            }
        }
    }

    fn decode(ty: u8, p: &[u8]) -> Result<Self> {
        let small = |v: u32| u16::try_from(v).map_err(|_| Error::Proto("range"));
        Ok(match ty {
            1 => ToVmm::Key { code: small(get_u32(p, 0)?)?, down: get_u32(p, 1)? != 0 },
            2 => {
                let (x, y) = (small(get_u32(p, 0)?)?, small(get_u32(p, 1)?)?);
                ToVmm::Pointer { x: x.min(POINTER_MAX), y: y.min(POINTER_MAX) }
            }
            3 => ToVmm::Button { code: small(get_u32(p, 0)?)?, down: get_u32(p, 1)? != 0 },
            4 => ToVmm::Wheel { dx: get_u32(p, 0)? as u16 as i16, dy: get_u32(p, 1)? as u16 as i16 },
            5 => {
                // The scale is optional (older viewers, scripts): 0.
                let scale = if p.len() >= 12 { get_u32(p, 2)? } else { 0 };
                if scale != 0 && !(SCALE_MIN..=SCALE_MAX).contains(&scale) {
                    return Err(Error::Proto("scale"));
                }
                ToVmm::Resize { width: get_u32(p, 0)?, height: get_u32(p, 1)?, scale }
            }
            6 => ToVmm::Focus(*p.first().ok_or(Error::Proto("short"))? != 0),
            7 => ToVmm::Clipboard(text(p)?),
            8 => ToVmm::Command(match p.first() {
                Some(0) => Command::Pause,
                Some(1) => Command::Resume,
                Some(2) => Command::Snapshot,
                Some(3) => Command::Reset,
                Some(4) => Command::Shutdown,
                Some(5) => Command::PowerOff,
                Some(6) => Command::Suspend,
                _ => return Err(Error::Proto("unknown command")),
            }),
            9 => {
                let id = get_u32(p, 0)?;
                let mut items = Vec::new();
                let mut o = 4;
                while o < p.len() {
                    let len = u16::from_le_bytes(p.get(o + 1..o + 3).ok_or(Error::Proto("short"))?.try_into().map_err(|_| Error::Proto("short"))?) as usize;
                    let name = text(p.get(o + 3..o + 3 + len).ok_or(Error::Proto("short"))?)?;
                    items.push((name, p[o] != 0));
                    o += 3 + len;
                }
                if items.is_empty() || items.len() > DRAG_ITEMS_MAX {
                    return Err(Error::Proto("drag items"));
                }
                ToVmm::DragEnter { id, items }
            }
            10 if p.len() == 4 => ToVmm::DragCancel { id: get_u32(p, 0)? },
            11 if p.len() == 4 => ToVmm::Drop { id: get_u32(p, 0)? },
            12 if p.len() == 1 => ToVmm::Coherence(p[0] != 0),
            13 if p.len() > 4 => ToVmm::Window { id: get_u32(p, 0)?, op: WindowOp::decode(&p[4..]).ok_or(Error::Proto("window op"))? },
            _ => return Err(Error::Proto("unknown vmm message")),
        })
    }
}

/// Stream decoder, generic over direction.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn push(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    fn next_raw(&mut self) -> Result<Option<(u8, Vec<u8>)>> {
        if self.buf.len() < 5 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if len > MAX_MSG {
            // Unrecoverable framing: drop everything.
            self.buf.clear();
            return Err(Error::Proto("message too large"));
        }
        if self.buf.len() < 5 + len {
            return Ok(None);
        }
        let ty = self.buf[0];
        let p = self.buf[5..5 + len].to_vec();
        self.buf.drain(..5 + len);
        Ok(Some((ty, p)))
    }

    pub fn next_to_vmm(&mut self) -> Result<Option<ToVmm>> {
        match self.next_raw()? {
            Some((t, p)) => ToVmm::decode(t, &p).map(Some),
            None => Ok(None),
        }
    }

    pub fn next_to_viewer(&mut self) -> Result<Option<ToViewer>> {
        match self.next_raw()? {
            Some((t, p)) => ToViewer::decode(t, &p).map(Some),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let vm = [
            ToVmm::Key { code: 30, down: true },
            ToVmm::Pointer { x: 100, y: 32767 },
            ToVmm::Button { code: 0x110, down: false },
            ToVmm::Wheel { dx: -1, dy: 3 },
            ToVmm::Resize { width: 1920, height: 1080, scale: 0 },
            ToVmm::Resize { width: 3456, height: 2234, scale: 200 },
            ToVmm::Focus(true),
            ToVmm::Clipboard("x".into()),
            ToVmm::Command(Command::Snapshot),
            ToVmm::DragEnter { id: 4, items: vec![("a.txt".into(), false), ("dir".into(), true)] },
            ToVmm::DragCancel { id: 4 },
            ToVmm::Drop { id: 4 },
            ToVmm::Coherence(true),
            ToVmm::Window { id: 9, op: WindowOp::Activate },
            ToVmm::Window { id: 9, op: WindowOp::Move { x: -3, y: 4, w: 100, h: 50 } },
        ];
        let mut d = Decoder::default();
        for m in &vm {
            d.push(&m.encode());
        }
        for m in &vm {
            assert_eq!(d.next_to_vmm().unwrap().as_ref(), Some(m));
        }
        let v = [
            ToViewer::Mode { width: 800, height: 600 },
            ToViewer::Damage { x: 1, y: 2, w: 3, h: 4 },
            ToViewer::Closed,
            ToViewer::Mic(true),
            ToViewer::Cursor(GuestCursor::Hidden),
            ToViewer::Cursor(GuestCursor::Image { w: 2, h: 1, hot_x: 1, hot_y: 0, rgba: vec![1, 2, 3, 4, 5, 6, 7, 8] }),
            ToViewer::Cursor(GuestCursor::Unset),
            ToViewer::DragReady { id: 4, live: true },
            ToViewer::DropDone { id: 4, ok: false, place: "Desktop".into() },
            ToViewer::Agent { features: 1 << 40 | 128 },
            ToViewer::Windows(vec![GuestWindow { id: 1, x: 0, y: -2, w: 3, h: 4, flags: 4, title: "Paint".into() }]),
        ];
        for m in &v {
            d.push(&m.encode());
        }
        for m in &v {
            assert_eq!(d.next_to_viewer().unwrap().as_ref(), Some(m));
        }
    }

    #[test]
    fn hostile_lengths() {
        let mut d = Decoder::default();
        d.push(&[1, 0xff, 0xff, 0xff, 0xff]);
        assert!(d.next_to_vmm().is_err());
        d.push(&[2, 4, 0, 0, 0, 1, 2, 3, 4]); // pointer with 1 field
        assert!(d.next_to_vmm().is_err());
        // resize without a scale: logical pixels
        d.push(&[5, 8, 0, 0, 0, 0, 4, 0, 0, 0, 3, 0, 0]);
        assert_eq!(d.next_to_vmm().unwrap(), Some(ToVmm::Resize { width: 1024, height: 768, scale: 0 }));
        d.push(&ToVmm::Resize { width: 1, height: 1, scale: 99 }.encode());
        assert!(d.next_to_vmm().is_err());
        // Cursors: pixels must match the size, the hot spot be inside.
        let img = |w: u32, h: u32, hx: u32, n: usize| {
            let mut p = vec![2u8];
            p.extend(u32s(&[w, h, hx, 0]));
            p.extend(vec![0u8; n]);
            frame(7, &p)
        };
        d.push(&img(2, 2, 0, 15));
        assert!(d.next_to_viewer().is_err());
        d.push(&img(2, 2, 2, 16));
        assert!(d.next_to_viewer().is_err());
        d.push(&img(CURSOR_MAX + 1, 1, 0, (CURSOR_MAX as usize + 1) * 4));
        assert!(d.next_to_viewer().is_err());
        d.push(&frame(7, &[2, 1, 0]));
        assert!(d.next_to_viewer().is_err());
        d.push(&img(2, 2, 1, 16));
        assert!(matches!(d.next_to_viewer().unwrap(), Some(ToViewer::Cursor(GuestCursor::Image { w: 2, h: 2, .. }))));
    }
}

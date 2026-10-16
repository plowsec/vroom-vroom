//! Host <-> guest agent protocol.
//!
//! Byte stream (virtio-console port), framed:
//! ```text
//! "VR" | version u8 | type u8 | len u32 LE | payload[len]
//! ```
//! Both sides treat the other as hostile: lengths are capped, strings are
//! validated UTF-8, unknown types are errors. On any error the receiver
//! drops its buffer and resyncs on the next magic.
#![forbid(unsafe_code)]

pub const MAGIC: [u8; 2] = *b"VR";
pub const VERSION: u8 = 1;
const HDR: usize = 8;
/// Absolute payload cap. Clipboard policy caps lower.
pub const MAX_PAYLOAD: usize = 64 << 20;
/// Port name the guest agent opens.
pub const AGENT_PORT: &str = "org.vroom.agent.0";

/// Feature bits for Hello.
pub mod features {
    pub const CLIPBOARD_TEXT: u64 = 1;
    pub const TIME_SYNC: u64 = 2;
    pub const RESOLUTION: u64 = 4;
    pub const SHUTDOWN: u64 = 8;
    /// The agent can change the desktop's UI scale (HiDPI host windows).
    pub const SCALE: u64 = 16;
    /// The agent can turn the 3D display device off and on (Windows:
    /// Direct3D state can't be saved, so a suspend parks it).
    pub const GPU3D: u64 = 32;
    /// Files dropped on the VM window land in the guest (`DragEnter`...).
    pub const FILE_DROP: u64 = 64;
    /// Files dragged out of the guest's desktop reach the Mac
    /// (`DragOutQuery`...).
    pub const DRAG_OUT: u64 = 256;
}

/// Longest relative path a drop may carry, and longest name in it.
pub const DROP_PATH_MAX: usize = 1024;
pub const DROP_NAME_MAX: usize = 255;
/// Most top-level items in one drop.
pub const DROP_ITEMS_MAX: usize = 4096;
/// File data goes in pieces of at most this.
pub const DROP_CHUNK: usize = 256 << 10;
/// Most files and folders one dragged item may hold.
pub const DROP_ENTRIES_MAX: usize = 200_000;

/// UI scales a host may ask for, in percent. 0 = "the host no longer
/// manages the scale": the agent puts back what the guest had before.
pub const SCALE_MIN: u32 = 100;
pub const SCALE_MAX: u32 = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Hello { features: u64, agent: String },
    /// Clipboard now holds this text (either direction).
    ClipboardText(String),
    /// Host -> guest: set the clock (after resume from snapshot).
    TimeSync { unix_secs: u64, nanos: u32 },
    /// Host -> guest: window resized; change the display mode.
    SetResolution { width: u32, height: u32 },
    /// Host -> guest: UI scale in percent (100 = 1x, 200 = 2x), or 0 to
    /// undo ours. Sent after SetResolution.
    SetScale { percent: u32 },
    /// Host -> guest: please shut down / reboot cleanly.
    Shutdown { reboot: bool },
    Ping(u64),
    Pong(u64),
    /// Host -> guest: turn the 3D display device off (before a suspend)
    /// or on. Guest -> host: whether it is on now.
    Gpu3d { on: bool },
    /// Host -> guest: files are being dragged over the VM window (names
    /// only: nothing is sent unless they are dropped). `items`: top-level
    /// names, and whether each is a folder. The guest answers DragReady.
    DragEnter { id: u32, items: Vec<(String, bool)> },
    /// Guest -> host: ready for the drop. `live`: the guest runs a drag of
    /// its own under the pointer (Windows), which the host drives with
    /// the mouse button; otherwise the files land in a folder.
    DragReady { id: u32, live: bool },
    /// Host -> guest: the drag left the window or was given up.
    DragCancel { id: u32 },
    /// Host -> guest, after a drop: one file or folder (relative path,
    /// '/' between names), then its data in DropData, in order.
    DropEntry { id: u32, path: String, dir: bool, size: u64 },
    DropData { id: u32, data: Vec<u8> },
    /// Host -> guest: that was all of it.
    DropEnd { id: u32 },
    /// Guest -> host: the files are written. `place`: where they went
    /// when there was no live drag ("Desktop"), or why it failed.
    DropDone { id: u32, ok: bool, place: String },
    /// Host -> guest: the mouse left the VM window with the button held.
    /// If the guest's desktop is dragging files, the agent says which
    /// (DragOutItems), and nothing else until the Mac drop asks.
    DragOutQuery { id: u32 },
    /// Guest -> host: the files being dragged (top-level names, and
    /// whether each is a folder); none if it isn't a drag of files.
    DragOutItems { id: u32, items: Vec<(String, bool)> },
    /// Host -> guest: the Mac drop wants item `item` (an index into
    /// DragOutItems) of drag `id`. The guest sends it as DropEntry,
    /// DropData and DropEnd frames whose id is `stream` (the item first,
    /// then what is in it, folders before their contents), or
    /// DragOutFail. A new stream number for each, so the frames of one
    /// the host gave up on can't pass for the next. One at a time: a new
    /// DragOutGet stops the one underway. The item's own DropEntry, when
    /// it is a folder, has the total size of its files (for progress).
    DragOutGet { id: u32, item: u32, stream: u32 },
    /// Host -> guest: the drag is over; stop sending, forget it.
    DragOutEnd { id: u32 },
    /// Guest -> host: DragOutGet `stream` can't be done (a file can't be
    /// read).
    DragOutFail { stream: u32, why: String },
}

/// A name `drop_name_ok` takes, made from any file name: what Windows or
/// a shell would read another way becomes `_` (macOS and Linux allow `:`
/// and `?` in names, Windows doesn't), control characters go, long names
/// are cut.
pub fn fix_name(n: &str) -> String {
    let mut s: String = n
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    if s.len() > DROP_NAME_MAX {
        let mut end = DROP_NAME_MAX;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    if s.is_empty() || s == "." || s == ".." {
        s = "_".into();
    }
    s
}

/// Checks one dragged item as its frames come in (DropEntry, DropData,
/// DropEnd, in order), on the side that receives it: the item itself
/// first, then what is in it, each entry under a folder announced before
/// it; data only for the file just announced and never more than it
/// said; at most `DROP_ENTRIES_MAX` entries. Guest -> host drags run it
/// twice: in the VMM and in the app that writes the files.
#[derive(Debug)]
pub struct TreeCheck {
    id: u32,
    top: String,
    dir: bool,
    /// Folders announced so far (hashes: a hostile sender could make
    /// many long names).
    dirs: std::collections::HashSet<u64>,
    hasher: std::collections::hash_map::RandomState,
    /// Bytes the current file still owes.
    left: u64,
    entries: usize,
}

impl TreeCheck {
    /// For item `top` (a folder if `dir`) of drag `id`.
    pub fn new(id: u32, top: &str, dir: bool) -> TreeCheck {
        TreeCheck { id, top: top.to_string(), dir, dirs: Default::default(), hasher: Default::default(), left: 0, entries: 0 }
    }

    fn key(&self, p: &str) -> u64 {
        use std::hash::BuildHasher;
        self.hasher.hash_one(p)
    }

    /// Whether `m` may come next: Ok(true) when it was the end.
    pub fn check(&mut self, m: &Message) -> Result<bool, &'static str> {
        match m {
            Message::DropEntry { id, path, dir, size } if *id == self.id => {
                if self.left != 0 {
                    return Err("a file came short");
                }
                if self.entries >= DROP_ENTRIES_MAX {
                    return Err("too many files");
                }
                if self.entries == 0 {
                    if *path != self.top || *dir != self.dir {
                        return Err("not the item asked for");
                    }
                } else {
                    let parent = path.rsplit_once('/').map(|p| p.0).ok_or("not inside the item")?;
                    if !self.dirs.contains(&self.key(parent)) || self.dirs.contains(&self.key(path)) {
                        return Err("not inside the item");
                    }
                }
                self.entries += 1;
                if *dir {
                    let k = self.key(path);
                    self.dirs.insert(k);
                } else {
                    self.left = *size;
                }
                Ok(false)
            }
            Message::DropData { id, data } if *id == self.id && self.entries > 0 => {
                if data.len() as u64 > self.left {
                    return Err("more data than announced");
                }
                self.left -= data.len() as u64;
                Ok(false)
            }
            Message::DropEnd { id } if *id == self.id && self.entries > 0 => {
                if self.left != 0 {
                    return Err("a file came short");
                }
                Ok(true)
            }
            _ => Err("unexpected message"),
        }
    }
}

/// Whether `p` is a path a drop may carry: relative, '/' between names,
/// no empty, "." or ".." names, nothing Windows or a shell would read
/// another way.
pub fn drop_path_ok(p: &str) -> bool {
    p.len() <= DROP_PATH_MAX && !p.is_empty() && p.split('/').all(drop_name_ok)
}

/// One name of a drop path.
pub fn drop_name_ok(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= DROP_NAME_MAX
        && n != "."
        && n != ".."
        && !n.chars().any(|c| c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("bad magic")]
    Magic,
    #[error("unsupported version {0}")]
    Version(u8),
    #[error("unknown message type {0}")]
    Type(u8),
    #[error("payload too large ({0} bytes)")]
    TooLarge(usize),
    #[error("malformed payload")]
    Malformed,
}

const T_HELLO: u8 = 1;
const T_CLIP: u8 = 2;
const T_TIME: u8 = 3;
const T_RES: u8 = 4;
const T_SHUTDOWN: u8 = 5;
const T_PING: u8 = 6;
const T_PONG: u8 = 7;
const T_SCALE: u8 = 8;
const T_GPU3D: u8 = 9;
const T_DRAG_ENTER: u8 = 10;
const T_DRAG_READY: u8 = 11;
const T_DRAG_CANCEL: u8 = 12;
const T_DROP_ENTRY: u8 = 13;
const T_DROP_DATA: u8 = 14;
const T_DROP_END: u8 = 15;
const T_DROP_DONE: u8 = 16;
const T_DRAG_OUT_QUERY: u8 = 20;
const T_DRAG_OUT_ITEMS: u8 = 21;
const T_DRAG_OUT_GET: u8 = 22;
const T_DRAG_OUT_END: u8 = 23;
const T_DRAG_OUT_FAIL: u8 = 24;

fn put_items(p: &mut Vec<u8>, items: &[(String, bool)]) {
    p.extend_from_slice(&(items.len() as u32).to_le_bytes());
    for (name, dir) in items {
        p.push(*dir as u8);
        p.extend_from_slice(&(name.len() as u16).to_le_bytes());
        p.extend_from_slice(name.as_bytes());
    }
}

/// Items after the id: count, then dir u8 | len u16 | name each. Each
/// name must be one `drop_name_ok` takes; `min`: the fewest allowed.
fn get_items(p: &[u8], min: usize) -> Result<Vec<(String, bool)>, Error> {
    let n = u32::from_le_bytes(p.get(4..8).ok_or(Error::Malformed)?.try_into().map_err(|_| Error::Malformed)?) as usize;
    if n < min || n > DROP_ITEMS_MAX {
        return Err(Error::Malformed);
    }
    let mut items = Vec::with_capacity(n);
    let mut o = 8;
    for _ in 0..n {
        let dir = *p.get(o).ok_or(Error::Malformed)? != 0;
        let len = u16::from_le_bytes(p.get(o + 1..o + 3).ok_or(Error::Malformed)?.try_into().map_err(|_| Error::Malformed)?) as usize;
        let name = std::str::from_utf8(p.get(o + 3..o + 3 + len).ok_or(Error::Malformed)?).map_err(|_| Error::Malformed)?;
        if !drop_name_ok(name) {
            return Err(Error::Malformed);
        }
        items.push((name.to_string(), dir));
        o += 3 + len;
    }
    if o != p.len() {
        return Err(Error::Malformed);
    }
    Ok(items)
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        let (ty, payload) = match self {
            Message::Hello { features, agent } => {
                let mut p = features.to_le_bytes().to_vec();
                p.extend_from_slice(agent.as_bytes());
                (T_HELLO, p)
            }
            Message::ClipboardText(s) => (T_CLIP, s.as_bytes().to_vec()),
            Message::TimeSync { unix_secs, nanos } => {
                let mut p = unix_secs.to_le_bytes().to_vec();
                p.extend_from_slice(&nanos.to_le_bytes());
                (T_TIME, p)
            }
            Message::SetResolution { width, height } => {
                let mut p = width.to_le_bytes().to_vec();
                p.extend_from_slice(&height.to_le_bytes());
                (T_RES, p)
            }
            Message::SetScale { percent } => (T_SCALE, percent.to_le_bytes().to_vec()),
            Message::Shutdown { reboot } => (T_SHUTDOWN, vec![*reboot as u8]),
            Message::Ping(v) => (T_PING, v.to_le_bytes().to_vec()),
            Message::Pong(v) => (T_PONG, v.to_le_bytes().to_vec()),
            Message::Gpu3d { on } => (T_GPU3D, vec![*on as u8]),
            Message::DragEnter { id, items } => {
                let mut p = id.to_le_bytes().to_vec();
                put_items(&mut p, items);
                (T_DRAG_ENTER, p)
            }
            Message::DragReady { id, live } => {
                let mut p = id.to_le_bytes().to_vec();
                p.push(*live as u8);
                (T_DRAG_READY, p)
            }
            Message::DragCancel { id } => (T_DRAG_CANCEL, id.to_le_bytes().to_vec()),
            Message::DropEntry { id, path, dir, size } => {
                let mut p = id.to_le_bytes().to_vec();
                p.push(*dir as u8);
                p.extend_from_slice(&size.to_le_bytes());
                p.extend_from_slice(path.as_bytes());
                (T_DROP_ENTRY, p)
            }
            Message::DropData { id, data } => {
                let mut p = id.to_le_bytes().to_vec();
                p.extend_from_slice(data);
                (T_DROP_DATA, p)
            }
            Message::DropEnd { id } => (T_DROP_END, id.to_le_bytes().to_vec()),
            Message::DropDone { id, ok, place } => {
                let mut p = id.to_le_bytes().to_vec();
                p.push(*ok as u8);
                p.extend_from_slice(place.as_bytes());
                (T_DROP_DONE, p)
            }
            Message::DragOutQuery { id } => (T_DRAG_OUT_QUERY, id.to_le_bytes().to_vec()),
            Message::DragOutItems { id, items } => {
                let mut p = id.to_le_bytes().to_vec();
                put_items(&mut p, items);
                (T_DRAG_OUT_ITEMS, p)
            }
            Message::DragOutGet { id, item, stream } => {
                let mut p = id.to_le_bytes().to_vec();
                p.extend_from_slice(&item.to_le_bytes());
                p.extend_from_slice(&stream.to_le_bytes());
                (T_DRAG_OUT_GET, p)
            }
            Message::DragOutEnd { id } => (T_DRAG_OUT_END, id.to_le_bytes().to_vec()),
            Message::DragOutFail { stream, why } => {
                let mut p = stream.to_le_bytes().to_vec();
                p.extend_from_slice(why.as_bytes());
                (T_DRAG_OUT_FAIL, p)
            }
        };
        let mut out = Vec::with_capacity(HDR + payload.len());
        out.extend_from_slice(&MAGIC);
        out.push(VERSION);
        out.push(ty);
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }

    fn decode(ty: u8, p: &[u8]) -> Result<Message, Error> {
        let u64_at = |o: usize| -> Result<u64, Error> {
            let b: [u8; 8] = p.get(o..o + 8).ok_or(Error::Malformed)?.try_into().map_err(|_| Error::Malformed)?;
            Ok(u64::from_le_bytes(b))
        };
        let u32_at = |o: usize| -> Result<u32, Error> {
            let b: [u8; 4] = p.get(o..o + 4).ok_or(Error::Malformed)?.try_into().map_err(|_| Error::Malformed)?;
            Ok(u32::from_le_bytes(b))
        };
        let text = |b: &[u8]| -> Result<String, Error> {
            let s = std::str::from_utf8(b).map_err(|_| Error::Malformed)?;
            Ok(s.replace('\0', ""))
        };
        Ok(match ty {
            T_HELLO => Message::Hello { features: u64_at(0)?, agent: text(p.get(8..).ok_or(Error::Malformed)?)? },
            T_CLIP => Message::ClipboardText(text(p)?),
            T_TIME if p.len() == 12 => {
                let nanos = u32_at(8)?;
                if nanos >= 1_000_000_000 {
                    return Err(Error::Malformed);
                }
                Message::TimeSync { unix_secs: u64_at(0)?, nanos }
            }
            T_RES if p.len() == 8 => {
                let (w, h) = (u32_at(0)?, u32_at(4)?);
                if w == 0 || h == 0 || w > 16384 || h > 16384 {
                    return Err(Error::Malformed);
                }
                Message::SetResolution { width: w, height: h }
            }
            T_SCALE if p.len() == 4 => {
                let percent = u32_at(0)?;
                if percent != 0 && !(SCALE_MIN..=SCALE_MAX).contains(&percent) {
                    return Err(Error::Malformed);
                }
                Message::SetScale { percent }
            }
            T_SHUTDOWN if p.len() == 1 => Message::Shutdown { reboot: p[0] != 0 },
            T_PING if p.len() == 8 => Message::Ping(u64_at(0)?),
            T_PONG if p.len() == 8 => Message::Pong(u64_at(0)?),
            T_GPU3D if p.len() == 1 => Message::Gpu3d { on: p[0] != 0 },
            T_DRAG_ENTER => Message::DragEnter { id: u32_at(0)?, items: get_items(p, 1)? },
            T_DRAG_READY if p.len() == 5 => Message::DragReady { id: u32_at(0)?, live: p[4] != 0 },
            T_DRAG_CANCEL if p.len() == 4 => Message::DragCancel { id: u32_at(0)? },
            T_DROP_ENTRY if p.len() > 13 => {
                let path = std::str::from_utf8(&p[13..]).map_err(|_| Error::Malformed)?;
                if !drop_path_ok(path) {
                    return Err(Error::Malformed);
                }
                Message::DropEntry { id: u32_at(0)?, dir: p[4] != 0, size: u64_at(5)?, path: path.to_string() }
            }
            T_DROP_DATA if p.len() >= 4 && p.len() - 4 <= DROP_CHUNK => Message::DropData { id: u32_at(0)?, data: p[4..].to_vec() },
            T_DROP_END if p.len() == 4 => Message::DropEnd { id: u32_at(0)? },
            T_DROP_DONE if p.len() >= 5 => Message::DropDone { id: u32_at(0)?, ok: p[4] != 0, place: text(&p[5..])?.chars().take(256).collect() },
            T_DRAG_OUT_QUERY if p.len() == 4 => Message::DragOutQuery { id: u32_at(0)? },
            T_DRAG_OUT_ITEMS => Message::DragOutItems { id: u32_at(0)?, items: get_items(p, 0)? },
            T_DRAG_OUT_GET if p.len() == 12 => Message::DragOutGet { id: u32_at(0)?, item: u32_at(4)?, stream: u32_at(8)? },
            T_DRAG_OUT_END if p.len() == 4 => Message::DragOutEnd { id: u32_at(0)? },
            T_DRAG_OUT_FAIL if p.len() >= 4 => Message::DragOutFail { stream: u32_at(0)?, why: text(&p[4..])?.chars().take(256).collect() },
            T_TIME | T_RES | T_SCALE | T_SHUTDOWN | T_PING | T_PONG | T_GPU3D | T_DRAG_READY | T_DRAG_CANCEL | T_DROP_ENTRY | T_DROP_DATA
            | T_DROP_END | T_DROP_DONE | T_DRAG_OUT_QUERY | T_DRAG_OUT_GET | T_DRAG_OUT_END | T_DRAG_OUT_FAIL => {
                return Err(Error::Malformed)
            }
            t => return Err(Error::Type(t)),
        })
    }
}

/// Incremental decoder for a byte stream.
#[derive(Debug)]
pub struct Decoder {
    buf: Vec<u8>,
    max_payload: usize,
}

impl Decoder {
    pub fn new(max_payload: usize) -> Self {
        Decoder { buf: Vec::new(), max_payload: max_payload.min(MAX_PAYLOAD) }
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Next complete message. `Ok(None)` = need more bytes. On `Err`, the
    /// offending bytes are skipped; calling again continues with the rest.
    // Not an Iterator: errors are recoverable and Ok(None) means "later".
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Message>, Error> {
        if self.buf.len() < HDR {
            return Ok(None);
        }
        if self.buf[0..2] != MAGIC {
            self.resync();
            return Err(Error::Magic);
        }
        if self.buf[2] != VERSION {
            let v = self.buf[2];
            self.resync();
            return Err(Error::Version(v));
        }
        let ty = self.buf[3];
        let len = u32::from_le_bytes([self.buf[4], self.buf[5], self.buf[6], self.buf[7]]) as usize;
        if len > self.max_payload {
            self.resync();
            return Err(Error::TooLarge(len));
        }
        if self.buf.len() < HDR + len {
            return Ok(None);
        }
        let r = Message::decode(ty, &self.buf[HDR..HDR + len]);
        self.buf.drain(..HDR + len);
        r.map(Some)
    }

    /// Drop bytes until the next possible frame start.
    fn resync(&mut self) {
        let skip = self.buf[1..].windows(2).position(|w| w == MAGIC).map(|p| p + 1).unwrap_or(self.buf.len());
        self.buf.drain(..skip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<Message> {
        vec![
            Message::Hello { features: 3, agent: "vroom-agent 0.1".into() },
            Message::ClipboardText("héllo 🦀".into()),
            Message::ClipboardText(String::new()),
            Message::TimeSync { unix_secs: 1_800_000_000, nanos: 5 },
            Message::SetResolution { width: 1920, height: 1080 },
            Message::SetScale { percent: 200 },
            Message::SetScale { percent: 0 },
            Message::Shutdown { reboot: true },
            Message::Ping(42),
            Message::Pong(42),
            Message::Gpu3d { on: false },
            Message::Gpu3d { on: true },
            Message::DragEnter { id: 3, items: vec![("photo.jpg".into(), false), ("Some folder".into(), true)] },
            Message::DragReady { id: 3, live: true },
            Message::DragCancel { id: 3 },
            Message::DropEntry { id: 3, path: "Some folder/sub/é.txt".into(), dir: false, size: 12 },
            Message::DropEntry { id: 3, path: "Some folder".into(), dir: true, size: 0 },
            Message::DropData { id: 3, data: vec![1, 2, 3] },
            Message::DropData { id: 3, data: Vec::new() },
            Message::DropEnd { id: 3 },
            Message::DropDone { id: 3, ok: true, place: "Desktop".into() },
            Message::DragOutQuery { id: 9 },
            Message::DragOutItems { id: 9, items: vec![("report.pdf".into(), false), ("Photos".into(), true)] },
            Message::DragOutItems { id: 9, items: Vec::new() },
            Message::DragOutGet { id: 9, item: 1, stream: 10 },
            Message::DragOutEnd { id: 9 },
            Message::DragOutFail { stream: 10, why: "can't read report.pdf".into() },
        ]
    }

    #[test]
    fn fixed_names_are_taken() {
        assert_eq!(fix_name("a:b?.txt"), "a_b_.txt");
        assert_eq!(fix_name(".."), "_");
        assert_eq!(fix_name("x\ny"), "xy");
        assert_eq!(fix_name("a/b\\c"), "a_b_c");
        assert!(drop_name_ok(&fix_name(&"é".repeat(300))));
    }

    #[test]
    fn guest_names_are_checked() {
        for bad in ["..", "a/b", "c:", ""] {
            let mut d = Decoder::new(1 << 20);
            d.push(&Message::DragOutItems { id: 1, items: vec![(bad.into(), false)] }.encode());
            assert_eq!(d.next(), Err(Error::Malformed), "{bad:?}");
        }
    }

    #[test]
    fn trees_come_in_order_and_whole() {
        let e = |path: &str, dir: bool, size: u64| Message::DropEntry { id: 1, path: path.into(), dir, size };
        let data = |n: usize| Message::DropData { id: 1, data: vec![0; n] };
        let end = Message::DropEnd { id: 1 };
        let run = |top: &str, dir: bool, ms: &[Message]| -> Result<bool, &'static str> {
            let mut c = TreeCheck::new(1, top, dir);
            let mut last = false;
            for m in ms {
                last = c.check(m)?;
            }
            Ok(last)
        };
        // A folder with a sub folder and files.
        let good = [e("F", true, 9), e("F/a", false, 3), data(2), data(1), e("F/s", true, 0), e("F/s/b", false, 0), end.clone()];
        assert_eq!(run("F", true, &good), Ok(true));
        assert_eq!(run("a.txt", false, &[e("a.txt", false, 1), data(1), end.clone()]), Ok(true));
        // Not the item asked for, or something outside it.
        assert!(run("F", true, &[e("G", true, 0)]).is_err());
        assert!(run("F", true, &[e("F", false, 0)]).is_err());
        assert!(run("F", true, &[e("F", true, 0), e("G/a", false, 0)]).is_err());
        assert!(run("F", true, &[e("F", true, 0), e("F/s/b", false, 0)]).is_err());
        // Nothing under a file, nothing announced twice.
        assert!(run("a", false, &[e("a", false, 0), e("a/b", false, 0)]).is_err());
        assert!(run("F", true, &[e("F", true, 0), e("F/s", true, 0), e("F/s", true, 0)]).is_err());
        // Sizes hold.
        assert!(run("a", false, &[e("a", false, 1), data(2)]).is_err());
        assert!(run("a", false, &[e("a", false, 2), data(1), end.clone()]).is_err());
        assert!(run("F", true, &[e("F", true, 0), e("F/a", false, 2), e("F/b", false, 0)]).is_err());
        // Another drag's frames, data or an end before any entry.
        assert!(run("a", false, &[Message::DropEntry { id: 2, path: "a".into(), dir: false, size: 0 }]).is_err());
        assert!(run("a", false, &[data(0)]).is_err());
        assert!(run("a", false, std::slice::from_ref(&end)).is_err());
        assert!(run("a", false, &[Message::Ping(1)]).is_err());
    }

    #[test]
    fn drop_paths_stay_inside() {
        for bad in ["", "/etc/passwd", "a/../b", "..", "a//b", "a/", "./a", "C:\\x", "c:x", "a\\b", "a\nb", "x?", "con\0"] {
            assert!(!drop_path_ok(bad), "{bad:?}");
        }
        for good in ["a", "a b/c.d", "photo (1).JPG", "Ünïcode/日本"] {
            assert!(drop_path_ok(good), "{good:?}");
        }
        let mut f = Message::DropEntry { id: 1, path: "aa/bb".into(), dir: false, size: 1 }.encode();
        let n = f.len();
        // "aa/bb" -> "aa/.."
        f[n - 1] = b'.';
        f[n - 2] = b'.';
        let mut d = Decoder::new(1024);
        d.push(&f);
        assert_eq!(d.next(), Err(Error::Malformed));
        let mut d = Decoder::new(1 << 20);
        d.push(&Message::DragEnter { id: 1, items: vec![("..".into(), true)] }.encode());
        assert_eq!(d.next(), Err(Error::Malformed));
    }

    #[test]
    fn roundtrip_byte_by_byte() {
        let mut d = Decoder::new(1 << 20);
        let mut got = Vec::new();
        for m in all() {
            for b in m.encode() {
                d.push(&[b]);
                if let Some(x) = d.next().unwrap() {
                    got.push(x);
                }
            }
        }
        assert_eq!(got, all());
    }

    #[test]
    fn limits_and_resync() {
        let mut d = Decoder::new(16);
        d.push(&Message::ClipboardText("x".repeat(17)).encode());
        assert_eq!(d.next(), Err(Error::TooLarge(17)));
        // garbage then a good frame
        let mut d = Decoder::new(1024);
        d.push(b"garbage!!");
        d.push(&Message::Ping(1).encode());
        let mut out = None;
        for _ in 0..10 {
            if let Ok(Some(m)) = d.next() {
                out = Some(m);
                break;
            }
        }
        assert_eq!(out, Some(Message::Ping(1)));
        // invalid utf-8 and NUL stripping
        let mut f = Message::ClipboardText("ab".into()).encode();
        f[8] = 0xff;
        let mut d = Decoder::new(1024);
        d.push(&f);
        assert_eq!(d.next(), Err(Error::Malformed));
        let mut d = Decoder::new(1024);
        d.push(&Message::ClipboardText("a\0b".into()).encode());
        assert_eq!(d.next(), Ok(Some(Message::ClipboardText("ab".into()))));
        // scales out of range
        for bad in [1u32, 99, 501, u32::MAX] {
            let mut f = Message::SetScale { percent: 200 }.encode();
            f[8..12].copy_from_slice(&bad.to_le_bytes());
            let mut d = Decoder::new(1024);
            d.push(&f);
            assert_eq!(d.next(), Err(Error::Malformed), "{bad}");
        }
    }

    #[test]
    fn fuzz_never_panics() {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..2000 {
            let mut d = Decoder::new(4096);
            let mut buf = vec![0u8; (x % 64) as usize];
            for b in buf.iter_mut() {
                x ^= x << 7;
                x ^= x >> 9;
                *b = x as u8;
            }
            if buf.len() > 3 && x.is_multiple_of(3) {
                buf[0] = b'B';
                buf[1] = b'W';
                buf[2] = VERSION;
            }
            d.push(&buf);
            for _ in 0..8 {
                let _ = d.next();
            }
        }
    }
}

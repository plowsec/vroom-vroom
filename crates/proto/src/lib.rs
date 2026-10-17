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
    /// Coherence: the agent reports the guest's windows (`Windows`) and
    /// acts on them (`WindowDo`).
    pub const COHERENCE: u64 = 128;
    /// Files dragged out of the guest's desktop reach the Mac
    /// (`DragOutQuery`...).
    pub const DRAG_OUT: u64 = 256;
    /// The agent can start an app by name for the host (`StartApp`).
    pub const START_APP: u64 = 1024;
    /// The agent arranges several monitors (`SetLayout`), and on Windows
    /// moves the pointer over all of them (`Mouse`).
    pub const MONITORS: u64 = 512;
    /// The agent lists the apps it can start (`Apps`), for the host to
    /// suggest names as one types.
    pub const APP_LIST: u64 = 1 << 16;
}

/// Most windows one `Windows` list may carry, and the longest title (bytes).
pub const WINDOWS_MAX: usize = 64;
pub const TITLE_MAX: usize = 256;
/// Window rectangles stay within this many pixels of the origin.
pub const COORD_MAX: i32 = 1 << 16;
/// Biggest window icon, in pixels on a side.
pub const ICON_MAX: u32 = 64;
/// Longest app name `StartApp` carries, and longest reason in `CoherenceNo`.
pub const START_MAX: usize = 256;

/// Most apps one `Apps` list may carry.
pub const APPS_MAX: usize = 512;
/// Biggest icon in an `Apps` list, in pixels on a side (smaller than
/// window icons: a list holds hundreds).
pub const APP_ICON_MAX: u32 = 32;

/// An app the guest can start (`Apps`): its name, as `StartApp` takes
/// it, and its icon, `w` x `h` RGBA (0 x 0: none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestApp {
    pub name: String,
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

/// One of the guest's top-level windows (Coherence).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestWindow {
    /// The guest's name for it (Windows: the window handle). Unique in a list.
    pub id: u32,
    /// What can be seen of it, in guest screen pixels. May reach past the
    /// screen's edges; never empty.
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    /// `win_flags`.
    pub flags: u32,
    pub title: String,
}

/// `GuestWindow::flags`.
pub mod win_flags {
    /// The window keyboard input goes to.
    pub const FOREGROUND: u32 = 1;
    /// Minimized: not on screen, listed so the host can bring it back.
    pub const MINIMIZED: u32 = 2;
    /// A main window of an app (it would have a taskbar button), not a
    /// menu, tooltip or dialog.
    pub const APP: u32 = 4;
    pub const MAXIMIZED: u32 = 8;
    pub const ALL: u32 = 15;
}

/// What the host asks of a guest window (Coherence).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowOp {
    /// Bring it to the front with the keyboard (restoring it if minimized).
    Activate,
    Minimize,
    /// Ask it to close, as its close button would.
    Close,
    /// Put what can be seen of it here (guest screen pixels).
    Move { x: i32, y: i32, w: u32, h: u32 },
}

/// Most monitors a layout may hold.
pub const LAYOUT_MAX: usize = 16;

/// One monitor of a layout, in guest pixels on the guest's desktop. The
/// first is the main one, at 0,0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Monitor {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
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
    /// Host -> guest: several monitors, each to this size and place; in
    /// order (the host's first, second... GPU output); `None`: that one is
    /// off. Sent instead of SetResolution when there is more than one.
    SetLayout { monitors: Vec<Option<Monitor>> },
    /// Host -> guest (Windows, several monitors): the pointer at `x`, `y`
    /// on the whole desktop (0..=65535 each way), these buttons down
    /// (bit 0 left, 1 right, 2 middle), wheel clicks. Windows maps the
    /// USB tablet to its main monitor only.
    Mouse { x: u16, y: u16, buttons: u8, wheel: i8 },
    /// Host -> guest: Coherence on (report windows, hide the taskbar and
    /// give windows the whole screen) or off (put it all back).
    Coherence { on: bool },
    /// Guest -> host: the guest's windows, front to back, whenever they
    /// change while Coherence is on.
    Windows(Vec<GuestWindow>),
    /// Host -> guest: do this to a window of the last list.
    WindowDo { id: u32, op: WindowOp },
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
    /// Guest -> host: the app icon of a window of the last list, `w` x
    /// `h` RGBA (not premultiplied), at most `ICON_MAX` on a side; 0 x 0:
    /// it has none. Sent when it changes.
    WindowIcon { id: u32, w: u32, h: u32, rgba: Vec<u8> },
    /// Host -> guest: start the installed app this name fits best (the
    /// Start menu's, the desktop's menu's), never a command. Plain text,
    /// at most `START_MAX` bytes.
    StartApp(String),
    /// Guest -> host: Coherence can't work in this session, and why
    /// ("a Wayland session"). Short plain text.
    CoherenceNo(String),
    /// Guest -> host: the apps `StartApp` can start (Windows: the Start
    /// menu's; Linux: the desktop's menu entries), sorted by name, at
    /// most `APPS_MAX`. Sent when Coherence goes on and when it changes.
    Apps(Vec<GuestApp>),
}

/// Whether `s` is fit for `StartApp` / `CoherenceNo`: short, one line.
pub fn line_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= START_MAX && !s.chars().any(char::is_control)
}

/// A window icon as bytes: `id u32 | w u16 | h u16 | rgba`.
pub fn encode_icon(id: u32, w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let mut p = id.to_le_bytes().to_vec();
    p.extend_from_slice(&(w as u16).to_le_bytes());
    p.extend_from_slice(&(h as u16).to_le_bytes());
    p.extend_from_slice(rgba);
    p
}

/// The other way, checked: sides of at most `ICON_MAX` (both 0 or
/// neither), exactly `w * h * 4` bytes of pixels.
pub fn decode_icon(p: &[u8]) -> Option<(u32, u32, u32, Vec<u8>)> {
    let id = u32::from_le_bytes(p.get(0..4)?.try_into().ok()?);
    let w = u16::from_le_bytes(p.get(4..6)?.try_into().ok()?) as u32;
    let h = u16::from_le_bytes(p.get(6..8)?.try_into().ok()?) as u32;
    if w > ICON_MAX || h > ICON_MAX || (w == 0) != (h == 0) || p.len() - 8 != (w * h * 4) as usize {
        return None;
    }
    Some((id, w, h, p[8..].to_vec()))
}

/// An app list as bytes: `count u32`, then for each `len u16 | name |
/// w u16 | h u16 | rgba` (the agent and viewer protocols both carry it).
pub fn encode_apps(list: &[GuestApp]) -> Vec<u8> {
    let mut p = (list.len() as u32).to_le_bytes().to_vec();
    for a in list {
        p.extend_from_slice(&(a.name.len() as u16).to_le_bytes());
        p.extend_from_slice(a.name.as_bytes());
        p.extend_from_slice(&(a.w as u16).to_le_bytes());
        p.extend_from_slice(&(a.h as u16).to_le_bytes());
        p.extend_from_slice(&a.rgba);
    }
    p
}

/// The other way, checked: at most `APPS_MAX` apps, names `line_ok`
/// takes, no name twice, icons of at most `APP_ICON_MAX` on a side (both
/// 0 or neither) with exactly their pixels. None if anything is off.
pub fn decode_apps(p: &[u8]) -> Option<Vec<GuestApp>> {
    let u16_at = |o: usize| -> Option<u32> { Some(u16::from_le_bytes(p.get(o..o + 2)?.try_into().ok()?) as u32) };
    let n = u32::from_le_bytes(p.get(0..4)?.try_into().ok()?) as usize;
    if n > APPS_MAX {
        return None;
    }
    let mut out: Vec<GuestApp> = Vec::with_capacity(n);
    let mut names = std::collections::HashSet::new();
    let mut o = 4;
    for _ in 0..n {
        let len = u16_at(o)? as usize;
        let name = std::str::from_utf8(p.get(o + 2..o + 2 + len)?).ok()?;
        if !line_ok(name) || !names.insert(name) {
            return None;
        }
        o += 2 + len;
        let (w, h) = (u16_at(o)?, u16_at(o + 2)?);
        if w > APP_ICON_MAX || h > APP_ICON_MAX || (w == 0) != (h == 0) {
            return None;
        }
        let px = (w * h * 4) as usize;
        let rgba = p.get(o + 4..o + 4 + px)?.to_vec();
        o += 4 + px;
        out.push(GuestApp { name: name.to_string(), w, h, rgba });
    }
    (o == p.len()).then_some(out)
}

fn rect_ok(x: i32, y: i32, w: u32, h: u32) -> bool {
    (-COORD_MAX..=COORD_MAX).contains(&x) && (-COORD_MAX..=COORD_MAX).contains(&y) && (1..=COORD_MAX as u32).contains(&w) && (1..=COORD_MAX as u32).contains(&h)
}

/// A window list as bytes (the agent and viewer protocols both carry it).
pub fn encode_windows(list: &[GuestWindow]) -> Vec<u8> {
    let mut p = (list.len() as u32).to_le_bytes().to_vec();
    for w in list {
        for v in [w.id, w.x as u32, w.y as u32, w.w, w.h, w.flags] {
            p.extend_from_slice(&v.to_le_bytes());
        }
        p.extend_from_slice(&(w.title.len() as u16).to_le_bytes());
        p.extend_from_slice(w.title.as_bytes());
    }
    p
}

/// The other way, checked: at most `WINDOWS_MAX` windows, unique ids,
/// sane rectangles, known flags, titles of at most `TITLE_MAX` bytes of
/// UTF-8. None if anything is off.
pub fn decode_windows(p: &[u8]) -> Option<Vec<GuestWindow>> {
    let u32_at = |o: usize| -> Option<u32> { Some(u32::from_le_bytes(p.get(o..o + 4)?.try_into().ok()?)) };
    let n = u32_at(0)? as usize;
    if n > WINDOWS_MAX {
        return None;
    }
    let mut out: Vec<GuestWindow> = Vec::with_capacity(n);
    let mut o = 4;
    for _ in 0..n {
        let (id, x, y, w, h, flags) = (u32_at(o)?, u32_at(o + 4)? as i32, u32_at(o + 8)? as i32, u32_at(o + 12)?, u32_at(o + 16)?, u32_at(o + 20)?);
        let len = u16::from_le_bytes(p.get(o + 24..o + 26)?.try_into().ok()?) as usize;
        if len > TITLE_MAX || !rect_ok(x, y, w, h) || flags & !win_flags::ALL != 0 || out.iter().any(|v| v.id == id) {
            return None;
        }
        let title = std::str::from_utf8(p.get(o + 26..o + 26 + len)?).ok()?.replace('\0', "");
        out.push(GuestWindow { id, x, y, w, h, flags, title });
        o += 26 + len;
    }
    (o == p.len()).then_some(out)
}

impl WindowOp {
    /// `op u8 [| x i32 | y i32 | w u32 | h u32]`.
    pub fn encode(&self) -> Vec<u8> {
        match *self {
            WindowOp::Activate => vec![1],
            WindowOp::Minimize => vec![2],
            WindowOp::Close => vec![3],
            WindowOp::Move { x, y, w, h } => {
                let mut p = vec![4];
                for v in [x as u32, y as u32, w, h] {
                    p.extend_from_slice(&v.to_le_bytes());
                }
                p
            }
        }
    }

    pub fn decode(p: &[u8]) -> Option<WindowOp> {
        let u32_at = |o: usize| -> Option<u32> { Some(u32::from_le_bytes(p.get(o..o + 4)?.try_into().ok()?)) };
        Some(match (p.first()?, p.len()) {
            (1, 1) => WindowOp::Activate,
            (2, 1) => WindowOp::Minimize,
            (3, 1) => WindowOp::Close,
            (4, 17) => {
                let (x, y, w, h) = (u32_at(1)? as i32, u32_at(5)? as i32, u32_at(9)?, u32_at(13)?);
                if !rect_ok(x, y, w, h) {
                    return None;
                }
                WindowOp::Move { x, y, w, h }
            }
            _ => return None,
        })
    }
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
const T_LAYOUT: u8 = 25;
const T_MOUSE: u8 = 26;
const T_COHERENCE: u8 = 17;
const T_WINDOWS: u8 = 18;
const T_WINDOW_DO: u8 = 19;
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
const T_WINDOW_ICON: u8 = 28;
const T_START_APP: u8 = 29;
const T_COHERENCE_NO: u8 = 30;
// Well above the others: other work adds types in the 30s.
const T_APPS: u8 = 60;

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
            Message::SetLayout { monitors } => {
                let mut p = Vec::with_capacity(monitors.len() * 17);
                for m in monitors {
                    let m2 = m.unwrap_or(Monitor { x: 0, y: 0, width: 0, height: 0 });
                    p.push(m.is_some() as u8);
                    p.extend_from_slice(&m2.x.to_le_bytes());
                    p.extend_from_slice(&m2.y.to_le_bytes());
                    p.extend_from_slice(&m2.width.to_le_bytes());
                    p.extend_from_slice(&m2.height.to_le_bytes());
                }
                (T_LAYOUT, p)
            }
            Message::Mouse { x, y, buttons, wheel } => {
                let mut p = x.to_le_bytes().to_vec();
                p.extend_from_slice(&y.to_le_bytes());
                p.push(*buttons);
                p.push(*wheel as u8);
                (T_MOUSE, p)
            }
            Message::Coherence { on } => (T_COHERENCE, vec![*on as u8]),
            Message::Windows(list) => (T_WINDOWS, encode_windows(list)),
            Message::WindowDo { id, op } => {
                let mut p = id.to_le_bytes().to_vec();
                p.extend_from_slice(&op.encode());
                (T_WINDOW_DO, p)
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
            Message::WindowIcon { id, w, h, rgba } => (T_WINDOW_ICON, encode_icon(*id, *w, *h, rgba)),
            Message::StartApp(s) => (T_START_APP, s.as_bytes().to_vec()),
            Message::CoherenceNo(s) => (T_COHERENCE_NO, s.as_bytes().to_vec()),
            Message::Apps(list) => (T_APPS, encode_apps(list)),
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
            T_LAYOUT if !p.is_empty() && p.len().is_multiple_of(17) && p.len() / 17 <= LAYOUT_MAX => {
                let mut monitors = Vec::new();
                for o in (0..p.len()).step_by(17) {
                    let (on, x, y, w, h) = (p[o], u32_at(o + 1)? as i32, u32_at(o + 5)? as i32, u32_at(o + 9)?, u32_at(o + 13)?);
                    let fits = |v: i32| (-65536..=65536).contains(&v);
                    monitors.push(match on {
                        0 => None,
                        1 if w > 0 && h > 0 && w <= 16384 && h <= 16384 && fits(x) && fits(y) => Some(Monitor { x, y, width: w, height: h }),
                        _ => return Err(Error::Malformed),
                    });
                }
                Message::SetLayout { monitors }
            }
            T_MOUSE if p.len() == 6 && p[4] < 8 => {
                Message::Mouse { x: u16::from_le_bytes([p[0], p[1]]), y: u16::from_le_bytes([p[2], p[3]]), buttons: p[4], wheel: p[5] as i8 }
            }
            T_COHERENCE if p.len() == 1 => Message::Coherence { on: p[0] != 0 },
            T_WINDOWS => Message::Windows(decode_windows(p).ok_or(Error::Malformed)?),
            T_WINDOW_DO if p.len() > 4 => Message::WindowDo { id: u32_at(0)?, op: WindowOp::decode(&p[4..]).ok_or(Error::Malformed)? },
            T_DRAG_OUT_QUERY if p.len() == 4 => Message::DragOutQuery { id: u32_at(0)? },
            T_DRAG_OUT_ITEMS => Message::DragOutItems { id: u32_at(0)?, items: get_items(p, 0)? },
            T_DRAG_OUT_GET if p.len() == 12 => Message::DragOutGet { id: u32_at(0)?, item: u32_at(4)?, stream: u32_at(8)? },
            T_DRAG_OUT_END if p.len() == 4 => Message::DragOutEnd { id: u32_at(0)? },
            T_DRAG_OUT_FAIL if p.len() >= 4 => Message::DragOutFail { stream: u32_at(0)?, why: text(&p[4..])?.chars().take(256).collect() },
            T_WINDOW_ICON => {
                let (id, w, h, rgba) = decode_icon(p).ok_or(Error::Malformed)?;
                Message::WindowIcon { id, w, h, rgba }
            }
            T_START_APP | T_COHERENCE_NO => {
                let s = std::str::from_utf8(p).map_err(|_| Error::Malformed)?;
                if !line_ok(s) {
                    return Err(Error::Malformed);
                }
                if ty == T_START_APP {
                    Message::StartApp(s.to_string())
                } else {
                    Message::CoherenceNo(s.to_string())
                }
            }
            T_APPS => Message::Apps(decode_apps(p).ok_or(Error::Malformed)?),
            T_TIME | T_RES | T_SCALE | T_SHUTDOWN | T_PING | T_PONG | T_GPU3D | T_DRAG_READY | T_DRAG_CANCEL | T_DROP_ENTRY | T_DROP_DATA
            | T_DROP_END | T_DROP_DONE | T_COHERENCE | T_WINDOW_DO | T_DRAG_OUT_QUERY | T_DRAG_OUT_GET | T_DRAG_OUT_END | T_DRAG_OUT_FAIL | T_LAYOUT | T_MOUSE => {
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
            Message::SetLayout {
                monitors: vec![Some(Monitor { x: 0, y: 0, width: 1280, height: 800 }), None, Some(Monitor { x: 1280, y: -5, width: 640, height: 480 })],
            },
            Message::Mouse { x: 65535, y: 3, buttons: 5, wheel: -2 },
            Message::Coherence { on: true },
            Message::Windows(Vec::new()),
            Message::Windows(vec![
                GuestWindow { id: 0x1234, x: -8, y: 0, w: 800, h: 600, flags: win_flags::APP | win_flags::FOREGROUND, title: "Notepad — é".into() },
                GuestWindow { id: 7, x: 10, y: 20, w: 1, h: 1, flags: 0, title: String::new() },
            ]),
            Message::WindowDo { id: 0x1234, op: WindowOp::Activate },
            Message::WindowDo { id: 1, op: WindowOp::Close },
            Message::WindowDo { id: 1, op: WindowOp::Minimize },
            Message::WindowDo { id: 1, op: WindowOp::Move { x: -5, y: 3, w: 640, h: 480 } },
            Message::DragOutQuery { id: 9 },
            Message::DragOutItems { id: 9, items: vec![("report.pdf".into(), false), ("Photos".into(), true)] },
            Message::DragOutItems { id: 9, items: Vec::new() },
            Message::DragOutGet { id: 9, item: 1, stream: 10 },
            Message::DragOutEnd { id: 9 },
            Message::DragOutFail { stream: 10, why: "can't read report.pdf".into() },
            Message::WindowIcon { id: 0x1234, w: 2, h: 1, rgba: vec![1, 2, 3, 4, 5, 6, 7, 8] },
            Message::WindowIcon { id: 9, w: 0, h: 0, rgba: Vec::new() },
            Message::StartApp("Notepad".into()),
            Message::CoherenceNo("a Wayland session".into()),
            Message::Apps(Vec::new()),
            Message::Apps(vec![
                GuestApp { name: "Calculator".into(), w: 1, h: 2, rgba: vec![1, 2, 3, 4, 5, 6, 7, 8] },
                GuestApp { name: "Éditeur de texte".into(), w: 0, h: 0, rgba: Vec::new() },
            ]),
        ]
    }

    #[test]
    fn app_lists_are_checked() {
        let a = |name: &str| GuestApp { name: name.into(), w: 0, h: 0, rgba: Vec::new() };
        let bad = |list: Vec<GuestApp>| decode_apps(&encode_apps(&list)).is_none();
        assert!(!bad(vec![a("a"), a("b")]));
        assert!(bad(vec![a("a"), a("a")]), "same name twice");
        assert!(bad(vec![a("")]));
        assert!(bad(vec![a("a\nb")]));
        assert!(bad(vec![a(&"x".repeat(START_MAX + 1))]));
        assert!(bad((0..APPS_MAX + 1).map(|i| a(&i.to_string())).collect()), "too many");
        let n = APP_ICON_MAX + 1;
        assert!(bad(vec![GuestApp { w: n, h: 1, rgba: vec![0; n as usize * 4], ..a("a") }]), "icon too big");
        assert!(bad(vec![GuestApp { w: 0, h: 1, rgba: Vec::new(), ..a("a") }]), "half empty");
        assert!(bad(vec![GuestApp { w: 1, h: 1, rgba: vec![0; 3], ..a("a") }]), "short icon");
        let mut p = encode_apps(&[a("a")]);
        p.push(0);
        assert!(decode_apps(&p).is_none(), "trailing bytes");
        let mut p = encode_apps(&[a("a")]);
        p[0] = 2;
        assert!(decode_apps(&p).is_none(), "a count that lies");
    }

    #[test]
    fn layouts_are_checked() {
        let bad = |m: Monitor| {
            let mut d = Decoder::new(1024);
            d.push(&Message::SetLayout { monitors: vec![Some(m)] }.encode());
            d.next()
        };
        assert_eq!(bad(Monitor { x: 0, y: 0, width: 0, height: 5 }), Err(Error::Malformed));
        assert_eq!(bad(Monitor { x: 0, y: 0, width: 16385, height: 5 }), Err(Error::Malformed));
        assert_eq!(bad(Monitor { x: i32::MIN, y: 0, width: 5, height: 5 }), Err(Error::Malformed));
        let mut d = Decoder::new(1024);
        d.push(&Message::SetLayout { monitors: vec![None; LAYOUT_MAX + 1] }.encode());
        assert_eq!(d.next(), Err(Error::Malformed));
        let mut d = Decoder::new(1024);
        d.push(&Message::SetLayout { monitors: vec![] }.encode());
        assert_eq!(d.next(), Err(Error::Malformed));
    }

    #[test]
    fn window_lists_are_checked() {
        let w = |id: u32| GuestWindow { id, x: 0, y: 0, w: 10, h: 10, flags: 0, title: "t".into() };
        let bad = |list: Vec<GuestWindow>| decode_windows(&encode_windows(&list)).is_none();
        assert!(!bad(vec![w(1), w(2)]));
        assert!(bad(vec![w(1), w(1)]), "same id twice");
        assert!(bad((0..WINDOWS_MAX as u32 + 1).map(w).collect()), "too many");
        assert!(bad(vec![GuestWindow { title: "x".repeat(TITLE_MAX + 1), ..w(1) }]));
        assert!(bad(vec![GuestWindow { w: 0, ..w(1) }]));
        assert!(bad(vec![GuestWindow { x: i32::MIN, ..w(1) }]));
        assert!(bad(vec![GuestWindow { h: u32::MAX, ..w(1) }]));
        assert!(bad(vec![GuestWindow { flags: 1 << 9, ..w(1) }]));
        // Trailing bytes, a short list, a count that lies.
        let mut p = encode_windows(&[w(1)]);
        p.push(0);
        assert!(decode_windows(&p).is_none());
        let p = encode_windows(&[w(1)]);
        assert!(decode_windows(&p[..p.len() - 1]).is_none());
        let mut p = encode_windows(&[w(1)]);
        p[0] = 2;
        assert!(decode_windows(&p).is_none());
        // Ops: sizes and rects.
        assert_eq!(WindowOp::decode(&[1, 0]), None);
        assert_eq!(WindowOp::decode(&[9]), None);
        let mut m = WindowOp::Move { x: 0, y: 0, w: 1, h: 1 }.encode();
        m[9..13].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(WindowOp::decode(&m), None);
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
    fn icons_and_names_are_checked() {
        let bad = |p: Vec<u8>| decode_icon(&p).is_none();
        assert!(!bad(encode_icon(1, ICON_MAX, ICON_MAX, &vec![0; (ICON_MAX * ICON_MAX * 4) as usize])));
        assert!(bad(encode_icon(1, ICON_MAX + 1, 1, &vec![0; ((ICON_MAX + 1) * 4) as usize])), "too wide");
        assert!(bad(encode_icon(1, 2, 2, &[0; 15])), "short");
        assert!(bad(encode_icon(1, 2, 2, &[0; 17])), "long");
        assert!(bad(encode_icon(1, 0, 2, &[])), "half empty");
        assert!(bad(vec![1, 0, 0]), "no header");
        for s in ["", "a\nb", "\u{7}", &"x".repeat(START_MAX + 1)] {
            let mut d = Decoder::new(1 << 20);
            let f = Message::StartApp("x".into()).encode();
            let mut f = f[..4].to_vec();
            f.extend_from_slice(&(s.len() as u32).to_le_bytes());
            f.extend_from_slice(s.as_bytes());
            d.push(&f);
            assert_eq!(d.next(), Err(Error::Malformed), "{s:?}");
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

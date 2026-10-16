//! Files dragged out of the guest's desktop (guest -> host).
//!
//! When the Mac's mouse leaves the VM window with the button held, the
//! host asks (`DragOutQuery`). Where the desktop lets us (Windows, X11),
//! a window of ours goes under the pointer and becomes a drop target: if
//! the drag going on carries files, it enters our window and we learn
//! their paths. We answer with their names (`DragOutItems`); the host
//! starts a drag on the Mac and gives up the one here (Escape). When the
//! Mac drop wants an item (`DragOutGet`), we read it and send it as
//! DropEntry, DropData and DropEnd frames, from a thread of our own.
//!
//! Only what the user dragged goes: the paths the desktop handed our
//! drop target, read when the host asks for them.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use vroom_proto::{fix_name, Message, DROP_CHUNK, DROP_ENTRIES_MAX, DROP_ITEMS_MAX, DROP_PATH_MAX};

/// The host link, shared by the session loop and our threads. A frame is
/// written whole under the lock.
#[derive(Clone)]
pub struct Shared(Arc<Mutex<crate::port::Writer>>);

impl Shared {
    pub fn new(w: crate::port::Writer) -> Shared {
        Shared(Arc::new(Mutex::new(w)))
    }

    pub fn send(&self, m: &Message) -> io::Result<()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).write_all(&m.encode())
    }
}

impl Write for Shared {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).write(b)
    }
    fn write_all(&mut self, b: &[u8]) -> io::Result<()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).write_all(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).flush()
    }
}

/// What the desktop's drag carries, found by a drop target under the
/// pointer: the paths (none: not a drag of files), and how to take the
/// drop target away again.
pub struct Found {
    pub paths: Vec<PathBuf>,
    pub close: Option<Box<dyn FnOnce() + Send>>,
}

pub type ProbeFn = fn() -> Result<Found, String>;

#[derive(Default)]
struct State {
    /// The drag the host asked about last.
    want: Option<u32>,
    /// Its paths, once found.
    cur: Option<(u32, Vec<PathBuf>)>,
    close: Option<Box<dyn FnOnce() + Send>>,
}

pub struct DragOut {
    probe: ProbeFn,
    st: Arc<Mutex<State>>,
    /// Stops the item being sent.
    stop: Option<Arc<AtomicBool>>,
}

impl DragOut {
    pub fn new() -> DragOut {
        DragOut::with(probe)
    }

    pub fn with(probe: ProbeFn) -> DragOut {
        DragOut { probe, st: Arc::default(), stop: None }
    }

    /// Whether `m` is ours; then it is handled.
    pub fn handle(&mut self, m: &Message, out: &Shared) -> io::Result<bool> {
        match m {
            Message::DragOutQuery { id } => {
                self.forget();
                self.st.lock().unwrap_or_else(|e| e.into_inner()).want = Some(*id);
                let (id, probe, st, out) = (*id, self.probe, self.st.clone(), out.clone());
                // A drop target takes a moment to set up and to be
                // entered: not on the session loop.
                std::thread::Builder::new().name("drag-out".into()).spawn(move || query(id, probe, &st, &out))?;
            }
            Message::DragOutGet { id, item, stream } => {
                self.stop_stream();
                let path = {
                    let st = self.st.lock().unwrap_or_else(|e| e.into_inner());
                    st.cur.as_ref().filter(|c| c.0 == *id).and_then(|c| c.1.get(*item as usize)).cloned()
                };
                let Some(path) = path else {
                    out.send(&Message::DragOutFail { stream: *stream, why: "that drag is over".into() })?;
                    return Ok(true);
                };
                let stop = Arc::new(AtomicBool::new(false));
                self.stop = Some(stop.clone());
                let (stream, out) = (*stream, out.clone());
                std::thread::Builder::new().name("drag-out-send".into()).spawn(move || {
                    if let Err(why) = send_item(&path, stream, &out, &stop) {
                        log::warn!("drag out: {}: {why}", path.display());
                        let _ = out.send(&Message::DragOutFail { stream, why });
                    }
                })?;
            }
            Message::DragOutEnd { id } => {
                let ours = self.st.lock().unwrap_or_else(|e| e.into_inner()).cur.as_ref().is_some_and(|c| c.0 == *id);
                if ours {
                    self.forget();
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn stop_stream(&mut self) {
        if let Some(s) = self.stop.take() {
            s.store(true, Ordering::Relaxed);
        }
    }

    fn forget(&mut self) {
        self.stop_stream();
        let close = {
            let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
            st.cur = None;
            st.want = None;
            st.close.take()
        };
        if let Some(c) = close {
            c();
        }
    }
}

fn query(id: u32, probe: ProbeFn, st: &Mutex<State>, out: &Shared) {
    let found = probe().unwrap_or_else(|e| {
        log::info!("drag out: {e}");
        Found { paths: Vec::new(), close: None }
    });
    let mut paths = found.paths;
    paths.truncate(DROP_ITEMS_MAX);
    let items: Vec<(String, bool)> = paths
        .iter()
        .map(|p| (fix_name(&p.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()), p.is_dir()))
        .collect();
    log::info!("drag {id} out: {} item(s)", items.len());
    {
        let mut st = st.lock().unwrap_or_else(|e| e.into_inner());
        if st.want != Some(id) {
            // Asked about another since, or over.
            drop(st);
            if let Some(c) = found.close {
                c();
            }
            return;
        }
        st.cur = (!paths.is_empty()).then_some((id, paths));
        st.close = found.close;
    }
    let _ = out.send(&Message::DragOutItems { id, items });
}

/// One file or folder of an item: its path in the drag, where it is here.
struct Entry {
    rel: String,
    path: PathBuf,
    dir: bool,
    size: u64,
}

/// The item and everything in it, folders before what is in them. The
/// item itself may be a link (the user dragged it); inside, links and
/// special files are left out (a link could lead anywhere, a loop
/// forever), and so is what is too deep for a drag path.
fn walk(top: &Path) -> Result<Vec<Entry>, String> {
    let m = std::fs::metadata(top).map_err(|e| e.to_string())?;
    let name = fix_name(&top.file_name().map(|n| n.to_string_lossy()).unwrap_or_default());
    let mut out = Vec::new();
    if m.is_dir() {
        out.push(Entry { rel: name.clone(), path: top.to_path_buf(), dir: true, size: 0 });
        add_dir(&mut out, top, &name);
        // The folder's size: the total, for the host's progress.
        out[0].size = out.iter().map(|e| e.size).sum();
    } else if m.is_file() {
        out.push(Entry { rel: name, path: top.to_path_buf(), dir: false, size: m.len() });
    } else {
        return Err("not a file or folder".into());
    }
    Ok(out)
}

fn add_dir(out: &mut Vec<Entry>, dir: &Path, rel: &str) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut kids: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    kids.sort();
    let mut seen = std::collections::HashSet::new();
    for k in kids {
        if out.len() >= DROP_ENTRIES_MAX {
            return;
        }
        let Some(n) = k.file_name() else { continue };
        let r = format!("{rel}/{}", fix_name(&n.to_string_lossy()));
        // Two names that read the same once fixed: the first one.
        if r.len() > DROP_PATH_MAX || !seen.insert(r.clone()) {
            continue;
        }
        let Ok(m) = std::fs::symlink_metadata(&k) else { continue };
        if m.is_file() {
            out.push(Entry { rel: r, path: k, dir: false, size: m.len() });
        } else if m.is_dir() {
            out.push(Entry { rel: r.clone(), path: k.clone(), dir: true, size: 0 });
            add_dir(out, &k, &r);
        }
    }
}

/// Send one item as `stream`. Err: why it can't be (the host is told).
/// A file that changes size underway goes as announced, cut or padded.
fn send_item(top: &Path, stream: u32, out: &Shared, stop: &AtomicBool) -> Result<(), String> {
    let entries = walk(top)?;
    let mut buf = vec![0u8; DROP_CHUNK];
    let (mut files, mut bytes) = (0u32, 0u64);
    let gone = |e: io::Error| format!("host link: {e}");
    for e in &entries {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        // Opened before it is announced: a file that can't be read
        // (in use, no access) fails the item with its name.
        let mut f = if e.dir { None } else { Some(std::fs::File::open(&e.path).map_err(|err| format!("can't read {}: {err}", e.rel))?) };
        out.send(&Message::DropEntry { id: stream, path: e.rel.clone(), dir: e.dir, size: e.size }).map_err(gone)?;
        let Some(f) = f.as_mut() else { continue };
        files += 1;
        let mut left = e.size;
        let mut eof = false;
        while left > 0 {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let want = left.min(buf.len() as u64) as usize;
            let n = if eof {
                0
            } else {
                match f.read(&mut buf[..want]) {
                    Ok(n) => n,
                    Err(err) => return Err(format!("can't read {}: {err}", e.rel)),
                }
            };
            let n = if n == 0 {
                // Shorter than it was: zeros.
                eof = true;
                buf[..want].fill(0);
                want
            } else {
                n
            };
            out.send(&Message::DropData { id: stream, data: buf[..n].to_vec() }).map_err(gone)?;
            left -= n as u64;
            bytes += n as u64;
        }
    }
    out.send(&Message::DropEnd { id: stream }).map_err(gone)?;
    log::info!("drag out: {} sent ({files} file(s), {bytes} bytes)", top.display());
    Ok(())
}

#[cfg(windows)]
fn probe() -> Result<Found, String> {
    crate::dnd_win::probe()
}

#[cfg(target_os = "linux")]
fn probe() -> Result<Found, String> {
    crate::dnd_x11::probe()
}

#[cfg(not(any(windows, target_os = "linux")))]
fn probe() -> Result<Found, String> {
    Err("no drop target on this desktop".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use vroom_proto::{Decoder, TreeCheck};

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Messages written so far, waiting until `until` says enough.
    fn msgs(s: &Sink, until: impl Fn(&[Message]) -> bool) -> Vec<Message> {
        let t0 = Instant::now();
        loop {
            let mut d = Decoder::new(1 << 22);
            d.push(&s.0.lock().unwrap());
            let v: Vec<Message> = std::iter::from_fn(|| d.next().unwrap()).collect();
            if until(&v) || t0.elapsed() > Duration::from_secs(5) {
                return v;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn tree() -> PathBuf {
        static ONCE: std::sync::Once = std::sync::Once::new();
        let d = std::env::temp_dir().join(format!("vroom-dragout-test-{}", std::process::id()));
        ONCE.call_once(|| {
            std::fs::create_dir_all(d.join("Photos/sub")).unwrap();
            std::fs::write(d.join("Photos/a.jpg"), vec![1u8; DROP_CHUNK + 7]).unwrap();
            std::fs::write(d.join("Photos/sub/b?.txt"), b"hi").unwrap();
            std::fs::write(d.join("note.txt"), b"hello").unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink("/etc", d.join("Photos/link")).unwrap();
        });
        d
    }

    fn fake() -> Result<Found, String> {
        let d = tree();
        Ok(Found { paths: vec![d.join("note.txt"), d.join("Photos")], close: None })
    }

    #[test]
    fn a_dragged_folder_goes_whole_and_checked() {
        let sink = Sink::default();
        let out = Shared::new(Box::new(sink.clone()));
        let mut d = DragOut::with(fake);
        d.handle(&Message::DragOutQuery { id: 4 }, &out).unwrap();
        let m = msgs(&sink, |v| !v.is_empty());
        assert_eq!(m, vec![Message::DragOutItems { id: 4, items: vec![("note.txt".into(), false), ("Photos".into(), true)] }]);
        sink.0.lock().unwrap().clear();
        d.handle(&Message::DragOutGet { id: 4, item: 1, stream: 9 }, &out).unwrap();
        let m = msgs(&sink, |v| v.last().is_some_and(|m| matches!(m, Message::DropEnd { .. } | Message::DragOutFail { .. })));
        let mut c = TreeCheck::new(9, "Photos", true);
        let ends: Vec<bool> = m.iter().map(|m| c.check(m).unwrap()).collect();
        assert_eq!(ends.last(), Some(&true));
        let entries: Vec<(String, u64)> =
            m.iter().filter_map(|m| if let Message::DropEntry { path, size, .. } = m { Some((path.clone(), *size)) } else { None }).collect();
        // The link is left out; the folder says its total.
        let total = DROP_CHUNK as u64 + 7 + 2;
        assert_eq!(
            entries,
            vec![("Photos".into(), total), ("Photos/a.jpg".into(), DROP_CHUNK as u64 + 7), ("Photos/sub".into(), 0), ("Photos/sub/b_.txt".into(), 2)]
        );
    }

    #[test]
    fn only_the_drag_asked_about_is_read() {
        let sink = Sink::default();
        let out = Shared::new(Box::new(sink.clone()));
        let mut d = DragOut::with(fake);
        d.handle(&Message::DragOutQuery { id: 1 }, &out).unwrap();
        msgs(&sink, |v| !v.is_empty());
        for m in [Message::DragOutGet { id: 2, item: 0, stream: 5 }, Message::DragOutGet { id: 1, item: 7, stream: 6 }] {
            d.handle(&m, &out).unwrap();
        }
        d.handle(&Message::DragOutEnd { id: 1 }, &out).unwrap();
        d.handle(&Message::DragOutGet { id: 1, item: 0, stream: 7 }, &out).unwrap();
        let m = msgs(&sink, |v| v.len() >= 4);
        let failed: Vec<u32> = m.iter().filter_map(|m| if let Message::DragOutFail { stream, .. } = m { Some(*stream) } else { None }).collect();
        assert_eq!(failed, vec![5, 6, 7]);
        assert!(!m.iter().any(|m| matches!(m, Message::DropEntry { .. })));
    }

    #[test]
    fn a_desktop_without_files_says_none() {
        let sink = Sink::default();
        let out = Shared::new(Box::new(sink.clone()));
        let mut d = DragOut::with(|| Err("not here".into()));
        d.handle(&Message::DragOutQuery { id: 3 }, &out).unwrap();
        assert_eq!(msgs(&sink, |v| !v.is_empty()), vec![Message::DragOutItems { id: 3, items: Vec::new() }]);
    }
}

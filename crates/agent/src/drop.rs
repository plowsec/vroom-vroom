//! Files dropped on the VM window (host -> guest).
//!
//! While files are dragged over the window the host sends their names
//! (`DragEnter`); nothing more unless they are dropped. We make a staging
//! folder with the top-level names in it (empty), and where the desktop
//! can (Windows) start a drag of our own from under the pointer: the host
//! holds the guest's mouse button down and moves the pointer as the
//! user's moves, so the drop lands in whatever app is under it, as if the
//! files had been dragged there from inside the guest. On the drop the
//! host sends the contents (`DropEntry`, `DropData`, `DropEnd`); once
//! they are written we say `DropDone` and the host lets go of the button.
//! Without a live drag the files go to the Desktop instead.
//!
//! The host is trusted more than the guest, still: paths come checked by
//! vroom-proto (relative, no `..`), must start with a name from
//! `DragEnter`, and a file gets no more bytes than it announced.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use vroom_proto::Message;

/// A drag of our own in the guest's desktop, started under the pointer.
pub trait LiveDrag {
    /// The host gave up (the drag left the window): we are done with it.
    fn cancel(&mut self);
}

struct Incoming {
    id: u32,
    dir: PathBuf,
    items: Vec<(String, bool)>,
    live: Option<Box<dyn LiveDrag>>,
    /// The file being written and the bytes it still expects.
    file: Option<(File, u64)>,
    /// The first error; the rest of the drop is skipped.
    failed: Option<String>,
    files: u32,
}

/// Starts a live drag over a staging folder; None where the desktop can't.
type StartFn = fn(&Path, &[(String, bool)]) -> Option<Box<dyn LiveDrag>>;

pub struct Drops {
    base: PathBuf,
    cur: Option<Incoming>,
    start: StartFn,
}

/// Staging folders older than this go when a new drag starts (an app
/// may still read the files of a drop for a while: Explorer copies in
/// the background, an editor opens the file in place).
const KEEP: Duration = Duration::from_secs(24 * 3600);

impl Drops {
    pub fn new() -> Drops {
        Drops {
            base: staging_base(),
            cur: None,
            start: live_drag,
        }
    }

    #[cfg(test)]
    fn with(base: PathBuf, start: StartFn) -> Drops {
        Drops {
            base,
            cur: None,
            start,
        }
    }

    /// Whether `m` is ours; then it is handled, answers go to `out`.
    pub fn handle(&mut self, m: &Message, out: &mut dyn Write) -> io::Result<bool> {
        match m {
            Message::DragEnter { id, items } => {
                self.cancel();
                let live = self.enter(*id, items);
                match live {
                    Ok(live) => out.write_all(&Message::DragReady { id: *id, live }.encode())?,
                    Err(e) => out.write_all(
                        &Message::DropDone {
                            id: *id,
                            ok: false,
                            place: e.to_string(),
                        }
                        .encode(),
                    )?,
                }
            }
            Message::DragCancel { id } => {
                if self.cur.as_ref().is_some_and(|d| d.id == *id) {
                    self.cancel();
                }
            }
            Message::DropEntry {
                id,
                path,
                dir,
                size,
            } => {
                if let Some(d) = self
                    .cur
                    .as_mut()
                    .filter(|d| d.id == *id && d.failed.is_none())
                {
                    if let Err(e) = d.entry(path, *dir, *size) {
                        d.failed = Some(format!("{path}: {e}"));
                    }
                }
            }
            Message::DropData { id, data } => {
                if let Some(d) = self
                    .cur
                    .as_mut()
                    .filter(|d| d.id == *id && d.failed.is_none())
                {
                    if let Err(e) = d.data(data) {
                        d.failed = Some(e.to_string());
                    }
                }
            }
            Message::DropEnd { id } => {
                if self.cur.as_ref().is_some_and(|d| d.id == *id) {
                    let (ok, place) = self.end();
                    out.write_all(&Message::DropDone { id: *id, ok, place }.encode())?;
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn enter(&mut self, id: u32, items: &[(String, bool)]) -> io::Result<bool> {
        let _ = std::fs::create_dir_all(&self.base);
        sweep(&self.base);
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let dir = self.base.join(format!("{stamp}-{id}"));
        std::fs::create_dir_all(&dir)?;
        // The names exist from the start: a drag needs real files to
        // point at. Their contents come with the drop.
        for (name, is_dir) in items {
            let p = dir.join(name);
            if *is_dir {
                std::fs::create_dir_all(&p)?;
            } else {
                File::create(&p)?;
            }
        }
        let live = (self.start)(&dir, items);
        let is_live = live.is_some();
        self.cur = Some(Incoming {
            id,
            dir,
            items: items.to_vec(),
            live,
            file: None,
            failed: None,
            files: 0,
        });
        log::info!(
            "drop {id}: {} item(s) over the window{}",
            items.len(),
            if is_live { ", live drag" } else { "" }
        );
        Ok(is_live)
    }

    fn cancel(&mut self) {
        if let Some(mut d) = self.cur.take() {
            if let Some(l) = d.live.as_mut() {
                l.cancel();
            }
            d.file = None;
            let _ = std::fs::remove_dir_all(&d.dir);
            log::info!("drop {}: given up", d.id);
        }
    }

    /// The drop is complete: (ok, where the files are or what failed).
    fn end(&mut self) -> (bool, String) {
        let Some(mut d) = self.cur.take() else {
            return (false, String::new());
        };
        if d.failed.is_none() && d.file.as_ref().is_some_and(|f| f.1 != 0) {
            d.failed = Some("a file came short".into());
        }
        d.file = None;
        if let Some(e) = d.failed.take() {
            log::warn!("drop {}: {e}", d.id);
            let _ = std::fs::remove_dir_all(&d.dir);
            return (false, e);
        }
        log::info!("drop {}: {} file(s) written", d.id, d.files);
        if d.live.is_some() {
            // The app under the pointer takes them from the staging
            // folder when the host lets go of the button.
            return (true, String::new());
        }
        match land(&d.dir, &d.items) {
            Ok(place) => {
                let _ = std::fs::remove_dir_all(&d.dir);
                notify(&d.items, &place);
                (
                    true,
                    place
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                )
            }
            Err(e) => (false, e.to_string()),
        }
    }
}

impl Incoming {
    fn entry(&mut self, path: &str, dir: bool, size: u64) -> io::Result<()> {
        if self.file.as_ref().is_some_and(|f| f.1 != 0) {
            return Err(io::Error::other("the previous file came short"));
        }
        self.file = None;
        let top = path.split('/').next().unwrap_or_default();
        if !self
            .items
            .iter()
            .any(|(n, d)| n == top && (*d || path == top))
        {
            return Err(io::Error::other("not part of this drop"));
        }
        let p = self
            .dir
            .join(path.replace('/', std::path::MAIN_SEPARATOR_STR));
        if dir {
            std::fs::create_dir_all(&p)
        } else {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let f = File::create(&p)?;
            self.files += 1;
            self.file = Some((f, size));
            Ok(())
        }
    }

    fn data(&mut self, data: &[u8]) -> io::Result<()> {
        let Some((f, left)) = self.file.as_mut() else {
            return Err(io::Error::other("data without a file"));
        };
        if data.len() as u64 > *left {
            return Err(io::Error::other("more data than announced"));
        }
        f.write_all(data)?;
        *left -= data.len() as u64;
        Ok(())
    }
}

/// Where staging folders go.
fn staging_base() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::temp_dir();
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("vroom-drop")
}

fn sweep(base: &Path) {
    let Ok(rd) = std::fs::read_dir(base) else {
        return;
    };
    for e in rd.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|a| a > KEEP);
        if old {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// The folder dropped files land in without a live drag: the Desktop.
fn landing() -> PathBuf {
    #[cfg(windows)]
    {
        let home = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let desk = home.join("Desktop");
        if desk.is_dir() {
            return desk;
        }
        home
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        // xdg-user-dirs: XDG_DESKTOP_DIR="$HOME/Desktop" (localized).
        let dirs = std::fs::read_to_string(home.join(".config/user-dirs.dirs")).unwrap_or_default();
        let desk = dirs
            .lines()
            .find_map(|l| l.strip_prefix("XDG_DESKTOP_DIR="))
            .map(|v| {
                v.trim()
                    .trim_matches('"')
                    .replace("$HOME", &home.to_string_lossy())
            })
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("Desktop"));
        if desk.is_dir() {
            desk
        } else {
            home
        }
    }
}

/// Move the drop's items into the landing folder, renaming on clashes
/// ("photo (2).jpg"). Returns the folder.
fn land(staging: &Path, items: &[(String, bool)]) -> io::Result<PathBuf> {
    let to = landing();
    for (name, _) in items {
        let target = free_name(&to, name);
        let from = staging.join(name);
        if std::fs::rename(&from, &target).is_err() {
            // Another file system: copy.
            copy_tree(&from, &target)?;
        }
    }
    Ok(to)
}

fn free_name(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    if !p.exists() {
        return p;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (2..10_000)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .unwrap_or(p)
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)? {
            let e = e?;
            copy_tree(&e.path(), &to.join(e.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

/// Say where the files went (Linux desktops: a notification).
fn notify(items: &[(String, bool)], place: &Path) {
    #[cfg(target_os = "linux")]
    {
        let what = match items {
            [(n, _)] => n.clone(),
            _ => format!("{} items", items.len()),
        };
        let _ = std::process::Command::new("notify-send")
            .arg("--app-name=vroom")
            .arg(format!("{what} copied from the Mac"))
            .arg(format!("In {}", place.display()))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (items, place);
}

#[cfg(windows)]
fn live_drag(dir: &Path, items: &[(String, bool)]) -> Option<Box<dyn LiveDrag>> {
    crate::dnd_win::start(dir, items).map(|d| Box::new(d) as Box<dyn LiveDrag>)
}

#[cfg(not(windows))]
fn live_drag(_: &Path, _: &[(String, bool)]) -> Option<Box<dyn LiveDrag>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use vroom_proto::Decoder;

    fn answers(out: &[u8]) -> Vec<Message> {
        let mut d = Decoder::new(1 << 20);
        d.push(out);
        std::iter::from_fn(|| d.next().unwrap()).collect()
    }

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("vroom-drop-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    struct FakeLive;
    impl LiveDrag for FakeLive {
        fn cancel(&mut self) {}
    }

    #[test]
    fn a_live_drop_is_staged_with_its_tree() {
        let base = tmp("live");
        let mut d = Drops::with(base.clone(), |_, _| Some(Box::new(FakeLive)));
        let mut out = Vec::new();
        let items = vec![("a.txt".to_string(), false), ("dir".to_string(), true)];
        d.handle(&Message::DragEnter { id: 7, items }, &mut out)
            .unwrap();
        let staged = std::fs::read_dir(&base)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(staged.join("a.txt").is_file() && staged.join("dir").is_dir());
        for m in [
            Message::DropEntry {
                id: 7,
                path: "a.txt".into(),
                dir: false,
                size: 5,
            },
            Message::DropData {
                id: 7,
                data: b"hel".to_vec(),
            },
            Message::DropData {
                id: 7,
                data: b"lo".to_vec(),
            },
            Message::DropEntry {
                id: 7,
                path: "dir/sub".into(),
                dir: true,
                size: 0,
            },
            Message::DropEntry {
                id: 7,
                path: "dir/sub/b".into(),
                dir: false,
                size: 0,
            },
            Message::DropEnd { id: 7 },
        ] {
            d.handle(&m, &mut out).unwrap();
        }
        assert_eq!(
            answers(&out),
            vec![
                Message::DragReady { id: 7, live: true },
                Message::DropDone {
                    id: 7,
                    ok: true,
                    place: String::new()
                }
            ]
        );
        assert_eq!(std::fs::read(staged.join("a.txt")).unwrap(), b"hello");
        assert!(staged.join("dir/sub/b").is_file());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn strays_and_overruns_fail_the_drop_and_cancel_cleans_up() {
        let base = tmp("bad");
        let mut d = Drops::with(base.clone(), |_, _| Some(Box::new(FakeLive)));
        let mut out = Vec::new();
        d.handle(
            &Message::DragEnter {
                id: 1,
                items: vec![("a".into(), false)],
            },
            &mut out,
        )
        .unwrap();
        d.handle(
            &Message::DropEntry {
                id: 1,
                path: "a".into(),
                dir: false,
                size: 1,
            },
            &mut out,
        )
        .unwrap();
        d.handle(
            &Message::DropData {
                id: 1,
                data: b"too much".to_vec(),
            },
            &mut out,
        )
        .unwrap();
        d.handle(&Message::DropEnd { id: 1 }, &mut out).unwrap();
        assert!(matches!(
            answers(&out).last(),
            Some(Message::DropDone { ok: false, .. })
        ));

        // A path outside the announced names.
        let mut out = Vec::new();
        d.handle(
            &Message::DragEnter {
                id: 2,
                items: vec![("a".into(), false)],
            },
            &mut out,
        )
        .unwrap();
        d.handle(
            &Message::DropEntry {
                id: 2,
                path: "other".into(),
                dir: false,
                size: 0,
            },
            &mut out,
        )
        .unwrap();
        d.handle(&Message::DropEnd { id: 2 }, &mut out).unwrap();
        assert!(matches!(
            answers(&out).last(),
            Some(Message::DropDone { ok: false, .. })
        ));

        // A file can't hide under a name announced as a file.
        let mut out = Vec::new();
        d.handle(
            &Message::DragEnter {
                id: 3,
                items: vec![("a".into(), false)],
            },
            &mut out,
        )
        .unwrap();
        d.handle(
            &Message::DropEntry {
                id: 3,
                path: "a/b".into(),
                dir: false,
                size: 0,
            },
            &mut out,
        )
        .unwrap();
        d.handle(&Message::DropEnd { id: 3 }, &mut out).unwrap();
        assert!(matches!(
            answers(&out).last(),
            Some(Message::DropDone { ok: false, .. })
        ));

        d.handle(
            &Message::DragEnter {
                id: 4,
                items: vec![("x".into(), true)],
            },
            &mut Vec::new(),
        )
        .unwrap();
        d.handle(&Message::DragCancel { id: 4 }, &mut Vec::new())
            .unwrap();
        assert_eq!(std::fs::read_dir(&base).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn clashing_names_get_a_number() {
        let dir = tmp("names");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("photo.jpg"), b"").unwrap();
        std::fs::write(dir.join("photo (2).jpg"), b"").unwrap();
        assert_eq!(free_name(&dir, "photo.jpg"), dir.join("photo (3).jpg"));
        assert_eq!(free_name(&dir, "new"), dir.join("new"));
        std::fs::write(dir.join(".rc"), b"").unwrap();
        assert_eq!(free_name(&dir, ".rc"), dir.join(".rc (2)"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

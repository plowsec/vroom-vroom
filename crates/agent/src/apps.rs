//! Apps the agent starts by name for the host (`StartApp`), and the list
//! of them it gives the host (`Apps`) so the Coherence bar can suggest
//! names as one types.
//!
//! The host's text only ever picks one of the apps installed in the guest
//! (Windows: a Start menu shortcut or an app of shell:AppsFolder; Linux:
//! a desktop entry of the menu). It never becomes a command line, an
//! argument or a path: whatever the host sends, the most it can do is
//! start an app the user could start from the menu. See docs/SECURITY.md.

use vroom_proto::{GuestApp, APPS_MAX, APP_ICON_MAX};

/// One app of the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry<T> {
    /// The name the menu shows (in the session's language).
    pub name: String,
    /// Other names it answers to, after its own: Linux's generic name
    /// ("Text Editor"), keywords, the entry's file name.
    pub also: Vec<String>,
    /// Comes after the others that match as well (Linux: an app that
    /// runs in a terminal, Vim before gedit for "Text Editor" otherwise).
    pub lesser: bool,
    pub target: T,
}

/// How well `q` (lower case, trimmed) names `e`: lower is better.
fn rank<T>(e: &Entry<T>, q: &str) -> Option<u32> {
    let name = e.name.to_lowercase();
    let also: Vec<String> = e.also.iter().map(|a| a.to_lowercase()).collect();
    let word_start = |s: &str| s.split(|c: char| !c.is_alphanumeric()).any(|w| !w.is_empty() && w.starts_with(q));
    // As GNOME's search: the name first, then the other names.
    if name == q {
        Some(0)
    } else if name.starts_with(q) {
        Some(1)
    } else if word_start(&name) {
        Some(2)
    } else if also.iter().any(|a| a == q) {
        Some(3)
    } else if also.iter().any(|a| a.starts_with(q) || word_start(a)) {
        Some(4)
    } else if name.contains(q) {
        Some(5)
    } else {
        None
    }
}

/// The app `q` names best: its own name (whole, its start, a word's
/// start) over its other names ("calc": Calculator, not LibreOffice
/// Calc, whose keyword it is), a shorter name first ("Notepad" before
/// "Notepad++") unless it is a lesser one, then the first in the list
/// (the dirs that take precedence come first).
pub fn best<'a, T>(list: &'a [Entry<T>], q: &str) -> Option<&'a Entry<T>> {
    let q = q.trim().to_lowercase();
    if q.is_empty() {
        return None;
    }
    let mut found: Option<(u32, bool, usize, usize)> = None;
    for (i, e) in list.iter().enumerate() {
        let Some(r) = rank(e, &q) else { continue };
        let k = (r, e.lesser, e.name.chars().count(), i);
        if found.is_none_or(|f| k < f) {
            found = Some(k);
        }
    }
    found.map(|(_, _, _, i)| &list[i])
}

/// Whether a name is just a name (letters, digits, spaces and the few
/// marks app names have), not a path or a command line: Windows types
/// only such into Start's search when no app has the name.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn plain(n: &str) -> bool {
    !n.trim().is_empty() && n.chars().all(|c| c.is_alphanumeric() || " .-+&'()_".contains(c))
}

/// A name as the protocol takes it, if there is anything left of it.
pub fn clean_name(n: &str) -> Option<String> {
    let s: String = n.chars().filter(|c| !c.is_control()).collect();
    let mut s = s.trim().to_string();
    if s.len() > vroom_proto::START_MAX {
        let mut end = vroom_proto::START_MAX;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    vroom_proto::line_ok(&s).then_some(s)
}

/// The entries to tell the host about: one per name (the first), names
/// the protocol takes, sorted, at most `APPS_MAX`. Indexes into `list`.
pub fn for_host<T>(list: &[Entry<T>]) -> Vec<(String, usize)> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<(String, usize)> = Vec::new();
    for (i, e) in list.iter().enumerate() {
        let Some(n) = clean_name(&e.name) else { continue };
        if seen.insert(n.to_lowercase()) {
            out.push((n, i));
        }
    }
    out.sort_by_key(|a| a.0.to_lowercase());
    out.truncate(APPS_MAX);
    out
}

/// The list for the host, with icons made to fit.
pub fn host_list<T>(list: &[Entry<T>], mut icon: impl FnMut(&T) -> Option<crate::coherence::Icon>) -> Vec<GuestApp> {
    for_host(list)
        .into_iter()
        .map(|(name, i)| {
            let ic = icon(&list[i].target).and_then(|ic| crate::coherence::fit(ic.w, ic.h, &ic.rgba, APP_ICON_MAX));
            match ic {
                Some(ic) => GuestApp { name, w: ic.w, h: ic.h, rgba: ic.rgba },
                None => GuestApp { name, w: 0, h: 0, rgba: Vec::new() },
            }
        })
        .collect()
}

/// A PNG file's picture as RGBA, if it is one and not too big (icons
/// and logos: a few hundred pixels at most).
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub fn png(bytes: &[u8]) -> Option<crate::coherence::Icon> {
    let mut d = png::Decoder::new_with_limits(bytes, png::Limits { bytes: 8 << 20 });
    d.set_transformations(png::Transformations::normalize_to_color8());
    let mut r = d.read_info().ok()?;
    let (w, h) = (r.info().width, r.info().height);
    if w == 0 || h == 0 || w > 1024 || h > 1024 {
        return None;
    }
    let mut buf = vec![0; r.output_buffer_size()];
    let f = r.next_frame(&mut buf).ok()?;
    let px = &buf[..f.buffer_size()];
    let n = (w * h) as usize;
    let rgba: Vec<u8> = match f.color_type {
        png::ColorType::Rgba => px.to_vec(),
        png::ColorType::Rgb => px.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => px.as_chunks::<2>().0.iter().flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => px.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    (rgba.len() == n * 4).then_some(crate::coherence::Icon { w, h, rgba })
}

#[cfg(windows)]
use crate::apps_win as plat;
#[cfg(target_os = "linux")]
use crate::apps_xdg as plat;

/// Start the app `name` names best. False: none does (or it couldn't be
/// started); the caller may try another way. Takes a moment (the menu
/// is read again, so a new app is found): call it off the session loop.
#[cfg(any(windows, target_os = "linux"))]
pub fn start(name: &str) -> bool {
    let list = plat::scan();
    let Some(e) = best(&list, name) else {
        log::info!("apps: nothing installed is called {name:?} ({} apps)", list.len());
        return false;
    };
    log::info!("apps: {name:?} is {:?}", e.name);
    match plat::launch(e) {
        Ok(()) => true,
        Err(err) => {
            log::warn!("apps: can't start {:?}: {err}", e.name);
            false
        }
    }
}

/// The list for the host (reads the menu and every icon: a second or so).
#[cfg(any(windows, target_os = "linux"))]
pub fn list() -> Vec<GuestApp> {
    plat::with_icons(|list, icon| host_list(list, icon))
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn list() -> Vec<GuestApp> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(name: &str, also: &[&str]) -> Entry<u32> {
        Entry { name: name.into(), also: also.iter().map(|s| s.to_string()).collect(), lesser: false, target: 0 }
    }

    #[test]
    fn the_best_match_wins() {
        let list = vec![
            e("Notepad++", &[]),
            e("Notepad", &[]),
            e("Text Editor", &["gedit", "org.gnome.gedit", "Notepad"]),
            e("LibreOffice Calc", &["Spreadsheet", "Calc", "libreoffice-calc", "calc"]),
            e("Calculator", &["org.gnome.Calculator", "Calculator"]),
            e("GNU Image Manipulation Program", &["gimp"]),
            e("Visual Studio Code", &[]),
        ];
        let name = |q: &str| best(&list, q).map(|e| e.name.as_str());
        // Whole name over a longer one and over a keyword.
        assert_eq!(name("Notepad"), Some("Notepad"));
        assert_eq!(name("notepad++"), Some("Notepad++"));
        assert_eq!(name("  NOTEPAD "), Some("Notepad"));
        // The start of a name: the shorter one.
        assert_eq!(name("note"), Some("Notepad"));
        // Other names.
        assert_eq!(name("gedit"), Some("Text Editor"));
        assert_eq!(name("calc"), Some("Calculator"));
        assert_eq!(name("spreadsheet"), Some("LibreOffice Calc"));
        assert_eq!(name("libreoffice"), Some("LibreOffice Calc"));
        let vim = Entry { lesser: true, ..e("Vim", &["Text Editor"]) };
        assert_eq!(best(&[vim, e("gedit", &["Text Editor"])], "text editor").map(|e| e.name.as_str()), Some("gedit"), "not the terminal one");
        assert_eq!(name("gimp"), Some("GNU Image Manipulation Program"));
        // A word of the name.
        assert_eq!(name("code"), Some("Visual Studio Code"));
        assert_eq!(name("image"), Some("GNU Image Manipulation Program"));
        // Inside a name, last.
        assert_eq!(name("culat"), Some("Calculator"));
        assert_eq!(name("zzz"), None);
        assert_eq!(name(""), None);
        assert_eq!(name("   "), None);
    }

    #[test]
    fn pngs_are_read() {
        let mut b = Vec::new();
        {
            let mut e = png::Encoder::new(&mut b, 2, 1);
            e.set_color(png::ColorType::Rgb);
            e.set_depth(png::BitDepth::Eight);
            e.write_header().unwrap().write_image_data(&[1, 2, 3, 4, 5, 6]).unwrap();
        }
        let i = png(&b).unwrap();
        assert_eq!((i.w, i.h, i.rgba), (2, 1, vec![1, 2, 3, 255, 4, 5, 6, 255]));
        assert!(png(&b[..b.len() - 4]).is_none() || png(&b[..20]).is_none());
        assert!(png(b"not a png").is_none());
    }

    #[test]
    fn only_plain_names_are_searched_for() {
        for ok in ["Notepad", "Paint 3D", "Notepad++", "Microsoft Edge", "Visual Studio Code - Insiders", "Ég"] {
            assert!(plain(ok), "{ok}");
        }
        for bad in ["cmd /c del x", r"C:\x.exe", "a|b", "%TEMP%", "\"x\"", "a:b", "", " ", "a<b"] {
            assert!(!plain(bad), "{bad}");
        }
    }

    #[test]
    fn ties_go_to_the_first() {
        let mut a = e("Files", &[]);
        a.target = 1;
        let mut b = e("Files", &[]);
        b.target = 2;
        assert_eq!(best(&[a, b], "files").map(|e| e.target), Some(1));
    }

    #[test]
    fn the_host_gets_one_of_each_name() {
        let list = vec![e("b", &[]), e("A", &[]), e("a", &[]), e("bad\nname", &[]), e("  ", &[]), e(&"é".repeat(200), &[])];
        let h = for_host(&list);
        let names: Vec<&str> = h.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names[..3], ["A", "b", "badname"]);
        assert_eq!(h[0].1, 1, "the first of two names");
        assert!(h.len() == 4 && h[3].0.len() <= vroom_proto::START_MAX);
        let many: Vec<Entry<u32>> = (0..APPS_MAX + 10).map(|i| e(&format!("app {i}"), &[])).collect();
        assert_eq!(for_host(&many).len(), APPS_MAX);
        // What goes out always decodes.
        let red = crate::coherence::Icon { w: 64, h: 64, rgba: vec![255; 64 * 64 * 4] };
        let l = host_list(&list, |_| Some(red.clone()));
        assert!(l.iter().all(|a| a.w == APP_ICON_MAX && a.h == APP_ICON_MAX));
        assert!(vroom_proto::decode_apps(&vroom_proto::encode_apps(&l)).is_some());
    }
}

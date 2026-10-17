//! Linux: the apps of the desktop's menu, from their desktop entries
//! (freedesktop.org "Desktop Entry" files in the XDG data dirs), and
//! starting one as the desktop does.
//!
//! An entry is found by its name (the session's language first), its
//! generic name, its keywords or its file name. Entries the menu hides
//! (NoDisplay, Hidden, OnlyShowIn another desktop, a TryExec that isn't
//! there) are left out. It starts with `gio launch` (what GNOME's own
//! launchers do: terminal apps, D-Bus activation) or else with the
//! entry's Exec line, split into words and its field codes taken out,
//! run directly: never through a shell. Nothing of the host's text goes
//! into it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::apps::Entry;
use crate::coherence::Icon;

/// What starting an entry takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The .desktop file.
    pub path: PathBuf,
    /// Its Exec line as words, field codes out.
    pub exec: Vec<String>,
    /// Run in a terminal.
    pub terminal: bool,
    /// Its working directory (`Path`).
    pub dir: Option<String>,
    /// Its icon: a name in the icon theme, or a file.
    pub icon: Option<String>,
}

/// Most entries read, and the biggest file taken.
const MAX_ENTRIES: usize = 4096;
const MAX_FILE: u64 = 256 << 10;

/// The `[Desktop Entry]` group of a desktop file: key (with its locale,
/// "Name[de]") to raw value.
fn group(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            // Only the first group counts; actions etc. come after it.
            if inside {
                break;
            }
            inside = line.trim_end() == "[Desktop Entry]";
            continue;
        }
        if !inside {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.push((k.trim().to_string(), v.trim_start().to_string()));
        }
    }
    out
}

/// A value's escapes (`\s`, `\n`, `\t`, `\r`, `\\`).
fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut it = v.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// A list value (`a;b;c;`), `\;` being a `;` in a value.
fn list(v: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut it = v.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\\' if it.peek() == Some(&';') => {
                cur.push(';');
                it.next();
            }
            ';' => out.push(unescape(&std::mem::take(&mut cur))),
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(unescape(&cur));
    }
    out.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// The locale names to try, best first, from a POSIX locale
/// ("de_CH.UTF-8@euro": de_CH@euro, de_CH, de@euro, de).
pub fn locales(posix: &str) -> Vec<String> {
    let (rest, modifier) = match posix.split_once('@') {
        Some((r, m)) => (r, Some(m)),
        None => (posix, None),
    };
    let rest = rest.split('.').next().unwrap_or("");
    let (lang, country) = match rest.split_once('_') {
        Some((l, c)) => (l, Some(c)),
        None => (rest, None),
    };
    if lang.is_empty() || lang == "C" || lang == "POSIX" {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let (Some(c), Some(m)) = (country, modifier) {
        out.push(format!("{lang}_{c}@{m}"));
    }
    if let Some(c) = country {
        out.push(format!("{lang}_{c}"));
    }
    if let Some(m) = modifier {
        out.push(format!("{lang}@{m}"));
    }
    out.push(lang.to_string());
    out
}

/// The session's locale names (LC_ALL, LC_MESSAGES, LANG).
fn session_locales() -> Vec<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .map(|v| locales(&v))
        .unwrap_or_default()
}

/// A key's value in the best locale, else as is.
fn get<'a>(g: &'a [(String, String)], key: &str, locs: &[String]) -> Option<&'a str> {
    for l in locs {
        let k = format!("{key}[{l}]");
        if let Some((_, v)) = g.iter().find(|(gk, _)| *gk == k) {
            return Some(v);
        }
    }
    g.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn yes(v: Option<&str>) -> bool {
    v.is_some_and(|v| v.trim() == "true")
}

/// An Exec line as words: spaces split, `"..."` holds spaces (with `\"`,
/// `` \` ``, `\$`, `\\` inside). Field codes: `%f %F %u %U` (files: none)
/// and the old ones go, `%i` is the icon, `%c` the name, `%k` the file,
/// `%%` a `%`. None if nothing is left to run.
pub fn exec_words(exec: &str, icon: Option<&str>, name: &str, file: &str) -> Option<Vec<String>> {
    // Words first, keeping what was quoted (codes aren't read there).
    let mut words: Vec<(String, bool)> = Vec::new();
    let mut cur = String::new();
    let (mut quoted, mut in_quote, mut any) = (false, false, false);
    let mut it = exec.chars();
    while let Some(c) = it.next() {
        match c {
            '"' => {
                in_quote = !in_quote;
                quoted = true;
                any = true;
            }
            '\\' if in_quote => match it.next() {
                Some(n @ ('"' | '`' | '$' | '\\')) => cur.push(n),
                Some(n) => {
                    cur.push('\\');
                    cur.push(n);
                }
                None => return None,
            },
            ' ' | '\t' if !in_quote => {
                if any {
                    words.push((std::mem::take(&mut cur), quoted));
                }
                quoted = false;
                any = false;
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if in_quote {
        return None;
    }
    if any {
        words.push((cur, quoted));
    }
    let mut out = Vec::new();
    for (w, quoted) in words {
        if quoted {
            out.push(w);
            continue;
        }
        if w == "%i" {
            if let Some(i) = icon.filter(|i| !i.is_empty()) {
                out.push("--icon".into());
                out.push(i.to_string());
            }
            continue;
        }
        let mut s = String::new();
        let mut it = w.chars();
        let mut only_codes = true;
        while let Some(c) = it.next() {
            if c != '%' {
                s.push(c);
                only_codes = false;
                continue;
            }
            match it.next() {
                Some('%') => {
                    s.push('%');
                    only_codes = false;
                }
                Some('c') => {
                    s.push_str(name);
                    only_codes = false;
                }
                Some('k') => {
                    s.push_str(file);
                    only_codes = false;
                }
                // Files and URLs (we pass none), and the deprecated ones.
                Some(_) | None => {}
            }
        }
        if !(only_codes && s.is_empty()) {
            out.push(s);
        }
    }
    (!out.is_empty() && !out[0].is_empty()).then_some(out)
}

/// The desktops of this session (XDG_CURRENT_DESKTOP: "ubuntu:GNOME").
fn current_desktops() -> Vec<String> {
    std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().split(':').filter(|s| !s.is_empty()).map(str::to_string).collect()
}

/// Whether a program is there (a path, or a name on PATH).
fn found(prog: &str) -> bool {
    if prog.contains('/') {
        return Path::new(prog).is_file();
    }
    which(prog).is_some()
}

fn which(prog: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
    std::env::split_paths(&path).filter(|d| d.is_absolute()).map(|d| d.join(prog)).find(|p| p.is_file())
}

/// One desktop file as an entry of the menu, or None if the menu leaves
/// it out. `id`: its desktop file id ("org.gnome.Calculator.desktop").
pub fn entry(text: &str, id: &str, path: &Path, locs: &[String], desktops: &[String], exists: impl Fn(&str) -> bool) -> Option<Entry<Target>> {
    let g = group(text);
    if get(&g, "Type", &[]).map(str::trim) != Some("Application") || yes(get(&g, "NoDisplay", &[])) || yes(get(&g, "Hidden", &[])) {
        return None;
    }
    if let Some(only) = get(&g, "OnlyShowIn", &[]) {
        if !list(only).iter().any(|d| desktops.contains(d)) {
            return None;
        }
    }
    if let Some(not) = get(&g, "NotShowIn", &[]) {
        if list(not).iter().any(|d| desktops.contains(d)) {
            return None;
        }
    }
    if let Some(t) = get(&g, "TryExec", &[]).map(unescape) {
        if !t.trim().is_empty() && !exists(t.trim()) {
            return None;
        }
    }
    let name = unescape(get(&g, "Name", locs)?).trim().to_string();
    if name.is_empty() {
        return None;
    }
    let icon = get(&g, "Icon", &[]).map(unescape).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let file = path.to_string_lossy();
    let exec = exec_words(&unescape(get(&g, "Exec", &[])?), icon.as_deref(), &name, &file)?;
    let mut also = Vec::new();
    if let Some(gn) = get(&g, "GenericName", locs).map(unescape) {
        also.push(gn.trim().to_string());
    }
    // The untranslated ones too: "Text Editor" works in any language.
    for k in ["Name", "GenericName"] {
        if let Some(v) = get(&g, k, &[]).map(unescape) {
            also.push(v.trim().to_string());
        }
    }
    if let Some(k) = get(&g, "Keywords", locs) {
        also.extend(list(k));
    }
    // "org.gnome.Calculator", and its last part ("Calculator", "gedit").
    let stem = id.strip_suffix(".desktop").unwrap_or(id);
    also.push(stem.to_string());
    if let Some(last) = stem.rsplit(['.', '-']).next() {
        also.push(last.to_string());
    }
    let mut seen = HashSet::new();
    also.retain(|a| !a.is_empty() && *a != name && seen.insert(a.clone()));
    let terminal = yes(get(&g, "Terminal", &[]));
    let dir = get(&g, "Path", &[]).map(unescape).filter(|d| !d.trim().is_empty());
    Some(Entry { name, also, target: Target { path: path.to_path_buf(), exec, terminal, dir, icon } })
}

/// The XDG data dirs, the one that takes precedence first: the user's
/// (XDG_DATA_HOME), then XDG_DATA_DIRS, then where Flatpak and Snap put
/// theirs if the session didn't say.
fn data_dirs() -> Vec<PathBuf> {
    let abs = |s: &str| Some(PathBuf::from(s)).filter(|p| p.is_absolute());
    let home = std::env::var("HOME").ok().and_then(|h| abs(&h));
    let mut out = Vec::new();
    match std::env::var("XDG_DATA_HOME").ok().and_then(|d| abs(&d)) {
        Some(d) => out.push(d),
        None => out.extend(home.as_ref().map(|h| h.join(".local/share"))),
    }
    let dirs = std::env::var("XDG_DATA_DIRS").ok().filter(|d| !d.is_empty()).unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    out.extend(dirs.split(':').filter_map(abs));
    out.extend(home.map(|h| h.join(".local/share/flatpak/exports/share")));
    out.extend(["/var/lib/flatpak/exports/share", "/var/lib/snapd/desktop"].iter().filter_map(|d| abs(d)));
    let mut seen = HashSet::new();
    out.retain(|d| seen.insert(d.clone()));
    out
}

/// Desktop files under `dir` (and its folders, a few deep): (id, path).
fn walk(dir: &Path, prefix: &str, depth: u32, out: &mut Vec<(String, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut items: Vec<_> = rd.flatten().collect();
    items.sort_by_key(|e| e.file_name());
    for e in items {
        if out.len() >= MAX_ENTRIES {
            return;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        // Snap and Flatpak make links into their own trees: follow files.
        let is_dir = ft.is_dir() || ft.is_symlink() && p.is_dir();
        if is_dir && depth < 3 {
            walk(&p, &format!("{prefix}{name}-"), depth + 1, out);
        } else if name.ends_with(".desktop") && p.is_file() {
            out.push((format!("{prefix}{name}"), p));
        }
    }
}

/// The menu's apps, in precedence order (an id found again in a later
/// dir is the same app, already had).
pub fn scan() -> Vec<Entry<Target>> {
    let (locs, desktops) = (session_locales(), current_desktops());
    let mut ids = HashSet::new();
    let mut out = Vec::new();
    for d in data_dirs() {
        let mut files = Vec::new();
        walk(&d.join("applications"), "", 0, &mut files);
        for (id, path) in files {
            // A hidden one hides the same id further down, too.
            if !ids.insert(id.clone()) {
                continue;
            }
            if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_FILE) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            if let Some(e) = entry(&text, &id, &path, &locs, &desktops, found) {
                out.push(e);
            }
        }
    }
    out
}

/// Icon sizes to look for, best first.
const SIZES: &[u32] = &[32, 48, 64, 24, 128, 96, 256, 22, 16, 512];

/// Themes to look in, after the user's GTK one (if we can tell it).
const THEMES: &[&str] = &["Yaru", "Adwaita", "hicolor", "breeze", "Papirus", "elementary", "gnome", "HighContrast"];

/// The icon theme GTK uses (settings.ini), if set there.
fn gtk_theme() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let t = std::fs::read_to_string(Path::new(&home).join(".config/gtk-3.0/settings.ini")).ok()?;
    t.lines().find_map(|l| l.trim().strip_prefix("gtk-icon-theme-name").and_then(|r| r.trim().strip_prefix('=')).map(|v| v.trim().trim_matches('"').to_string()))
}

/// Where an icon name's PNG files may be, best first.
pub fn icon_paths(name: &str, bases: &[PathBuf], themes: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for t in themes {
        for b in bases {
            let d = b.join(t);
            for s in SIZES {
                out.push(d.join(format!("{s}x{s}/apps/{name}.png")));
                out.push(d.join(format!("{s}x{s}@2x/apps/{name}.png")));
                out.push(d.join(format!("apps/{s}/{name}.png")));
            }
        }
    }
    for b in bases {
        out.push(b.join(format!("../pixmaps/{name}.png")));
    }
    out
}

/// An entry's icon: a PNG file it names, or its name in an icon theme.
pub fn icon(t: &Target) -> Option<Icon> {
    let name = t.icon.as_deref()?;
    let read = |p: &Path| -> Option<Icon> {
        if std::fs::metadata(p).ok()?.len() > 4 << 20 {
            return None;
        }
        crate::apps::png(&std::fs::read(p).ok()?)
    };
    if name.starts_with('/') {
        return read(Path::new(name));
    }
    // A name, not a path into somewhere else.
    if name.contains('/') || name.starts_with('.') {
        return None;
    }
    let mut bases: Vec<PathBuf> = Vec::new();
    if let Ok(h) = std::env::var("HOME") {
        bases.push(Path::new(&h).join(".icons"));
    }
    bases.extend(data_dirs().into_iter().map(|d| d.join("icons")));
    let mut themes: Vec<String> = gtk_theme().into_iter().collect();
    themes.extend(THEMES.iter().map(|s| s.to_string()));
    let mut seen = HashSet::new();
    themes.retain(|t| seen.insert(t.clone()));
    icon_paths(name, &bases, &themes).iter().find(|p| p.is_file()).and_then(|p| read(p))
}

/// The scan and an icon reader, for the host's list.
pub fn with_icons<R>(f: impl FnOnce(&[Entry<Target>], &mut dyn FnMut(&Target) -> Option<Icon>) -> R) -> R {
    let list = scan();
    f(&list, &mut icon)
}

/// Start it, as the session's user (this process is the session helper).
pub fn launch(e: &Entry<Target>) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    // SAFETY: a plain call.
    #[allow(unsafe_code)]
    if unsafe { libc::geteuid() } == 0 {
        // The agent runs as root only without a session helper: an app
        // started from here would be root's.
        return Err("the agent runs as root here, not as the desktop's user".into());
    }
    let t = &e.target;
    let mut c = if which("gio").is_some() {
        let mut c = Command::new("gio");
        c.arg("launch").arg(&t.path);
        c
    } else {
        let mut words = t.exec.clone();
        if t.terminal {
            let term = which("x-terminal-emulator").ok_or("a terminal app, and no terminal to run it in")?;
            words.splice(0..0, [term.to_string_lossy().into_owned(), "-e".into()]);
        }
        let mut c = Command::new(&words[0]);
        c.args(&words[1..]);
        c
    };
    // Where the entry says (gio reads that itself), else the home folder,
    // as from the desktop.
    let home = std::env::var("HOME").ok();
    if let Some(d) = t.dir.iter().chain(home.iter()).find(|d| Path::new(d).is_dir()) {
        c.current_dir(d);
    }
    // Its own process group: it outlives us, and our signals aren't its.
    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0);
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    log::info!("apps: started {:?} ({})", e.name, t.path.display());
    // Reaped when it ends (gio ends right away; the app stays).
    let _ = std::thread::Builder::new().name("app-reaper".into()).spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GEDIT: &str = "[Desktop Entry]
Name=Text Editor
Name[de]=Texteditor
Name[de_CH]=Texteditor (CH)
GenericName=Text Editor
GenericName[de]=Editor
Comment=Edit text files
Keywords=Text;Editor;Plaintext;Write;gedit;
Keywords[de]=Text;Bearbeiten;
Exec=gedit %U
Terminal=false
Type=Application
Icon=org.gnome.gedit

[Desktop Action new-window]
Name=New Window
Exec=gedit --new-window
";

    fn ent(text: &str) -> Option<Entry<Target>> {
        entry(text, "org.gnome.gedit.desktop", Path::new("/usr/share/applications/org.gnome.gedit.desktop"), &locales("de_CH.UTF-8"), &["GNOME".into()], |_| true)
    }

    #[test]
    fn entries_are_read_in_the_sessions_language() {
        let e = ent(GEDIT).unwrap();
        assert_eq!(e.name, "Texteditor (CH)");
        assert_eq!(e.target.exec, ["gedit"]);
        assert_eq!(e.target.icon.as_deref(), Some("org.gnome.gedit"));
        for a in ["Editor", "Text Editor", "Bearbeiten", "org.gnome.gedit", "gedit"] {
            assert!(e.also.iter().any(|x| x == a), "{a} in {:?}", e.also);
        }
        // The action's Exec isn't the app's.
        assert!(!e.also.iter().any(|x| x.contains("New")));
        let c = entry(GEDIT, "gedit.desktop", Path::new("/x"), &locales("C.UTF-8"), &[], |_| true).unwrap();
        assert_eq!(c.name, "Text Editor");
    }

    #[test]
    fn hidden_entries_are_left_out() {
        let with = |extra: &str| ent(&GEDIT.replacen("Type=Application", &format!("Type=Application\n{extra}"), 1));
        assert!(with("NoDisplay=true").is_none());
        assert!(with("Hidden=true").is_none());
        assert!(with("NoDisplay=false").is_some());
        assert!(with("OnlyShowIn=KDE;XFCE;").is_none());
        assert!(with("OnlyShowIn=GNOME;").is_some());
        assert!(with("NotShowIn=GNOME;").is_none());
        assert!(ent(&GEDIT.replace("Type=Application", "Type=Link")).is_none());
        assert!(ent(&GEDIT.replace("Exec=gedit %U", "Exec=%U")).is_none(), "nothing to run");
        assert!(ent(&GEDIT.replace("Exec=gedit %U\n", "")).is_none());
        let t = GEDIT.replace("Type=Application", "Type=Application\nTryExec=/nope");
        assert!(entry(&t, "a.desktop", Path::new("/a"), &[], &[], |p| p != "/nope").is_none());
        // Keys before the group, or in another one, don't count.
        assert!(ent("Type=Application\nName=x\nExec=x\n[Other]\nName=y").is_none());
    }

    #[test]
    fn exec_lines_become_words_without_a_shell() {
        let w = |s: &str| exec_words(s, Some("icon-x"), "My App", "/a/b.desktop");
        assert_eq!(w("gedit %U").unwrap(), ["gedit"]);
        assert_eq!(w("/usr/bin/app --flag %f").unwrap(), ["/usr/bin/app", "--flag"]);
        assert_eq!(w("app %i --title=%c %k").unwrap(), ["app", "--icon", "icon-x", "--title=My App", "/a/b.desktop"]);
        assert_eq!(w("app 100%%").unwrap(), ["app", "100%"]);
        assert_eq!(w(r#""/opt/My App/run" "a b" "%f""#).unwrap(), ["/opt/My App/run", "a b", "%f"]);
        assert_eq!(w(r#"sh -c "echo \"hi\" \$HOME; rm -rf /""#).unwrap(), ["sh", "-c", r#"echo "hi" $HOME; rm -rf /"#], "one word for sh, as the entry says");
        // Shell syntax is plain text: nothing runs it.
        assert_eq!(w("app ; reboot && x|y $(z)").unwrap(), ["app", ";", "reboot", "&&", "x|y", "$(z)"]);
        assert_eq!(w(r#"app "unclosed"#), None);
        assert_eq!(w("   "), None);
        assert_eq!(w("%F"), None);
        assert_eq!(w("env A=1 app %u"), Some(vec!["env".into(), "A=1".into(), "app".into()]));
        assert_eq!(exec_words("app %i", None, "n", "f").unwrap(), ["app"]);
    }

    #[test]
    fn values_and_lists_unescape() {
        assert_eq!(unescape(r"a\sb\\c\n"), "a b\\c\n");
        assert_eq!(list(r"a;b\;c;;d"), ["a", "b;c", "d"]);
        assert_eq!(locales("de_CH.UTF-8@euro"), ["de_CH@euro", "de_CH", "de@euro", "de"]);
        assert_eq!(locales("fr"), ["fr"]);
        assert!(locales("C.UTF-8").is_empty() && locales("").is_empty());
    }

    #[test]
    fn icons_are_looked_for_by_size_and_theme() {
        let p = icon_paths("calc", &[PathBuf::from("/usr/share/icons")], &["Yaru".into(), "hicolor".into()]);
        assert_eq!(p[0], PathBuf::from("/usr/share/icons/Yaru/32x32/apps/calc.png"));
        assert!(p.contains(&PathBuf::from("/usr/share/icons/hicolor/48x48/apps/calc.png")));
        assert_eq!(p.last(), Some(&PathBuf::from("/usr/share/icons/../pixmaps/calc.png")));
        let t = Target { path: "/x".into(), exec: vec!["x".into()], terminal: false, dir: None, icon: Some("../../etc/passwd".into()) };
        assert!(icon(&t).is_none());
    }

    #[test]
    fn a_tree_of_entries_is_scanned_with_ids() {
        let d = std::env::temp_dir().join(format!("vroom-apps-{}", std::process::id()));
        let apps = d.join("applications");
        std::fs::create_dir_all(apps.join("kde")).unwrap();
        std::fs::write(apps.join("org.gnome.gedit.desktop"), GEDIT).unwrap();
        std::fs::write(apps.join("kde/kate.desktop"), "[Desktop Entry]\nType=Application\nName=Kate\nExec=kate %U\n").unwrap();
        let mut files = Vec::new();
        walk(&apps, "", 0, &mut files);
        let ids: Vec<&str> = files.iter().map(|f| f.0.as_str()).collect();
        assert_eq!(ids, ["kde-kate.desktop", "org.gnome.gedit.desktop"]);
        let _ = std::fs::remove_dir_all(&d);
    }
}

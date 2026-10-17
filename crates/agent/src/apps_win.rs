//! Windows: the Start menu's apps, and starting one as Start does.
//!
//! Two sources, as Start's own list: the shortcuts (.lnk) in the Start
//! menu folders (all users' in ProgramData, the user's in AppData), and
//! the apps of shell:AppsFolder (Store apps, by their AppUserModelID, and
//! the few others only it knows). A shortcut is opened as a double click
//! would; an AppsFolder app through "shell:AppsFolder\<its id>".

#![allow(unsafe_code)]

use std::cell::Cell;
use std::ffi::c_void;
use std::path::{Path, PathBuf};

use windows_sys::core::{GUID, HRESULT, PWSTR};
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};
use windows_sys::Win32::Storage::Packaging::Appx::{GetPackagePathByFullName, GetPackagesByPackageFamily};
use windows_sys::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows_sys::Win32::UI::Shell::{
    SHCreateItemFromParsingName, SHGetFileInfoW, SHGetIDListFromObject, ShellExecuteW, BHID_EnumItems, SHFILEINFOW, SHGFI_DISPLAYNAME, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_PIDL,
    SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, DestroyIcon, ASFW_ANY, SW_SHOWNORMAL};

use crate::apps::Entry;
use crate::coherence::Icon;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A Start menu shortcut.
    Lnk(PathBuf),
    /// An app of shell:AppsFolder, by its parsing name (a Store app's
    /// AppUserModelID: "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App").
    App(String),
}

const IID_ISHELLITEM: GUID = GUID::from_u128(0x43826d1e_e718_42ee_bc55_a1e261c37bfe);
const IID_IENUMSHELLITEMS: GUID = GUID::from_u128(0x70629033_e363_4a28_a567_0db78006e6d7);

/// Most shortcuts and apps read.
const MAX_ENTRIES: usize = 2048;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn wide_path(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain([0]).collect()
}

/// COM on this thread (the shell's objects want an apartment), once.
fn com() {
    thread_local!(static ON: Cell<bool> = const { Cell::new(false) });
    if !ON.get() {
        // SAFETY: a plain call. Already on (or the other kind): fine
        // either way; we never take it down (the thread is short-lived).
        unsafe { CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32) };
        ON.set(true);
    }
}

/// A COM object's method `n` (vtable slot).
///
/// # Safety
/// `obj` is a live COM object whose slot `n` has the type `F`.
unsafe fn method<F: Copy>(obj: *mut c_void, n: usize) -> F {
    // SAFETY: the caller's word; a vtable is an array of function pointers.
    unsafe {
        let vtbl = *(obj as *const *const usize);
        std::mem::transmute_copy(&*vtbl.add(n))
    }
}

/// IUnknown::Release.
fn release(obj: *mut c_void) {
    if obj.is_null() {
        return;
    }
    // SAFETY: slot 2 of any COM object.
    unsafe {
        let f: unsafe extern "system" fn(*mut c_void) -> u32 = method(obj, 2);
        f(obj);
    }
}

/// IShellItem::GetDisplayName.
fn display_name(item: *mut c_void, how: i32) -> Option<String> {
    let mut p: PWSTR = std::ptr::null_mut();
    // SAFETY: slot 5 of IShellItem; the string is ours to free.
    unsafe {
        let f: unsafe extern "system" fn(*mut c_void, i32, *mut PWSTR) -> HRESULT = method(item, 5);
        if f(item, how, &mut p) < 0 || p.is_null() {
            return None;
        }
        let n = (0..).take_while(|&i| *p.add(i) != 0).count();
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
        CoTaskMemFree(p as *const c_void);
        Some(s)
    }
}

fn shell_item(path: &str) -> Option<*mut c_void> {
    let w = wide(path);
    let mut item: *mut c_void = std::ptr::null_mut();
    // SAFETY: a NUL-terminated name, the IID we want, an out pointer.
    let hr = unsafe { SHCreateItemFromParsingName(w.as_ptr(), std::ptr::null_mut(), &IID_ISHELLITEM, &mut item) };
    (hr >= 0 && !item.is_null()).then_some(item)
}

/// The apps of shell:AppsFolder: (display name, parsing name).
fn apps_folder() -> Vec<(String, String)> {
    com();
    let mut out = Vec::new();
    let Some(folder) = shell_item("shell:AppsFolder") else { return out };
    let mut en: *mut c_void = std::ptr::null_mut();
    // SAFETY: IShellItem::BindToHandler (slot 3) for its items' enumerator.
    let hr = unsafe {
        let f: unsafe extern "system" fn(*mut c_void, *mut c_void, *const GUID, *const GUID, *mut *mut c_void) -> HRESULT = method(folder, 3);
        f(folder, std::ptr::null_mut(), &BHID_EnumItems, &IID_IENUMSHELLITEMS, &mut en)
    };
    if hr >= 0 && !en.is_null() {
        while out.len() < MAX_ENTRIES {
            let mut item: *mut c_void = std::ptr::null_mut();
            let mut got = 0u32;
            // SAFETY: IEnumShellItems::Next (slot 3): one item, ours to release.
            let hr = unsafe {
                let f: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void, *mut u32) -> HRESULT = method(en, 3);
                f(en, 1, &mut item, &mut got)
            };
            // S_FALSE (1): no more.
            if hr != 0 || got != 1 || item.is_null() {
                break;
            }
            if let (Some(n), Some(p)) = (display_name(item, SIGDN_NORMALDISPLAY), display_name(item, SIGDN_PARENTRELATIVEPARSING)) {
                out.push((n, p));
            }
            release(item);
        }
        release(en);
    }
    release(folder);
    out
}

/// The Start menu folders: the user's first.
fn start_menus() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for v in ["APPDATA", "ProgramData"] {
        if let Some(d) = std::env::var_os(v) {
            out.push(PathBuf::from(d).join(r"Microsoft\Windows\Start Menu\Programs"));
        }
    }
    out
}

fn walk(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut items: Vec<_> = rd.flatten().collect();
    items.sort_by_key(|e| e.file_name());
    for e in items {
        if out.len() >= MAX_ENTRIES {
            return;
        }
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() && depth < 6 {
            walk(&p, depth + 1, out);
        } else if ft.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk")) {
            out.push(p);
        }
    }
}

/// Start's apps: shortcuts, then the AppsFolder apps no shortcut has
/// the name of.
pub fn scan() -> Vec<Entry<Target>> {
    let mut out: Vec<Entry<Target>> = Vec::new();
    let mut lnks = Vec::new();
    for d in start_menus() {
        walk(&d, 0, &mut lnks);
    }
    for p in lnks {
        // The name Start shows (desktop.ini may give it another one in
        // the user's language: "dfrgui" is "Defragment and Optimize
        // Drives"), else the file's.
        let Some(stem) = p.file_stem().map(|s| s.to_string_lossy().into_owned()) else { continue };
        let name = shown_name(&p).filter(|n| !n.trim().is_empty()).unwrap_or_else(|| stem.clone());
        let also = if name == stem { Vec::new() } else { vec![stem] };
        out.push(Entry { name, also, target: Target::Lnk(p) });
    }
    let mut names: std::collections::HashSet<String> = out.iter().map(|e| e.name.to_lowercase()).collect();
    for (name, id) in apps_folder() {
        if names.insert(name.to_lowercase()) {
            out.push(Entry { name, also: Vec::new(), target: Target::App(id) });
        }
    }
    out
}

/// The name the shell shows for a file.
fn shown_name(p: &Path) -> Option<String> {
    let w = wide_path(p);
    // SAFETY: zeroed SHFILEINFOW is valid; a NUL-terminated path.
    unsafe {
        let mut fi: SHFILEINFOW = std::mem::zeroed();
        if SHGetFileInfoW(w.as_ptr(), 0, &mut fi, std::mem::size_of::<SHFILEINFOW>() as u32, SHGFI_DISPLAYNAME) == 0 {
            return None;
        }
        let n = fi.szDisplayName.iter().position(|&c| c == 0).unwrap_or(fi.szDisplayName.len());
        Some(String::from_utf16_lossy(&fi.szDisplayName[..n]))
    }
}

/// A shell icon (SHGetFileInfo) as RGBA. `pidl`: `path` is an item id list.
fn shell_icon(path: *const u16, pidl: bool) -> Option<Icon> {
    // SAFETY: zeroed SHFILEINFOW is valid; the icon is ours to destroy.
    unsafe {
        let mut fi: SHFILEINFOW = std::mem::zeroed();
        let flags = SHGFI_ICON | SHGFI_LARGEICON | if pidl { SHGFI_PIDL } else { 0 };
        if SHGetFileInfoW(path, 0, &mut fi, std::mem::size_of::<SHFILEINFOW>() as u32, flags) == 0 || fi.hIcon.is_null() {
            return None;
        }
        let i = crate::coh_win::render(fi.hIcon, crate::coherence::ICON_SIZE as i32);
        DestroyIcon(fi.hIcon);
        i
    }
}

/// The install folder of a package (its full name).
pub fn package_path(full: &[u16]) -> Option<PathBuf> {
    let mut b = vec![0u16; 1024];
    let mut n = b.len() as u32;
    // SAFETY: a NUL-terminated name, a buffer and its length.
    let r = unsafe { GetPackagePathByFullName(full.as_ptr(), &mut n, b.as_mut_ptr()) };
    if r != ERROR_SUCCESS {
        return None;
    }
    let len = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    Some(PathBuf::from(String::from_utf16_lossy(&b[..len])))
}

/// The install folder of a package family's (first) package.
fn family_path(family: &str) -> Option<PathBuf> {
    let f = wide(family);
    let (mut count, mut len) = (0u32, 0u32);
    // SAFETY: sizes first (null buffers), then the names into buffers that big.
    unsafe {
        let r = GetPackagesByPackageFamily(f.as_ptr(), &mut count, std::ptr::null_mut(), &mut len, std::ptr::null_mut());
        if r != ERROR_INSUFFICIENT_BUFFER || count == 0 || count > 64 || len == 0 || len > 1 << 16 {
            return None;
        }
        let mut names: Vec<PWSTR> = vec![std::ptr::null_mut(); count as usize];
        let mut buf = vec![0u16; len as usize];
        if GetPackagesByPackageFamily(f.as_ptr(), &mut count, names.as_mut_ptr(), &mut len, buf.as_mut_ptr()) != ERROR_SUCCESS || names[0].is_null() {
            return None;
        }
        // names[0] points into buf, NUL-terminated.
        let start = names[0].offset_from(buf.as_ptr());
        let start = usize::try_from(start).ok().filter(|&s| s < buf.len())?;
        let end = buf[start..].iter().position(|&c| c == 0)? + start;
        package_path(&buf[start..=end])
    }
}

/// A package's logo for one of its apps (by id, else by its program
/// relative to the package), about `ICON_SIZE`: see appx.rs.
pub fn package_logo(dir: &Path, id: Option<&str>, exe: Option<&str>) -> Option<Icon> {
    let m = dir.join("AppxManifest.xml");
    if std::fs::metadata(&m).ok()?.len() > 4 << 20 {
        return None;
    }
    let text = std::fs::read_to_string(&m).ok()?;
    let logo = crate::appx::logo(&text, id, exe)?;
    let (sub, name) = crate::appx::split(&logo);
    // Inside the package only.
    if sub.split('\\').any(|p| p == ".." || p.contains(':')) || name.contains(':') {
        return None;
    }
    let folder = dir.join(&sub);
    let names: Vec<String> = std::fs::read_dir(&folder).ok()?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).take(4096).collect();
    let size = crate::coherence::ICON_SIZE;
    let file = crate::appx::pick(&name, &names, 44, size)?;
    let p = folder.join(file);
    if std::fs::metadata(&p).ok()?.len() > 4 << 20 {
        return None;
    }
    let i = crate::apps::png(&std::fs::read(&p).ok()?)?;
    crate::coherence::fit(i.w, i.h, &i.rgba, size)
}

/// An app's icon: a Store app's logo from its package, else the shell's.
pub fn icon(t: &Target) -> Option<Icon> {
    com();
    match t {
        Target::Lnk(p) => shell_icon(wide_path(p).as_ptr(), false),
        Target::App(id) => {
            if let Some((family, app)) = id.split_once('!') {
                if let Some(i) = family_path(family).and_then(|d| package_logo(&d, Some(app), None)) {
                    return Some(i);
                }
            }
            let item = shell_item(&format!(r"shell:AppsFolder\{id}"))?;
            let mut pidl = std::ptr::null_mut();
            // SAFETY: a live item; the id list is ours to free.
            let i = unsafe {
                if SHGetIDListFromObject(item, &mut pidl) >= 0 && !pidl.is_null() {
                    let i = shell_icon(pidl as *const u16, true);
                    CoTaskMemFree(pidl as *const c_void);
                    i
                } else {
                    None
                }
            };
            release(item);
            i
        }
    }
}

pub fn with_icons<R>(f: impl FnOnce(&[Entry<Target>], &mut dyn FnMut(&Target) -> Option<Icon>) -> R) -> R {
    let list = scan();
    f(&list, &mut icon)
}

/// Open it as Start would (this process is the user's session helper).
pub fn launch(e: &Entry<Target>) -> Result<(), String> {
    com();
    let file = match &e.target {
        Target::Lnk(p) => wide_path(p),
        Target::App(id) => wide(&format!(r"shell:AppsFolder\{id}")),
    };
    let open = wide("open");
    // SAFETY: plain calls with NUL-terminated strings. The app it starts
    // may take the front (we are the last to have had the user's input
    // only sometimes).
    let r = unsafe {
        AllowSetForegroundWindow(ASFW_ANY);
        ShellExecuteW(std::ptr::null_mut(), open.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL)
    };
    // Above 32: it worked.
    if r as usize > 32 {
        log::info!("apps: started {:?} ({:?})", e.name, e.target);
        Ok(())
    } else {
        Err(format!("ShellExecute: {}", r as usize))
    }
}

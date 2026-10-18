//! What vroom.app gets from macOS besides windows: files opened from
//! Finder (double click, Open With, dropped on the Dock icon) and a click
//! on the Dock icon while no window shows. winit's app delegate has
//! neither, so we add the two methods to its class before the event loop
//! runs (macOS sends the files of the launch while it starts the loop).

#[cfg(target_os = "macos")]
use crate::msg::{self, Msg};

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod objc {
    use std::ffi::{c_char, c_void, CStr, OsStr};
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    pub type Id = *mut c_void;
    pub type Sel = *mut c_void;

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> Id;
        fn sel_registerName(name: *const c_char) -> Sel;
        fn class_addMethod(cls: Id, name: Sel, imp: *const c_void, types: *const c_char) -> bool;
        fn objc_msgSend();
    }

    pub fn sel(name: &CStr) -> Sel {
        // SAFETY: a valid C string; selectors live for the process.
        unsafe { sel_registerName(name.as_ptr()) }
    }

    /// Add `imp` as `name` to the class called `class`. False if there is
    /// no such class or it has that method already.
    pub fn add_method(class: &CStr, name: &CStr, imp: *const c_void, types: &CStr) -> bool {
        // SAFETY: valid C strings; `imp` has the signature `types` says
        // (checked by the callers, which pass their own functions).
        unsafe {
            let cls = objc_getClass(class.as_ptr());
            !cls.is_null() && class_addMethod(cls, sel(name), imp, types.as_ptr())
        }
    }

    /// The file paths in an NSArray of NSURLs (others are skipped).
    ///
    /// # Safety
    /// `urls` is an NSArray<NSURL *> (what AppKit hands the delegate).
    pub unsafe fn file_paths(urls: Id) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if urls.is_null() {
            return out;
        }
        let send = objc_msgSend as unsafe extern "C" fn();
        // SAFETY: objc_msgSend called with each method's own signature
        // (NSArray count/objectAtIndex:, NSURL isFileURL and
        // fileSystemRepresentation, a C string the URL owns).
        unsafe {
            let count: unsafe extern "C" fn(Id, Sel) -> usize = std::mem::transmute(send);
            let at: unsafe extern "C" fn(Id, Sel, usize) -> Id = std::mem::transmute(send);
            let is_file: unsafe extern "C" fn(Id, Sel) -> bool = std::mem::transmute(send);
            let fs_rep: unsafe extern "C" fn(Id, Sel) -> *const c_char = std::mem::transmute(send);
            for i in 0..count(urls, sel(c"count")) {
                let u = at(urls, sel(c"objectAtIndex:"), i);
                if u.is_null() || !is_file(u, sel(c"isFileURL")) {
                    continue;
                }
                let p = fs_rep(u, sel(c"fileSystemRepresentation"));
                if !p.is_null() {
                    out.push(PathBuf::from(OsStr::from_bytes(CStr::from_ptr(p).to_bytes())));
                }
            }
        }
        out
    }
}

/// `application:openURLs:`.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
extern "C" fn open_urls(_this: objc::Id, _cmd: objc::Sel, _app: objc::Id, urls: objc::Id) {
    // SAFETY: AppKit passes an NSArray of NSURLs here.
    for p in unsafe { objc::file_paths(urls) } {
        log::info!("open {}", p.display());
        msg::send(Msg::Open(p));
    }
}

/// `applicationShouldHandleReopen:hasVisibleWindows:`: the Dock icon was
/// clicked. With no window showing (the library was closed, the VMs run
/// on), show the library.
#[cfg(target_os = "macos")]
extern "C" fn reopen(_this: objc::Id, _cmd: objc::Sel, _app: objc::Id, visible: bool) -> bool {
    if !visible {
        msg::send(Msg::ShowLibrary(None));
    }
    true
}

/// Hook the app delegate. Call after the first window exists (winit made
/// its delegate then) and before the event loop runs.
#[cfg(target_os = "macos")]
pub fn install() {
    let class = c"WinitApplicationDelegate";
    // On arm64 BOOL is bool ("B").
    let ok = objc::add_method(class, c"application:openURLs:", open_urls as *const _, c"v@:@@")
        & objc::add_method(class, c"applicationShouldHandleReopen:hasVisibleWindows:", reopen as *const _, c"B@:@B");
    if !ok {
        log::warn!("couldn't hook the app delegate: files opened from Finder won't reach the app");
    }
}

#[cfg(not(target_os = "macos"))]
pub fn install() {}

/// Arguments to ignore: very old macOS passed `-psn_0_123` to apps
/// started from Finder.
pub fn is_finder_arg(a: &str) -> bool {
    a.starts_with("-psn_")
}

/// Started from Finder or the Dock (so stderr goes nowhere): send it to a
/// log file in the app's data dir instead, the last run only.
#[allow(unsafe_code)]
pub fn log_to_file_if_detached() {
    if !stderr_is_dev_null() {
        return;
    }
    let p = crate::paths::data_dir().join("logs").join("vroom-gui.log");
    let Some(dir) = p.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let Ok(f) = std::fs::File::create(&p) else { return };
    use std::os::fd::AsRawFd;
    // SAFETY: dup2 of a file we opened onto stderr; `f` closes its own fd.
    unsafe { libc::dup2(f.as_raw_fd(), 2) };
}

#[allow(unsafe_code)]
fn stderr_is_dev_null() -> bool {
    // SAFETY: fstat/stat into zeroed structs we own.
    unsafe {
        let mut a: libc::stat = std::mem::zeroed();
        let mut b: libc::stat = std::mem::zeroed();
        libc::fstat(2, &mut a) == 0
            && libc::stat(c"/dev/null".as_ptr(), &mut b) == 0
            && (a.st_mode & libc::S_IFMT) == libc::S_IFCHR
            && a.st_rdev == b.st_rdev
    }
}

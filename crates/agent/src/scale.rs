//! Desktop UI scale, for host windows on HiDPI screens: the host gives the
//! guest its window in physical pixels and asks for 2x so text stays the
//! same size on screen.
//!
//! The first time we change the scale we write down what the user had
//! (`scale-before-<backend>` in the agent's data dir); `SetScale(0)` (the
//! host turned HiDPI off) puts that back. Values are each desktop's own units: GNOME's
//! integer `scaling-factor` (X11), the monitor's scale in percent (GNOME on
//! Wayland), Windows' DPI percent.
//!
//! Requests are applied on a thread of their own, latest first: on Wayland
//! we wait for the compositor to take the new mode before scaling it.

use std::io;
use std::path::PathBuf;
use std::sync::{Condvar, Mutex};

pub trait Backend {
    /// Names the file the user's setting is kept in (units differ).
    fn name(&self) -> &'static str;
    /// Wait until the desktop runs at `w` x `h` (the resolution the host
    /// just asked for), if that matters to this backend.
    fn settle(&mut self, _size: Option<(u32, u32)>) {}
    /// The current setting, in this backend's units.
    fn get(&mut self) -> io::Result<u32>;
    fn set(&mut self, v: u32) -> io::Result<()>;
    /// The setting closest to `percent` (100 = 1x).
    fn for_percent(&self, percent: u32) -> u32;
}

/// Apply a host request: `percent` > 0 sets that scale, 0 restores what
/// the user had before we first changed it.
pub fn apply(b: &mut dyn Backend, saved: &mut dyn Saved, percent: u32) -> io::Result<()> {
    if percent == 0 {
        let Some(before) = saved.load() else { return Ok(()) };
        if b.get()? != before {
            b.set(before)?;
        }
        saved.clear();
        return Ok(());
    }
    let want = b.for_percent(percent);
    let cur = b.get()?;
    if saved.load().is_none() {
        saved.store(cur);
    }
    if cur != want {
        b.set(want)?;
    }
    Ok(())
}

/// Where the user's own setting is kept while we manage it.
pub trait Saved {
    fn load(&mut self) -> Option<u32>;
    fn store(&mut self, v: u32);
    fn clear(&mut self);
}

pub struct SavedFile(Option<PathBuf>);

impl SavedFile {
    pub fn new(backend: &str) -> Self {
        SavedFile(data_dir().map(|d| d.join(format!("scale-before-{backend}"))))
    }
}

/// Per-user: the helper runs as the desktop's user.
fn data_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Vroom"));
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("vroom"))
}

impl Saved for SavedFile {
    fn load(&mut self) -> Option<u32> {
        let s = std::fs::read_to_string(self.0.as_ref()?).ok()?;
        s.trim().parse().ok()
    }
    fn store(&mut self, v: u32) {
        let Some(p) = &self.0 else { return };
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Err(e) = std::fs::write(p, format!("{v}\n")) {
            log::warn!("scale: {}: {e}", p.display());
        }
    }
    fn clear(&mut self) {
        if let Some(p) = &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// The desktop's backend, if we know how to scale it.
pub fn system() -> Option<Box<dyn Backend>> {
    #[cfg(windows)]
    return Some(Box::new(win::Dpi));
    #[cfg(target_os = "linux")]
    return Some(if mutter::wayland() { Box::new(mutter::Mutter) } else { Box::new(gnome::Gnome) });
    #[allow(unreachable_code)]
    None
}

struct Pending {
    /// Scale to apply next.
    percent: Option<u32>,
    /// The last resolution the host asked for.
    size: Option<(u32, u32)>,
}

static PENDING: Mutex<Pending> = Mutex::new(Pending { percent: None, size: None });
static WAKE: Condvar = Condvar::new();
static WORKER: std::sync::Once = std::sync::Once::new();

/// The host asked for this resolution (scale requests follow it).
pub fn note_resolution(w: u32, h: u32) {
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).size = Some((w, h));
}

/// Host request, from the message loop. Applied in the background.
pub fn set(percent: u32) {
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).percent = Some(percent);
    WAKE.notify_one();
    WORKER.call_once(|| {
        if let Err(e) = std::thread::Builder::new().name("scale".into()).spawn(worker) {
            log::warn!("scale: {e}");
        }
    });
}

fn worker() {
    loop {
        let (percent, size) = {
            let mut p = PENDING.lock().unwrap_or_else(|e| e.into_inner());
            while p.percent.is_none() {
                p = WAKE.wait(p).unwrap_or_else(|e| e.into_inner());
            }
            (p.percent.take().unwrap_or(0), p.size)
        };
        let Some(mut b) = system() else { continue };
        b.settle(size);
        let mut saved = SavedFile::new(b.name());
        if let Err(e) = apply(b.as_mut(), &mut saved, percent) {
            log::info!("scale {percent}%: {e}");
        }
    }
}

/// GNOME: `org.gnome.desktop.interface scaling-factor` (0 = automatic,
/// else an integer factor). GNOME on X11 picks it up live. (Wayland: see
/// `mutter`.)
#[cfg(target_os = "linux")]
mod gnome {
    use std::io;
    use std::process::Command;

    const SCHEMA: &str = "org.gnome.desktop.interface";
    const KEY: &str = "scaling-factor";

    pub struct Gnome;

    fn run(args: &[&str]) -> io::Result<String> {
        let o = Command::new("gsettings").args(args).output()?;
        if !o.status.success() {
            return Err(io::Error::other(String::from_utf8_lossy(&o.stderr).trim().to_string()));
        }
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    }

    /// "uint32 2" -> 2
    pub fn parse(s: &str) -> Option<u32> {
        s.split_whitespace().last()?.parse().ok()
    }

    impl super::Backend for Gnome {
        fn name(&self) -> &'static str {
            "gnome"
        }
        fn get(&mut self) -> io::Result<u32> {
            parse(&run(&["get", SCHEMA, KEY])?).ok_or_else(|| io::Error::other("gsettings: unexpected output"))
        }
        fn set(&mut self, v: u32) -> io::Result<()> {
            run(&["set", SCHEMA, KEY, &v.to_string()])?;
            log::info!("scale: GNOME scaling-factor {v}");
            Ok(())
        }
        fn for_percent(&self, percent: u32) -> u32 {
            // GNOME's factor is a whole number; 150% rounds to 2.
            percent.div_ceil(100).clamp(1, 4)
        }
    }
}

/// GNOME on Wayland: Mutter ignores `scaling-factor` once it runs; set the
/// monitor's scale through its DisplayConfig D-Bus API, the way Settings >
/// Displays does (as a temporary config: no "keep these settings?" prompt,
/// and nothing written to monitors.xml). busctl talks D-Bus for us.
#[cfg(target_os = "linux")]
mod mutter {
    use std::io;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use serde_json::Value;

    const DEST: [&str; 3] = ["org.gnome.Mutter.DisplayConfig", "/org/gnome/Mutter/DisplayConfig", "org.gnome.Mutter.DisplayConfig"];
    /// How long the compositor may take to pick up a new mode.
    const SETTLE: Duration = Duration::from_secs(3);

    pub fn wayland() -> bool {
        std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland") || std::env::var_os("WAYLAND_DISPLAY").is_some()
    }

    pub struct Mutter;

    /// A logical monitor as Mutter reports it, with what we need to send
    /// it back.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Logical {
        pub x: i64,
        pub y: i64,
        pub scale: f64,
        pub transform: u64,
        pub primary: bool,
        pub monitors: Vec<Output>,
    }

    /// A monitor in its current mode.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Output {
        pub connector: String,
        pub mode: String,
        pub size: (u64, u64),
        /// The scales the mode allows.
        pub scales: Vec<f64>,
    }

    #[derive(Debug, Clone, PartialEq)]
    pub struct State {
        pub serial: u64,
        pub logical: Vec<Logical>,
    }

    fn busctl(args: &[&str]) -> io::Result<String> {
        let o = Command::new("busctl").args(["--user", "--json=short", "call"]).args(DEST).args(args).output()?;
        if !o.status.success() {
            return Err(io::Error::other(String::from_utf8_lossy(&o.stderr).trim().to_string()));
        }
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    }

    fn state() -> io::Result<State> {
        parse(&busctl(&["GetCurrentState"])?).ok_or_else(|| io::Error::other("unexpected DisplayConfig state"))
    }

    /// GetCurrentState: (serial, monitors, logical monitors, properties).
    pub fn parse(json: &str) -> Option<State> {
        let v: Value = serde_json::from_str(json).ok()?;
        let d = v.get("data")?.as_array()?;
        let serial = d.first()?.as_u64()?;
        // connector, current mode id, size, scales
        let mut current = Vec::new();
        for m in d.get(1)?.as_array()? {
            let conn = m.get(0)?.get(0)?.as_str()?;
            for mode in m.get(1)?.as_array()? {
                let is_cur = mode.get(6).and_then(|p| p.get("is-current")).and_then(|c| c.get("data")).and_then(Value::as_bool);
                if is_cur == Some(true) {
                    let size = (mode.get(1)?.as_u64()?, mode.get(2)?.as_u64()?);
                    let scales = mode.get(5)?.as_array()?.iter().filter_map(Value::as_f64).collect();
                    current.push(Output { connector: conn.to_string(), mode: mode.get(0)?.as_str()?.to_string(), size, scales });
                }
            }
        }
        let mut logical = Vec::new();
        for l in d.get(2)?.as_array()? {
            let names: Vec<&str> = l.get(5)?.as_array()?.iter().filter_map(|m| m.get(0)?.as_str()).collect();
            logical.push(Logical {
                x: l.get(0)?.as_i64()?,
                y: l.get(1)?.as_i64()?,
                scale: l.get(2)?.as_f64()?,
                transform: l.get(3)?.as_u64()?,
                primary: l.get(4)?.as_bool()?,
                monitors: current.iter().filter(|c| names.contains(&c.connector.as_str())).cloned().collect(),
            });
        }
        Some(State { serial, logical })
    }

    /// The ApplyMonitorsConfig arguments (busctl syntax) for `s` with every
    /// monitor at the allowed scale nearest `want`. None: nothing changes.
    pub fn config(s: &State, want: f64) -> Option<Vec<String>> {
        let mut a = vec!["uua(iiduba(ssa{sv}))a{sv}".to_string(), s.serial.to_string(), "1".into(), s.logical.len().to_string()];
        let mut change = false;
        for l in &s.logical {
            // Every monitor of a logical monitor must allow the scale.
            let scale = l
                .monitors
                .first()?
                .scales
                .iter()
                .copied()
                .filter(|c| l.monitors.iter().all(|m| m.scales.iter().any(|x| (x - c).abs() < 1e-6)))
                .min_by(|a, b| (a - want).abs().total_cmp(&(b - want).abs()))?;
            change |= (scale - l.scale).abs() > 1e-6;
            // One screen (a VM's): it stays at the origin.
            let (x, y) = if s.logical.len() == 1 { (0, 0) } else { (l.x, l.y) };
            a.extend([x.to_string(), y.to_string(), scale.to_string(), l.transform.to_string(), l.primary.to_string(), l.monitors.len().to_string()]);
            for m in &l.monitors {
                a.extend([m.connector.clone(), m.mode.clone(), "0".into()]);
            }
        }
        a.push("0".into());
        change.then_some(a)
    }

    impl super::Backend for Mutter {
        fn name(&self) -> &'static str {
            "mutter"
        }
        fn settle(&mut self, size: Option<(u32, u32)>) {
            let Some((w, h)) = size else { return };
            let until = Instant::now() + SETTLE;
            while Instant::now() < until {
                let at = state().ok().and_then(|s| s.logical.first()?.monitors.first().map(|m| m.size));
                if at == Some((w as u64, h as u64)) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(150));
            }
        }
        fn get(&mut self) -> io::Result<u32> {
            let s = state()?;
            let l = s.logical.first().ok_or_else(|| io::Error::other("no monitor"))?;
            Ok((l.scale * 100.0).round() as u32)
        }
        fn set(&mut self, v: u32) -> io::Result<()> {
            let s = state()?;
            let Some(args) = config(&s, v as f64 / 100.0) else { return Ok(()) };
            let mut call = vec!["ApplyMonitorsConfig"];
            call.extend(args.iter().map(String::as_str));
            busctl(&call)?;
            log::info!("scale: monitors at {}x", args[6]);
            Ok(())
        }
        fn for_percent(&self, percent: u32) -> u32 {
            percent
        }
    }
}

/// Windows' scale steps (Settings > Display > Scale), apart from the API
/// so they are tested everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
mod steps {
    pub const STEPS: [u32; 12] = [100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500];

    /// Index of the step nearest `percent`.
    pub fn nearest(percent: u32) -> usize {
        (0..STEPS.len()).min_by_key(|&i| STEPS[i].abs_diff(percent)).unwrap_or(0)
    }

    /// Windows reports steps relative to the monitor's recommended one;
    /// the minimum is always 100%, so the recommended step is -min.
    /// (recommended, current, max) as indexes into STEPS.
    pub fn steps(min: i32, cur: i32, max: i32) -> Option<(usize, usize, usize)> {
        let rec = usize::try_from(min.checked_neg()?).ok()?;
        let at = |rel: i32| rec.checked_add_signed(rel as isize).filter(|i| *i < STEPS.len());
        Some((rec, at(cur.clamp(min, max))?, at(max)?))
    }
}

/// Windows: per-monitor DPI, the way Settings > Display > Scale changes
/// it (DisplayConfig{Get,Set}DeviceInfo with Settings' private types -3
/// and -4). Takes effect at once for the shell and DPI-aware apps; the
/// rest pick it up when restarted. Values are relative to the monitor's
/// recommended scale, in steps of the table below.
#[cfg(windows)]
mod win {
    use std::io;

    use windows_sys::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
        DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, QDC_ONLY_ACTIVE_PATHS,
    };
    use windows_sys::Win32::Foundation::LUID;

    use super::steps::{nearest, steps, STEPS};

    const GET_DPI_SCALE: i32 = -3;
    const SET_DPI_SCALE: i32 = -4;

    #[repr(C)]
    struct GetScale {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
        min_rel: i32,
        cur_rel: i32,
        max_rel: i32,
    }

    #[repr(C)]
    struct SetScale {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
        rel: i32,
    }

    pub struct Dpi;

    fn header(ty: i32, size: usize, adapter: LUID, id: u32) -> DISPLAYCONFIG_DEVICE_INFO_HEADER {
        DISPLAYCONFIG_DEVICE_INFO_HEADER { r#type: ty, size: size as u32, adapterId: adapter, id }
    }

    /// Active display sources: (adapter, source id).
    fn sources() -> io::Result<Vec<(LUID, u32)>> {
        let (mut np, mut nm) = (0u32, 0u32);
        // SAFETY: valid out pointers.
        if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: plain data; all-zero is valid.
        let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> = vec![unsafe { std::mem::zeroed() }; np as usize];
        // SAFETY: as above.
        let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> = vec![unsafe { std::mem::zeroed() }; nm as usize];
        // SAFETY: arrays sized as the counts say.
        let r = unsafe { QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), std::ptr::null_mut()) };
        if r != 0 {
            return Err(io::Error::other(format!("QueryDisplayConfig: {r}")));
        }
        paths.truncate(np as usize);
        Ok(paths.iter().map(|p| (p.sourceInfo.adapterId, p.sourceInfo.id)).collect())
    }

    /// (index of the recommended step, current step, max step) for a source.
    fn info(adapter: LUID, id: u32) -> io::Result<(usize, usize, usize)> {
        let mut g = GetScale { header: header(GET_DPI_SCALE, std::mem::size_of::<GetScale>(), adapter, id), min_rel: 0, cur_rel: 0, max_rel: 0 };
        // SAFETY: header at the start of a struct as large as it says.
        let r = unsafe { DisplayConfigGetDeviceInfo(&mut g.header) };
        if r != 0 {
            return Err(io::Error::other(format!("DisplayConfigGetDeviceInfo: {r}")));
        }
        steps(g.min_rel, g.cur_rel, g.max_rel).ok_or_else(|| io::Error::other("unexpected DPI scale range"))
    }

    impl super::Backend for Dpi {
        fn name(&self) -> &'static str {
            "windows"
        }
        fn get(&mut self) -> io::Result<u32> {
            let &(a, id) = sources()?.first().ok_or_else(|| io::Error::other("no active display"))?;
            let (_, cur, _) = info(a, id)?;
            Ok(STEPS[cur])
        }
        fn set(&mut self, v: u32) -> io::Result<()> {
            for (a, id) in sources()? {
                let (rec, _, max) = info(a, id)?;
                // The step nearest v, as far as this mode allows.
                let want = nearest(v).min(max);
                let rel = want as i32 - rec as i32;
                let s = SetScale { header: header(SET_DPI_SCALE, std::mem::size_of::<SetScale>(), a, id), rel };
                // SAFETY: as in info().
                let r = unsafe { DisplayConfigSetDeviceInfo(&s.header) };
                if r != 0 {
                    return Err(io::Error::other(format!("DisplayConfigSetDeviceInfo: {r}")));
                }
                log::info!("scale: display {id} at {}%", STEPS[want]);
            }
            Ok(())
        }
        fn for_percent(&self, percent: u32) -> u32 {
            STEPS[nearest(percent)]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        v: u32,
        sets: Vec<u32>,
    }
    impl Backend for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn get(&mut self) -> io::Result<u32> {
            Ok(self.v)
        }
        fn set(&mut self, v: u32) -> io::Result<()> {
            self.v = v;
            self.sets.push(v);
            Ok(())
        }
        fn for_percent(&self, p: u32) -> u32 {
            p / 100
        }
    }
    #[derive(Default)]
    struct Mem(Option<u32>);
    impl Saved for Mem {
        fn load(&mut self) -> Option<u32> {
            self.0
        }
        fn store(&mut self, v: u32) {
            self.0 = Some(v);
        }
        fn clear(&mut self) {
            self.0 = None;
        }
    }

    #[test]
    fn remembers_and_restores_the_users_scale() {
        let (mut b, mut s) = (Fake { v: 0, sets: vec![] }, Mem::default());
        // Nothing to undo yet.
        apply(&mut b, &mut s, 0).unwrap();
        assert!(b.sets.is_empty());
        apply(&mut b, &mut s, 200).unwrap();
        apply(&mut b, &mut s, 200).unwrap();
        apply(&mut b, &mut s, 100).unwrap();
        assert_eq!(b.sets, vec![2, 1]);
        assert_eq!(s.0, Some(0), "the first value is what we keep");
        apply(&mut b, &mut s, 0).unwrap();
        assert_eq!(b.v, 0);
        assert_eq!(s.0, None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gnome_values() {
        use super::gnome::{parse, Gnome};
        assert_eq!(parse("uint32 2\n"), Some(2));
        assert_eq!(parse("garbage"), None);
        let g = Gnome;
        assert_eq!([100, 150, 200, 300, 500].map(|p| g.for_percent(p)), [1, 2, 2, 3, 4]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mutter_state() {
        use super::mutter::{config, parse, Output};
        // GNOME 42 on virtio-gpu (trimmed).
        let j = r#"{"type":"ua((ssss)a(siiddada{sv})a{sv})a(iiduba(ssss)a{sv})a{sv}","data":[3,
            [[["Virtual-1","unknown","unknown","unknown"],
              [["4096x2160@60",4096,2160,60.0,1.0,[1.0,2.0,3.0,4.0],{}],
               ["2400x1500@60",2400,1500,59.9,1.0,[1.0,2.0,3.0],{"is-current":{"type":"b","data":true},"is-preferred":{"type":"b","data":true}}]],
              {"is-builtin":{"type":"b","data":false}}]],
            [[0,0,1.0,0,true,[["Virtual-1","unknown","unknown","unknown"]],{}]],
            {"layout-mode":{"type":"u","data":2}}]}"#;
        let s = parse(j).unwrap();
        assert_eq!(s.serial, 3);
        let out = Output { connector: "Virtual-1".into(), mode: "2400x1500@60".into(), size: (2400, 1500), scales: vec![1.0, 2.0, 3.0] };
        assert_eq!(s.logical[0].monitors, vec![out]);
        let a = config(&s, 2.0).unwrap();
        assert_eq!(a[1..].join(" "), "3 1 1 0 0 2 0 true 1 Virtual-1 2400x1500@60 0 0");
        assert_eq!(config(&s, 1.2), None, "already at the nearest scale");
        assert_eq!(config(&s, 9.0).unwrap()[6], "3");
        assert!(parse("{}").is_none() && parse("not json").is_none());
    }

    #[test]
    fn windows_steps() {
        use super::steps::{nearest, steps, STEPS};
        assert_eq!(steps(0, 4, 6), Some((0, 4, 6)));
        assert_eq!(steps(-2, 0, 3), Some((2, 2, 5)));
        assert_eq!(steps(1, 0, 3), None);
        assert_eq!(steps(0, 0, 40), None);
        assert_eq!([100, 147, 200, 280, 9999].map(|p| STEPS[nearest(p)]), [100, 150, 200, 300, 500]);
    }
}

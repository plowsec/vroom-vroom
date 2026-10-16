//! Several monitors (`SetLayout`): each of the host's GPU outputs gets its
//! size and place on the desktop, or is turned off. The host maps its
//! pointer for monitors side by side in its order, so the guest has to
//! put them there.
//!
//! - Windows: one GPU per monitor (its drivers drive one output each); the
//!   host numbers them in the PCI subsystem id (0x1100 + n), which shows
//!   in the adapter's hardware id. ChangeDisplaySettingsEx per display,
//!   then once to apply them all; displays that aren't ours (the
//!   firmware's basic display) are taken off the desktop.
//! - Linux on X11: xrandr. The outputs are `Virtual-1`, `Virtual-2`...
//!   in the GPU's order.
//! - GNOME on Wayland: Mutter's DisplayConfig, as `scale.rs` does.

use std::io;

use vroom_proto::Monitor;

/// Which of the host's GPUs a Windows display adapter is, from its
/// hardware id (`PCI\VEN_1AF4&DEV_1050&SUBSYS_11011AF4&REV_01` is the
/// second).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn gpu_head(hwid: &str) -> Option<usize> {
    let up = hwid.to_ascii_uppercase();
    if !up.contains("VEN_1AF4&DEV_1050") {
        return None;
    }
    let i = up.find("SUBSYS_")? + 7;
    let sub = u32::from_str_radix(up.get(i..i + 8)?, 16).ok()?;
    let (dev, ven) = (sub >> 16, sub & 0xffff);
    (ven == 0x1af4 && (0x1100..0x1110).contains(&dev)).then(|| (dev - 0x1100) as usize)
}

/// An xrandr output: its name, whether something is connected, and the
/// modes it lists.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct Output {
    pub name: String,
    pub connected: bool,
    pub modes: Vec<(u32, u32)>,
}

/// `xrandr --query` output.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_xrandr(s: &str) -> Vec<Output> {
    let mut out: Vec<Output> = Vec::new();
    for l in s.lines() {
        if l.starts_with("Screen ") {
            continue;
        }
        if !l.starts_with(' ') && !l.starts_with('\t') {
            let mut w = l.split_whitespace();
            let (Some(name), Some(state)) = (w.next(), w.next()) else { continue };
            out.push(Output { name: name.to_string(), connected: state == "connected", modes: Vec::new() });
            continue;
        }
        let Some(o) = out.last_mut() else { continue };
        let Some(m) = l.split_whitespace().next() else { continue };
        let m = m.trim_end_matches('i');
        if let Some((w, h)) = m.split_once('x') {
            if let (Ok(w), Ok(h)) = (w.parse(), h.parse()) {
                o.modes.push((w, h));
            }
        }
    }
    out
}

/// The virtio GPU's outputs in its order: `Virtual-1`, `Virtual-2`...
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn virtual_index(name: &str) -> Option<usize> {
    let n = name.strip_prefix("Virtual")?.trim_start_matches('-');
    n.parse::<usize>().ok()?.checked_sub(1)
}

/// The xrandr arguments for `monitors`. None if no output matches.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn xrandr_args(outputs: &[Output], monitors: &[Option<Monitor>]) -> Option<Vec<String>> {
    let mut a = Vec::new();
    let mut any = false;
    for o in outputs {
        let Some(head) = virtual_index(&o.name) else { continue };
        a.extend(["--output".to_string(), o.name.clone()]);
        match monitors.get(head).copied().flatten() {
            Some(m) if o.connected => {
                any = true;
                if o.modes.contains(&(m.width, m.height)) {
                    a.extend(["--mode".to_string(), format!("{}x{}", m.width, m.height)]);
                } else {
                    a.push("--auto".into());
                }
                a.extend(["--pos".to_string(), format!("{}x{}", m.x, m.y)]);
                if head == 0 {
                    a.push("--primary".into());
                }
            }
            _ => a.push("--off".into()),
        }
    }
    any.then_some(a)
}

/// Apply a layout. Errors are for the log.
#[cfg(windows)]
pub fn apply(monitors: &[Option<Monitor>]) -> io::Result<()> {
    use windows_sys::Win32::Graphics::Gdi::{
        ChangeDisplaySettingsExW, EnumDisplayDevicesW, CDS_NORESET, CDS_SET_PRIMARY, CDS_UPDATEREGISTRY, DEVMODEW, DISPLAY_DEVICEW,
        DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISP_CHANGE_SUCCESSFUL, DM_PELSHEIGHT, DM_PELSWIDTH, DM_POSITION,
    };
    let text = |w: &[u16]| String::from_utf16_lossy(&w[..w.iter().position(|&c| c == 0).unwrap_or(w.len())]);
    // (device name, our monitor if it is one, on the desktop)
    let mut displays = Vec::new();
    for i in 0.. {
        // SAFETY: plain data; all zeroes is valid once cb is set.
        let mut dd: DISPLAY_DEVICEW = unsafe { std::mem::zeroed() };
        dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
        // SAFETY: valid out pointer, null device = adapters.
        if unsafe { EnumDisplayDevicesW(std::ptr::null(), i, &mut dd, 0) } == 0 {
            break;
        }
        displays.push((dd.DeviceName, gpu_head(&text(&dd.DeviceID)), dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP != 0));
    }
    let wanted = |h: Option<usize>| h.and_then(|h| monitors.get(h).copied().flatten());
    if !displays.iter().any(|d| wanted(d.1).is_some()) {
        return Err(io::Error::other(format!("none of the {} displays is one of the host's monitors", displays.len())));
    }
    for (name, head, attached) in &displays {
        // SAFETY: zeroed DEVMODEW is valid once dmSize is set.
        let mut dm: DEVMODEW = unsafe { std::mem::zeroed() };
        dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        dm.dmFields = DM_POSITION | DM_PELSWIDTH | DM_PELSHEIGHT;
        let mut flags = CDS_UPDATEREGISTRY | CDS_NORESET;
        match wanted(*head) {
            Some(m) => {
                dm.Anonymous1.Anonymous2.dmPosition.x = m.x;
                dm.Anonymous1.Anonymous2.dmPosition.y = m.y;
                dm.dmPelsWidth = m.width;
                dm.dmPelsHeight = m.height;
                if (m.x, m.y) == (0, 0) {
                    flags |= CDS_SET_PRIMARY;
                }
            }
            // Off: 0x0 takes it off the desktop.
            None if *attached => {}
            None => continue,
        }
        // SAFETY: a NUL-terminated name from Windows, a valid DEVMODEW.
        let r = unsafe { ChangeDisplaySettingsExW(name.as_ptr(), &dm, std::ptr::null_mut(), flags, std::ptr::null()) };
        log::info!("display {} (monitor {:?}): {}x{} at {},{}: {r}", text(name), head.map(|h| h + 1), dm.dmPelsWidth, dm.dmPelsHeight, m_x(&dm), m_y(&dm));
        if r != DISP_CHANGE_SUCCESSFUL {
            log::warn!("display {}: ChangeDisplaySettingsEx {r}", text(name));
        }
    }
    // Now all at once.
    // SAFETY: null everything = apply what the registry says.
    let r = unsafe { ChangeDisplaySettingsExW(std::ptr::null(), std::ptr::null(), std::ptr::null_mut(), 0, std::ptr::null()) };
    if r != DISP_CHANGE_SUCCESSFUL {
        return Err(io::Error::other(format!("ChangeDisplaySettingsEx: {r}")));
    }
    Ok(())
}

#[cfg(windows)]
fn m_x(dm: &windows_sys::Win32::Graphics::Gdi::DEVMODEW) -> i32 {
    // SAFETY: the display variant of the union, which we set.
    unsafe { dm.Anonymous1.Anonymous2.dmPosition.x }
}

#[cfg(windows)]
fn m_y(dm: &windows_sys::Win32::Graphics::Gdi::DEVMODEW) -> i32 {
    // SAFETY: as above.
    unsafe { dm.Anonymous1.Anonymous2.dmPosition.y }
}

/// Windows: how many of the host's GPUs show as displays.
#[cfg(windows)]
pub fn our_displays() -> usize {
    use windows_sys::Win32::Graphics::Gdi::{EnumDisplayDevicesW, DISPLAY_DEVICEW};
    let mut n = 0;
    for i in 0.. {
        // SAFETY: as in apply().
        let mut dd: DISPLAY_DEVICEW = unsafe { std::mem::zeroed() };
        dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
        // SAFETY: as in apply().
        if unsafe { EnumDisplayDevicesW(std::ptr::null(), i, &mut dd, 0) } == 0 {
            break;
        }
        let id = String::from_utf16_lossy(&dd.DeviceID[..dd.DeviceID.iter().position(|&c| c == 0).unwrap_or(128)]);
        n += gpu_head(&id).is_some() as usize;
    }
    n
}

#[cfg(target_os = "linux")]
pub fn apply(monitors: &[Option<Monitor>]) -> io::Result<()> {
    if crate::scale::wayland() {
        return mutter::apply(monitors);
    }
    let run = |args: &[String]| -> io::Result<String> {
        let o = std::process::Command::new("xrandr").args(args).output()?;
        if !o.status.success() {
            return Err(io::Error::other(String::from_utf8_lossy(&o.stderr).trim().to_string()));
        }
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    };
    // The GPU said its monitors changed just before: --query looks again.
    let outputs = parse_xrandr(&run(&["--query".into()])?);
    let args = xrandr_args(&outputs, monitors).ok_or_else(|| io::Error::other("no Virtual-N outputs"))?;
    log::info!("layout: xrandr {}", args.join(" "));
    run(&args).map(|_| ())
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn apply(_monitors: &[Option<Monitor>]) -> io::Result<()> {
    Ok(())
}

/// GNOME on Wayland: a temporary monitors config (as Settings applies
/// before "keep these settings?"), one logical monitor per output.
#[cfg(target_os = "linux")]
pub mod mutter {
    use std::io;

    use serde_json::Value;
    use vroom_proto::Monitor;

    /// A monitor Mutter knows: connector, its modes (id, w, h), and the
    /// scale of the logical monitor it is in now.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Mon {
        pub connector: String,
        pub modes: Vec<(String, u32, u32)>,
    }

    /// GetCurrentState (busctl --json=short): serial, monitors, and the
    /// first logical monitor's scale.
    pub fn parse(json: &str) -> Option<(u64, Vec<Mon>, f64)> {
        let v: Value = serde_json::from_str(json).ok()?;
        let d = v.get("data")?.as_array()?;
        let serial = d.first()?.as_u64()?;
        let mut mons = Vec::new();
        for m in d.get(1)?.as_array()? {
            let connector = m.get(0)?.get(0)?.as_str()?.to_string();
            let mut modes = Vec::new();
            for mode in m.get(1)?.as_array()? {
                modes.push((mode.get(0)?.as_str()?.to_string(), mode.get(1)?.as_u64()? as u32, mode.get(2)?.as_u64()? as u32));
            }
            mons.push(Mon { connector, modes });
        }
        let scale = d.get(2)?.as_array()?.first().and_then(|l| l.get(2)?.as_f64()).unwrap_or(1.0);
        Some((serial, mons, scale))
    }

    /// ApplyMonitorsConfig arguments (busctl syntax). Positions are in
    /// logical pixels: divided by the scale.
    pub fn config(serial: u64, mons: &[Mon], scale: f64, monitors: &[Option<Monitor>]) -> Option<Vec<String>> {
        let mut logical = Vec::new();
        for m in mons {
            let Some(head) = super::virtual_index(&m.connector) else { continue };
            let Some(want) = monitors.get(head).copied().flatten() else { continue };
            let mode = m.modes.iter().find(|x| (x.1, x.2) == (want.width, want.height)).or(m.modes.first())?;
            let k = if scale > 0.0 { scale } else { 1.0 };
            logical.push(vec![
                ((want.x as f64) / k).round().to_string(),
                ((want.y as f64) / k).round().to_string(),
                scale.to_string(),
                "0".into(),
                (head == 0).to_string(),
                "1".into(),
                m.connector.clone(),
                mode.0.clone(),
                "0".into(),
            ]);
        }
        if logical.is_empty() {
            return None;
        }
        let mut a = vec!["uua(iiduba(ssa{sv}))a{sv}".to_string(), serial.to_string(), "1".into(), logical.len().to_string()];
        for l in logical {
            a.extend(l);
        }
        a.push("0".into());
        Some(a)
    }

    #[cfg(target_os = "linux")]
    pub fn apply(monitors: &[Option<Monitor>]) -> io::Result<()> {
        const DEST: [&str; 3] = ["org.gnome.Mutter.DisplayConfig", "/org/gnome/Mutter/DisplayConfig", "org.gnome.Mutter.DisplayConfig"];
        let busctl = |args: &[&str]| -> io::Result<String> {
            let o = std::process::Command::new("busctl").args(["--user", "--json=short", "call"]).args(DEST).args(args).output()?;
            if !o.status.success() {
                return Err(io::Error::other(String::from_utf8_lossy(&o.stderr).trim().to_string()));
            }
            Ok(String::from_utf8_lossy(&o.stdout).into_owned())
        };
        let (serial, mons, scale) = parse(&busctl(&["GetCurrentState"])?).ok_or_else(|| io::Error::other("unexpected DisplayConfig state"))?;
        let args = config(serial, &mons, scale, monitors).ok_or_else(|| io::Error::other("no Virtual-N monitors"))?;
        let mut call = vec!["ApplyMonitorsConfig"];
        call.extend(args.iter().map(String::as_str));
        busctl(&call).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(x: i32, w: u32, h: u32) -> Option<Monitor> {
        Some(Monitor { x, y: 0, width: w, height: h })
    }

    #[test]
    fn windows_adapters_by_subsystem() {
        assert_eq!(gpu_head(r"PCI\VEN_1AF4&DEV_1050&SUBSYS_11001AF4&REV_01"), Some(0));
        assert_eq!(gpu_head(r"pci\ven_1af4&dev_1050&subsys_11021af4&rev_01"), Some(2));
        assert_eq!(gpu_head(r"PCI\VEN_1AF4&DEV_1041&SUBSYS_11001AF4&REV_01"), None, "not a GPU");
        assert_eq!(gpu_head(r"PCI\VEN_1AF4&DEV_1050&SUBSYS_00001AF4"), None);
        assert_eq!(gpu_head(r"ROOT\BasicDisplay"), None);
        assert_eq!(gpu_head(r"PCI\VEN_1AF4&DEV_1050&SUBSYS_11"), None, "cut short");
    }

    #[test]
    fn xrandr_layouts() {
        let q = "Screen 0: minimum 320 x 200, current 2304 x 800, maximum 8192 x 8192
Virtual-1 connected primary 1280x800+0+0 (normal left inverted right x axis y axis) 0mm x 0mm
   1280x800      59.81*+
   1024x768      60.00
Virtual-2 connected 1024x768+1280+0 (normal left inverted right x axis y axis) 0mm x 0mm
   1024x768      60.00*+
   800x600i      60.00
Virtual-3 disconnected (normal left inverted right x axis y axis)
";
        let o = parse_xrandr(q);
        assert_eq!(o.len(), 3);
        assert_eq!(o[1].modes, vec![(1024, 768), (800, 600)]);
        assert!(!o[2].connected);
        let a = xrandr_args(&o, &[mon(0, 1280, 800), mon(1280, 1600, 900)]).unwrap();
        assert_eq!(
            a.join(" "),
            "--output Virtual-1 --mode 1280x800 --pos 0x0 --primary --output Virtual-2 --auto --pos 1280x0 --output Virtual-3 --off"
        );
        let a = xrandr_args(&o, &[mon(0, 1024, 768), None]).unwrap();
        assert!(a.join(" ").contains("--output Virtual-2 --off"));
        assert_eq!(xrandr_args(&parse_xrandr("HDMI-1 connected\n"), &[mon(0, 1, 1)]), None);
        assert_eq!(virtual_index("Virtual1"), Some(0));
        assert_eq!(virtual_index("Virtual-0"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mutter_layouts() {
        let j = r#"{"type":"x","data":[7,
            [[["Virtual-1","u","u","u"],[["1280x800@60",1280,800,60.0,1.0,[1.0,2.0],{}]],{}],
             [["Virtual-2","u","u","u"],[["1024x768@60",1024,768,60.0,1.0,[1.0],{}],["800x600@60",800,600,60.0,1.0,[1.0],{}]],{}]],
            [[0,0,2.0,0,true,[["Virtual-1","u","u","u"]],{}]],{}]}"#;
        let (serial, mons, scale) = mutter::parse(j).unwrap();
        assert_eq!((serial, mons.len(), scale), (7, 2, 2.0));
        let a = mutter::config(serial, &mons, scale, &[mon(0, 1280, 800), mon(1280, 800, 600)]).unwrap();
        assert_eq!(a[1..].join(" "), "7 1 2 0 0 2 0 true 1 Virtual-1 1280x800@60 0 640 0 2 0 false 1 Virtual-2 800x600@60 0 0");
        assert_eq!(mutter::config(serial, &mons, scale, &[None, None]), None);
    }
}

//! The host's USB devices, from the IOKit registry (macOS) or sysfs
//! (Linux). Read only: nothing here opens a device, so listing never
//! disturbs one in use.

use crate::proto::Speed;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostDevice {
    pub vendor: u16,
    pub product: u16,
    pub manufacturer: Option<String>,
    pub name: Option<String>,
    pub serial: Option<String>,
    pub class: u8,
    pub speed: Option<Speed>,
    /// Bus number and the port path to the device, as libusb counts them.
    pub bus: u8,
    pub ports: Vec<u8>,
    /// Interface classes, and the host drivers that hold interfaces
    /// (a VM can't use those without root).
    pub interface_classes: Vec<u8>,
    pub claimed_by: Vec<String>,
    /// macOS: taken from macOS's drivers by a root helper (libusb's
    /// "capture") and not given back. Until it is re-enumerated (a root
    /// helper's reset, or unplugging it) only root can open it.
    pub captured: bool,
}

impl HostDevice {
    /// "vendor:product", the config's `id`.
    pub fn id(&self) -> String {
        format!("{:04x}:{:04x}", self.vendor, self.product)
    }

    /// "bus-port.port...", like Linux names devices.
    pub fn location(&self) -> String {
        let p: Vec<String> = self.ports.iter().map(|p| p.to_string()).collect();
        format!("{}-{}", self.bus, p.join("."))
    }

    /// A label for people: product name, else the id.
    pub fn label(&self) -> String {
        match (&self.manufacturer, &self.name) {
            (Some(m), Some(n)) if !n.starts_with(m.as_str()) => format!("{m} {n}"),
            (_, Some(n)) => n.clone(),
            _ => self.id(),
        }
    }

    /// Only a root helper (vroom-netd's) can take it: a macOS driver
    /// holds it, or an earlier root helper captured it. Linux: never (the
    /// helper detaches kernel drivers itself).
    pub fn needs_root(&self) -> bool {
        cfg!(target_os = "macos") && (!self.claimed_by.is_empty() || self.captured)
    }

    /// A hub, keyboard, mouse or disk: the things the host itself needs.
    pub fn is_host_essential(&self) -> bool {
        self.class == 9 || self.interface_classes.iter().any(|&c| c == 3 || c == 8 || c == 9)
    }

    /// A disk or card reader (mass storage).
    pub fn is_storage(&self) -> bool {
        self.class != 9 && (self.class == 8 || self.interface_classes.contains(&8))
    }
}

/// A disk of a USB device, as the IOKit registry has it below that
/// device (macOS).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HostDisk {
    /// "disk5": the whole disk (`/dev/disk5`, raw: `/dev/rdisk5`).
    pub bsd: String,
    pub bytes: u64,
    pub block: u32,
    /// Its device number (block and raw device share it).
    pub major: u32,
    pub minor: u32,
    pub removable: bool,
    pub ejectable: bool,
    pub writable: bool,
    /// The device says it is inside the computer (never a USB stick).
    pub internal: bool,
    /// Every BSD name on it: the disk, its partitions, and the disks
    /// made from them (an APFS container's).
    pub parts: Vec<String>,
}

impl HostDisk {
    /// Fit for a VM to take whole: one a person plugs in and out.
    pub fn takeable(&self) -> Result<(), &'static str> {
        if self.internal {
            return Err("it is an internal disk");
        }
        if !self.removable && !self.ejectable {
            return Err("macOS doesn't call it removable");
        }
        Ok(())
    }
}

/// A BSD whole-disk name: "disk" and a number.
pub fn bsd_disk_ok(s: &str) -> bool {
    s.strip_prefix("disk").is_some_and(|n| !n.is_empty() && n.len() <= 4 && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The disks of the USB device `vendor:product` (and `serial`): what its
/// registry subtree holds. Empty on Linux and for devices that aren't
/// disks.
pub fn disks(vendor: u16, product: u16, serial: Option<&str>) -> Vec<HostDisk> {
    #[cfg(target_os = "macos")]
    {
        mac::disks(vendor, product, serial)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (vendor, product, serial);
        Vec::new()
    }
}

/// The whole disk `bsd` ("disk6"), whatever it is attached to: for
/// tests with a disk image (`hdiutil attach`), which no USB device has.
pub fn disk_named(bsd: &str) -> Option<HostDisk> {
    #[cfg(target_os = "macos")]
    {
        bsd_disk_ok(bsd).then(|| mac::disk_named(bsd)).flatten()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = bsd;
        None
    }
}

/// Mounted file systems: (device, where), e.g. ("disk5s1", "/Volumes/test").
pub fn mounts() -> Vec<(String, std::path::PathBuf)> {
    #[cfg(target_os = "macos")]
    {
        mac::mounts()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Vec::new()
    }
}

pub fn list() -> Vec<HostDevice> {
    #[cfg(target_os = "macos")]
    {
        mac::list()
    }
    #[cfg(target_os = "linux")]
    {
        linux::list()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Vec::new()
    }
}

/// Printable, bounded: names come from the device.
fn clean(s: &str) -> Option<String> {
    let t: String = s.chars().filter(|c| !c.is_control()).take(126).collect();
    let t = t.trim().to_string();
    (!t.is_empty()).then_some(t)
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod mac {
    use std::ffi::{c_char, c_void, CStr, CString};

    use super::{clean, HostDevice, HostDisk};
    use crate::proto::Speed;

    type CFTypeRef = *const c_void;
    const UTF8: u32 = 0x0800_0100;
    const SINT64: isize = 4;

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOServiceMatching(name: *const c_char) -> *mut c_void;
        fn IOServiceGetMatchingServices(main: u32, matching: *mut c_void, existing: *mut u32) -> i32;
        fn IOIteratorNext(it: u32) -> u32;
        fn IOObjectRelease(o: u32) -> i32;
        fn IORegistryEntryCreateCFProperty(entry: u32, key: CFTypeRef, alloc: CFTypeRef, options: u32) -> CFTypeRef;
        fn IORegistryEntryGetChildIterator(entry: u32, plane: *const c_char, it: *mut u32) -> i32;
        fn IOObjectConformsTo(o: u32, class: *const c_char) -> u32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(alloc: CFTypeRef, s: *const c_char, enc: u32) -> CFTypeRef;
        fn CFRelease(cf: CFTypeRef);
        fn CFGetTypeID(cf: CFTypeRef) -> usize;
        fn CFNumberGetTypeID() -> usize;
        fn CFStringGetTypeID() -> usize;
        fn CFNumberGetValue(n: CFTypeRef, ty: isize, out: *mut c_void) -> u8;
        fn CFStringGetCString(s: CFTypeRef, buf: *mut c_char, size: isize, enc: u32) -> u8;
        fn CFBooleanGetTypeID() -> usize;
        fn CFBooleanGetValue(b: CFTypeRef) -> u8;
        fn CFDictionaryGetTypeID() -> usize;
        fn CFDictionaryGetValue(d: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    }

    /// An owned CF object.
    struct Cf(CFTypeRef);

    impl Drop for Cf {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: we own this reference (a Create/Copy result).
                unsafe { CFRelease(self.0) };
            }
        }
    }

    /// An owned IOKit object.
    struct Io(u32);

    impl Drop for Io {
        fn drop(&mut self) {
            if self.0 != 0 {
                // SAFETY: we own this reference (from an iterator).
                unsafe { IOObjectRelease(self.0) };
            }
        }
    }

    fn prop(entry: u32, key: &str) -> Option<Cf> {
        let k = CString::new(key).ok()?;
        // SAFETY: valid C string; the result is owned by us (Create rule).
        let ks = Cf(unsafe { CFStringCreateWithCString(std::ptr::null(), k.as_ptr(), UTF8) });
        if ks.0.is_null() {
            return None;
        }
        // SAFETY: entry is a live registry entry, ks a CFString.
        let v = Cf(unsafe { IORegistryEntryCreateCFProperty(entry, ks.0, std::ptr::null(), 0) });
        (!v.0.is_null()).then_some(v)
    }

    fn num(entry: u32, key: &str) -> Option<i64> {
        let v = prop(entry, key)?;
        // SAFETY: v is a live CF object; we check its type before reading.
        unsafe {
            if CFGetTypeID(v.0) != CFNumberGetTypeID() {
                return None;
            }
            let mut out = 0i64;
            (CFNumberGetValue(v.0, SINT64, (&mut out as *mut i64).cast()) != 0).then_some(out)
        }
    }

    fn flag(entry: u32, key: &str) -> bool {
        let Some(v) = prop(entry, key) else { return false };
        // SAFETY: v is a live CF object; its type is checked first.
        unsafe { CFGetTypeID(v.0) == CFBooleanGetTypeID() && CFBooleanGetValue(v.0) != 0 }
    }

    fn string(entry: u32, key: &str) -> Option<String> {
        let v = prop(entry, key)?;
        cf_string(v.0)
    }

    /// A CFString's text (None: not a string).
    fn cf_string(v: CFTypeRef) -> Option<String> {
        let mut buf = vec![0u8; 512];
        // SAFETY: v is a live CF object (type checked); buf is writable
        // for its length and CFStringGetCString NUL-terminates.
        unsafe {
            if CFGetTypeID(v) != CFStringGetTypeID() {
                return None;
            }
            if CFStringGetCString(v, buf.as_mut_ptr().cast(), buf.len() as isize, UTF8) == 0 {
                return None;
            }
        }
        let s = CStr::from_bytes_until_nul(&buf).ok()?.to_str().ok()?;
        clean(s)
    }

    /// The string under `key` in the dictionary property `dict`.
    fn dict_string(entry: u32, dict: &str, key: &str) -> Option<String> {
        let d = prop(entry, dict)?;
        let k = CString::new(key).ok()?;
        // SAFETY: valid C string; the result is owned by us (Create rule).
        let ks = Cf(unsafe { CFStringCreateWithCString(std::ptr::null(), k.as_ptr(), UTF8) });
        if ks.0.is_null() {
            return None;
        }
        // SAFETY: d is a live CF object, type checked before the lookup;
        // the value is borrowed from it (Get rule) and used while d lives.
        unsafe {
            if CFGetTypeID(d.0) != CFDictionaryGetTypeID() {
                return None;
            }
            let v = CFDictionaryGetValue(d.0, ks.0);
            if v.is_null() {
                return None;
            }
            cf_string(v)
        }
    }

    fn children(entry: u32) -> Vec<Io> {
        let mut out = Vec::new();
        let mut it = 0u32;
        // SAFETY: entry is live; "IOService" is a valid plane name.
        if unsafe { IORegistryEntryGetChildIterator(entry, c"IOService".as_ptr(), &mut it) } != 0 {
            return out;
        }
        let it = Io(it);
        loop {
            // SAFETY: it is a live iterator.
            let child = Io(unsafe { IOIteratorNext(it.0) });
            if child.0 == 0 {
                break;
            }
            out.push(child);
        }
        out
    }

    /// Walk `entry`'s subtree for disks. `internal`: what the nearest
    /// storage device above said about where it sits; `inside`: the disk
    /// (in `out`) this part of the tree belongs to.
    fn find_disks(entry: u32, depth: u32, internal: bool, out: &mut Vec<HostDisk>, inside: Option<usize>) {
        if depth > 24 || out.len() > 16 {
            return;
        }
        for c in children(entry) {
            // A device behind a hub has disks of its own.
            if is_device(c.0) {
                continue;
            }
            let mut internal = internal;
            if let Some(loc) = dict_string(c.0, "Protocol Characteristics", "Physical Interconnect Location") {
                internal = loc != "External";
            }
            let mut here = inside;
            // SAFETY: c is live.
            if unsafe { IOObjectConformsTo(c.0, c"IOMedia".as_ptr()) } != 0 {
                match (inside, string(c.0, "BSD Name")) {
                    (None, Some(bsd)) if flag(c.0, "Whole") => {
                        out.push(HostDisk {
                            bsd: bsd.clone(),
                            bytes: num(c.0, "Size").unwrap_or(0).max(0) as u64,
                            block: num(c.0, "Preferred Block Size").unwrap_or(512).clamp(0, u32::MAX as i64) as u32,
                            major: num(c.0, "BSD Major").unwrap_or(-1) as u32,
                            minor: num(c.0, "BSD Minor").unwrap_or(-1) as u32,
                            removable: flag(c.0, "Removable"),
                            ejectable: flag(c.0, "Ejectable"),
                            writable: flag(c.0, "Writable"),
                            internal,
                            parts: vec![bsd],
                        });
                        here = Some(out.len() - 1);
                    }
                    (Some(i), Some(bsd)) => {
                        if !out[i].parts.contains(&bsd) && out[i].parts.len() < 64 {
                            out[i].parts.push(bsd);
                        }
                    }
                    _ => {}
                }
            }
            find_disks(c.0, depth + 1, internal, out, here);
        }
    }

    pub fn disks(vendor: u16, product: u16, serial: Option<&str>) -> Vec<HostDisk> {
        let mut out = Vec::new();
        for d in matching(c"IOUSBHostDevice") {
            let ok = num(d.0, "idVendor") == Some(vendor as i64)
                && num(d.0, "idProduct") == Some(product as i64)
                && serial.is_none_or(|s| string(d.0, "kUSBSerialNumberString").or_else(|| string(d.0, "USB Serial Number")).as_deref() == Some(s));
            if ok {
                find_disks(d.0, 0, false, &mut out, None);
            }
        }
        out.retain(|d| super::bsd_disk_ok(&d.bsd));
        out
    }

    pub fn disk_named(bsd: &str) -> Option<HostDisk> {
        for m in matching(c"IOMedia") {
            if string(m.0, "BSD Name").as_deref() != Some(bsd) || !flag(m.0, "Whole") {
                continue;
            }
            let mut out = vec![HostDisk {
                bsd: bsd.to_string(),
                bytes: num(m.0, "Size").unwrap_or(0).max(0) as u64,
                block: num(m.0, "Preferred Block Size").unwrap_or(512).clamp(0, u32::MAX as i64) as u32,
                major: num(m.0, "BSD Major").unwrap_or(-1) as u32,
                minor: num(m.0, "BSD Minor").unwrap_or(-1) as u32,
                removable: flag(m.0, "Removable"),
                ejectable: flag(m.0, "Ejectable"),
                writable: flag(m.0, "Writable"),
                internal: false,
                parts: vec![bsd.to_string()],
            }];
            find_disks(m.0, 0, false, &mut out, Some(0));
            return out.pop();
        }
        None
    }

    pub fn mounts() -> Vec<(String, std::path::PathBuf)> {
        use std::os::unix::ffi::OsStrExt;
        // SAFETY: a size query (NULL buffer).
        let n = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
        if n <= 0 {
            return Vec::new();
        }
        let mut v: Vec<libc::statfs> = Vec::with_capacity(n as usize + 8);
        // SAFETY: room for that many records; getfsstat fills at most
        // the byte size given and returns how many it did.
        let got = unsafe { libc::getfsstat(v.as_mut_ptr(), (v.capacity() * std::mem::size_of::<libc::statfs>()) as i32, libc::MNT_NOWAIT) };
        if got <= 0 {
            return Vec::new();
        }
        // SAFETY: getfsstat initialized `got` records.
        unsafe { v.set_len(got as usize) };
        let text = |a: &[libc::c_char]| -> Vec<u8> { a.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect() };
        v.iter()
            .filter_map(|f| {
                let from = String::from_utf8(text(&f.f_mntfromname)).ok()?;
                let dev = from.strip_prefix("/dev/")?.to_string();
                let on = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&text(&f.f_mntonname)));
                Some((dev, on))
            })
            .collect()
    }

    fn interfaces(entry: u32, classes: &mut Vec<u8>, owners: &mut Vec<String>, depth: u32) {
        if depth > 3 {
            return;
        }
        let mut it = 0u32;
        // SAFETY: entry is live; "IOService" is a valid plane name.
        if unsafe { IORegistryEntryGetChildIterator(entry, c"IOService".as_ptr(), &mut it) } != 0 {
            return;
        }
        let it = Io(it);
        loop {
            // SAFETY: it is a live iterator.
            let child = Io(unsafe { IOIteratorNext(it.0) });
            if child.0 == 0 {
                break;
            }
            // SAFETY: child is live.
            if unsafe { IOObjectConformsTo(child.0, c"IOUSBHostInterface".as_ptr()) } != 0 {
                if let Some(c) = num(child.0, "bInterfaceClass") {
                    classes.push(c as u8);
                }
                if let Some(o) = string(child.0, "UsbExclusiveOwner") {
                    if !owners.contains(&o) {
                        owners.push(o);
                    }
                }
            } else if !is_device(child.0) {
                // A hub's own, not those of the devices behind it.
                interfaces(child.0, classes, owners, depth + 1);
            }
        }
    }

    fn is_device(entry: u32) -> bool {
        // SAFETY: entry is live.
        unsafe { IOObjectConformsTo(entry, c"IOUSBHostDevice".as_ptr()) != 0 }
    }

    /// Registry entries of an IOKit class.
    fn matching(class: &std::ffi::CStr) -> Vec<Io> {
        let mut out = Vec::new();
        let mut it = 0u32;
        // SAFETY: IOServiceMatching returns a dictionary that
        // IOServiceGetMatchingServices consumes.
        unsafe {
            let m = IOServiceMatching(class.as_ptr());
            if m.is_null() || IOServiceGetMatchingServices(0, m, &mut it) != 0 {
                return out;
            }
        }
        let it = Io(it);
        loop {
            // SAFETY: it is a live iterator.
            let d = Io(unsafe { IOIteratorNext(it.0) });
            if d.0 == 0 {
                break;
            }
            out.push(d);
        }
        out
    }

    pub fn list() -> Vec<HostDevice> {
        let mut out = Vec::new();
        for d in matching(c"IOUSBHostDevice") {
            let (Some(vendor), Some(product), Some(loc)) = (num(d.0, "idVendor"), num(d.0, "idProduct"), num(d.0, "locationID")) else {
                continue;
            };
            // locationID: bus in the top byte, then one nibble per port.
            let loc = loc as u32;
            let ports: Vec<u8> = (0..6).map(|i| ((loc >> (20 - 4 * i)) & 0xf) as u8).take_while(|&p| p != 0).collect();
            let speed = match num(d.0, "USBSpeed") {
                Some(1) => Some(Speed::Full),
                Some(2) => Some(Speed::Low),
                Some(3) => Some(Speed::High),
                Some(4) => Some(Speed::Super),
                Some(5..=6) => Some(Speed::SuperPlus),
                _ => None,
            };
            let mut dev = HostDevice {
                vendor: vendor as u16,
                product: product as u16,
                manufacturer: string(d.0, "kUSBVendorString").or_else(|| string(d.0, "USB Vendor Name")),
                name: string(d.0, "kUSBProductString").or_else(|| string(d.0, "USB Product Name")),
                serial: string(d.0, "kUSBSerialNumberString").or_else(|| string(d.0, "USB Serial Number")),
                class: num(d.0, "bDeviceClass").unwrap_or(0) as u8,
                speed,
                bus: (loc >> 24) as u8,
                ports,
                interface_classes: Vec::new(),
                claimed_by: Vec::new(),
                // Set on a device re-enumerated with capture: only root
                // or Apple's VM entitlement may open it from then on.
                captured: flag(d.0, "NeedsDeviceAccessEntitlement"),
            };
            interfaces(d.0, &mut dev.interface_classes, &mut dev.claimed_by, 0);
            out.push(dev);
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn reads_registry_properties() {
            // Every Mac has one of these, with a UUID string.
            let e = matching(c"IOPlatformExpertDevice");
            let uuid = e.first().and_then(|d| string(d.0, "IOPlatformUUID")).unwrap();
            assert_eq!(uuid.len(), 36);
            assert!(e.first().and_then(|d| num(d.0, "IOPlatformUUID")).is_none(), "a string is not a number");
            for d in list() {
                assert!(d.vendor != 0 || d.product != 0);
                // Whatever disks a device has are USB disks.
                for k in disks(d.vendor, d.product, d.serial.as_deref()) {
                    assert!(super::super::bsd_disk_ok(&k.bsd) && k.parts[0] == k.bsd, "{k:?}");
                    assert!(!k.internal, "{k:?}");
                }
            }
            // The boot volume is mounted, and is no USB device's.
            assert!(mounts().iter().any(|(_, on)| on == std::path::Path::new("/")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_names_and_fitness() {
        for ok in ["disk0", "disk5", "disk1234"] {
            assert!(bsd_disk_ok(ok), "{ok}");
        }
        for bad in ["disk", "disk5s1", "rdisk5", "disk-1", "disk12345", "../disk5", "disk5\n"] {
            assert!(!bsd_disk_ok(bad), "{bad}");
        }
        let stick = HostDisk { bsd: "disk5".into(), removable: true, ejectable: true, ..Default::default() };
        assert!(stick.takeable().is_ok());
        assert!(HostDisk { internal: true, ..stick.clone() }.takeable().is_err());
        assert!(HostDisk { removable: false, ejectable: false, ..stick }.takeable().is_err());
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::Path;

    use super::{clean, HostDevice};
    use crate::proto::Speed;

    fn read(p: &Path, f: &str) -> Option<String> {
        let s = std::fs::read_to_string(p.join(f)).ok()?;
        clean(&s)
    }

    fn hex(p: &Path, f: &str) -> Option<u16> {
        u16::from_str_radix(&read(p, f)?, 16).ok()
    }

    pub fn list() -> Vec<HostDevice> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir("/sys/bus/usb/devices") else { return out };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            // "1-2.3" are devices; "usb1" root hubs, "1-2:1.0" interfaces.
            let Some((bus, path)) = name.split_once('-') else { continue };
            if name.contains(':') {
                continue;
            }
            let p = e.path();
            let (Ok(bus), Some(vendor), Some(product)) = (bus.parse::<u8>(), hex(&p, "idVendor"), hex(&p, "idProduct")) else { continue };
            let ports: Vec<u8> = path.split('.').filter_map(|x| x.parse().ok()).collect();
            let speed = match read(&p, "speed").as_deref() {
                Some("1.5") => Some(Speed::Low),
                Some("12") => Some(Speed::Full),
                Some("480") => Some(Speed::High),
                Some("5000") => Some(Speed::Super),
                Some(s) if s.parse::<u32>().is_ok_and(|v| v > 5000) => Some(Speed::SuperPlus),
                _ => None,
            };
            let mut dev = HostDevice {
                vendor,
                product,
                manufacturer: read(&p, "manufacturer"),
                name: read(&p, "product"),
                serial: read(&p, "serial"),
                class: hex(&p, "bDeviceClass").unwrap_or(0) as u8,
                speed,
                bus,
                ports,
                interface_classes: Vec::new(),
                claimed_by: Vec::new(),
                captured: false,
            };
            if let Ok(ifs) = std::fs::read_dir(&p) {
                for i in ifs.flatten() {
                    if !i.file_name().to_string_lossy().starts_with(&format!("{name}:")) {
                        continue;
                    }
                    if let Some(c) = hex(&i.path(), "bInterfaceClass") {
                        dev.interface_classes.push(c as u8);
                    }
                    if let Ok(d) = std::fs::read_link(i.path().join("driver")) {
                        if let Some(n) = d.file_name().and_then(|n| clean(&n.to_string_lossy())) {
                            if !dev.claimed_by.contains(&n) {
                                dev.claimed_by.push(n);
                            }
                        }
                    }
                }
            }
            out.push(dev);
        }
        out.sort_by(|a, b| (a.bus, &a.ports).cmp(&(b.bus, &b.ports)));
        out
    }
}

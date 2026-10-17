//! `vroom usb list`, and the passthrough helper the VMM starts.

use vroom_usb::list::{list, HostDevice};
use vroom_usb::proto::Speed;

#[derive(clap::Subcommand, Debug)]
pub enum UsbCmd {
    /// List the host's USB devices, with the `id` a `[[usb]]` entry takes.
    List,
    /// Reset a device through vroom-netd's root helper: macOS
    /// re-enumerates it and its drivers take it back (a USB stick mounts
    /// again). For a device a VM left taken from macOS.
    Reset {
        /// vendor:product
        id: String,
        #[arg(long)]
        serial: Option<String>,
    },
}

#[derive(clap::Args, Debug)]
pub struct HostArgs {
    /// vendor:product
    #[arg(long)]
    id: String,
    #[arg(long)]
    serial: Option<String>,
}

fn speed(s: Option<Speed>) -> &'static str {
    match s {
        Some(Speed::Low) => "low",
        Some(Speed::Full) => "full",
        Some(Speed::High) => "high",
        Some(Speed::Super) => "super",
        Some(Speed::SuperPlus) => "super+",
        None => "?",
    }
}

fn line(d: &HostDevice) -> String {
    let mut s = format!("{}  {:<9} {:<6}  {}", d.id(), d.location(), speed(d.speed), d.label());
    if let Some(sn) = &d.serial {
        s += &format!("  serial {sn}");
    }
    if !d.claimed_by.is_empty() {
        s += &format!("\n           in use by {} (a VM can't take it without root)", d.claimed_by.join(", "));
    } else if d.captured {
        s += "\n           taken from macOS by a root helper earlier and not given back (root only; unplug it to reset)";
    }
    if d.is_storage() {
        let mounts = vroom_usb::list::mounts();
        for k in vroom_usb::list::disks(d.vendor, d.product, d.serial.as_deref()) {
            let on: Vec<String> = mounts.iter().filter(|(dev, _)| k.parts.contains(dev)).map(|(_, p)| p.display().to_string()).collect();
            s += &format!(
                "\n           disk /dev/{} ({} MB{}){}",
                k.bsd,
                k.bytes / 1_000_000,
                if k.removable || k.ejectable { ", removable" } else { "" },
                if on.is_empty() { String::new() } else { format!(", mounted at {}", on.join(", ")) }
            );
        }
    }
    s
}

pub fn run(c: UsbCmd) -> Result<(), String> {
    match c {
        UsbCmd::List => {
            let devs = list();
            if devs.is_empty() {
                println!("no USB devices");
                return Ok(());
            }
            println!("ID         WHERE     SPEED   NAME");
            for d in &devs {
                println!("{}", line(d));
            }
            Ok(())
        }
        UsbCmd::Reset { id, serial } => reset(&id, serial),
    }
}

/// Have netd's root helper open the device and reset it, then let it go.
fn reset(id: &str, serial: Option<String>) -> Result<(), String> {
    use std::io::Write;
    use vroom_net::netd;
    use vroom_usb::proto::{read_event, Event, Request};
    let req = netd::UsbRequest { version: netd::VERSION, vm: "reset".into(), usb: netd::UsbWanted { id: id.into(), serial } };
    let mut sock = netd::usb(std::path::Path::new(netd::DEFAULT_SOCKET), &req).map_err(|e| e.to_string())?;
    sock.set_read_timeout(Some(std::time::Duration::from_secs(10))).map_err(|e| e.to_string())?;
    let mut r = std::io::BufReader::new(sock.try_clone().map_err(|e| e.to_string())?);
    match read_event(&mut r) {
        Ok(Some(Event::Attached { .. })) => {}
        Ok(_) => return Err("the helper didn't find the device".into()),
        Err(e) => return Err(format!("the helper didn't answer: {e}")),
    }
    sock.write_all(&Request::Reset.encode()).map_err(|e| e.to_string())?;
    // The reset takes a moment (a re-enumeration); then we hang up and
    // the helper gives the device back as it ends.
    sock.set_read_timeout(Some(std::time::Duration::from_secs(3))).map_err(|e| e.to_string())?;
    while let Ok(Some(_)) = read_event(&mut r) {}
    drop(sock);
    println!("{id}: reset; macOS takes it back in a few seconds");
    Ok(())
}

/// The helper: the VMM hands us a stream socket as stdin.
pub fn host(a: HostArgs) -> Result<(), String> {
    use std::os::fd::FromRawFd;
    let (vendor, product) = vroom_config::usb_id(&a.id).ok_or_else(|| format!("bad --id {:?}", a.id))?;
    // SAFETY: fd 0 is the socket our parent set up for us; nobody else in
    // this process uses stdin.
    let sock = unsafe { std::os::unix::net::UnixStream::from_raw_fd(0) };
    let sel = vroom_usb::Selector { vendor, product, serial: a.serial };
    let lockdown = || -> std::io::Result<()> {
        // macOS: the VMM started us under sandbox-exec already.
        #[cfg(target_os = "linux")]
        vroom_sandbox::apply_usb_helper().map_err(std::io::Error::other)?;
        Ok(())
    };
    vroom_usb::helper_main(sock, sel, lockdown).map_err(|e| e.to_string())
}

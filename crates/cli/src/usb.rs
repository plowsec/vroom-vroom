//! `vroom usb list`, and the passthrough helper the VMM starts.

use vroom_usb::list::{list, HostDevice};
use vroom_usb::proto::Speed;

#[derive(clap::Subcommand, Debug)]
pub enum UsbCmd {
    /// List the host's USB devices, with the `id` a `[[usb]]` entry takes.
    List,
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
    }
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

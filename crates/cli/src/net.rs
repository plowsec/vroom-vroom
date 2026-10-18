//! `vroom net`: the host-only networks vroom-netd offers, and the address
//! each of your running VMs' adapters on them got.

use std::path::Path;

use vroom_net::leases;
use vroom_net::netd::{self, Attach, Status};

fn mac(m: &[u8; 6]) -> String {
    m.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// The report, from netd's status and the lease file's text.
fn report(st: &Status, leases_text: &str) -> String {
    let mut out = String::new();
    if !st.host_only_dhcp {
        out += if cfg!(target_os = "macos") {
            "vroom-netd is from an older vroom: host-only networks give no addresses and the Mac isn't on them. Set it up again (app: ⌘K, \"Set up vroom-netd again\").\n"
        } else {
            "host-only: no DHCP on Linux yet; give guests static addresses.\n"
        };
    }
    for n in &st.networks {
        out += &format!("host-only {}: this machine {}\n", n.name, n.gateway);
    }
    if st.networks.is_empty() && !st.host_only.is_empty() {
        out += &format!("host-only: {}\n", st.host_only.join(", "));
    }
    if st.ports.is_empty() {
        out += "no running VM of yours is on a vroom-netd network\n";
        return out;
    }
    let leases = leases::parse(leases_text);
    out += &format!("\n{:<24} {:<4} {:<18} {:<18} ADDRESS\n", "VM", "NIC", "NETWORK", "MAC");
    for p in &st.ports {
        let net = match &p.attach {
            Attach::HostOnly { network } => format!("host-only:{network}"),
            Attach::Bridged { interface } => format!("bridged:{interface}"),
        };
        let addr = match (leases::of(&leases, p.mac), &p.attach) {
            (Some(l), _) if l.name.is_empty() => l.ip.to_string(),
            (Some(l), _) => format!("{} ({})", l.ip, l.name),
            // The LAN's DHCP server knows, not this machine.
            (None, Attach::Bridged { .. }) => "-".into(),
            (None, Attach::HostOnly { .. }) => "no lease yet".into(),
        };
        out += &format!("{:<24} {:<4} {:<18} {:<18} {addr}\n", p.vm, p.nic + 1, net, mac(&p.mac));
    }
    out
}

pub fn run() -> Result<(), String> {
    let st = netd::status(Path::new(netd::DEFAULT_SOCKET)).map_err(|e| e.to_string())?;
    let text = std::fs::read_to_string(leases::PATH).unwrap_or_default();
    print!("{}", report(&st, &text));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vroom_net::netd::{HostOnlyNet, PortInfo};

    #[test]
    fn report_lists_addresses() {
        let p = |vm: &str, m: u8, attach: Attach| PortInfo { vm: vm.into(), nic: 0, attach, mac: [2, 0, 0, 0, 0, m] };
        let st = Status {
            ok: true,
            host_only: vec!["lab".into()],
            networks: vec![HostOnlyNet { name: "lab".into(), gateway: "192.168.77.1/24".into() }],
            host_only_dhcp: true,
            ports: vec![
                p("win", 1, Attach::HostOnly { network: "lab".into() }),
                p("deb", 2, Attach::HostOnly { network: "lab".into() }),
                p("lan", 3, Attach::Bridged { interface: "en0".into() }),
            ],
            ..Default::default()
        };
        let r = report(&st, "{\n\tname=WIN-1\n\tip_address=192.168.77.2\n\thw_address=1,2:0:0:0:0:1\n\tlease=0x10\n}\n");
        assert!(r.contains("host-only lab: this machine 192.168.77.1/24"), "{r}");
        assert!(r.contains("02:00:00:00:00:01") && r.contains("192.168.77.2 (WIN-1)"), "{r}");
        assert!(r.lines().any(|l| l.starts_with("deb") && l.ends_with("no lease yet")), "{r}");
        assert!(r.lines().any(|l| l.starts_with("lan") && l.contains("bridged:en0") && l.ends_with(" -")), "{r}");
        let old = report(&Status { ok: true, host_only: vec!["lab".into()], ..Default::default() }, "");
        assert!(old.contains("host-only") && old.contains("no running VM"), "{old}");
    }
}

//! Tiny DHCP server: one reply to one request, for a lease the caller
//! picked (NAT: one guest, one fixed lease; vroom-netd's host-only
//! networks: one per MAC).

use std::net::Ipv4Addr;

const OP_REQUEST: u8 = 1;
const OP_REPLY: u8 = 2;
const MAGIC: [u8; 4] = [99, 130, 83, 99];

const DISCOVER: u8 = 1;
const OFFER: u8 = 2;
const REQUEST: u8 = 3;
const ACK: u8 = 5;
const NAK: u8 = 6;

#[derive(Debug, Clone, Copy)]
pub struct Lease {
    pub client: Ipv4Addr,
    pub server: Ipv4Addr,
    pub mask: Ipv4Addr,
    /// None: no default route (host-only).
    pub router: Option<Ipv4Addr>,
    pub dns: Option<Ipv4Addr>,
    pub seconds: u32,
}

/// What a request says about its client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    /// The client hardware address (Ethernet).
    pub mac: [u8; 6],
    /// DISCOVER (1), REQUEST (3), ...
    pub msg_type: u8,
    /// Option 12, printable ASCII only.
    pub host_name: Option<String>,
}

struct Parsed {
    msg_type: Option<u8>,
    requested: Option<Ipv4Addr>,
    server: Option<Ipv4Addr>,
    host_name: Option<String>,
}

/// A BOOTREQUEST over Ethernet with its options walked; None if malformed.
fn parse(req: &[u8]) -> Option<Parsed> {
    if req.len() < 240 || req[0] != OP_REQUEST || req[1] != 1 || req[2] != 6 || req[236..240] != MAGIC {
        return None;
    }
    let mut p = Parsed { msg_type: None, requested: None, server: None, host_name: None };
    // Options: bounded walk.
    let mut i = 240;
    while i < req.len() {
        let code = req[i];
        if code == 255 {
            break;
        }
        if code == 0 {
            i += 1;
            continue;
        }
        let len = *req.get(i + 1)? as usize;
        let val = req.get(i + 2..i + 2 + len)?;
        match (code, len) {
            (53, 1) => p.msg_type = Some(val[0]),
            (50, 4) => p.requested = Some(Ipv4Addr::new(val[0], val[1], val[2], val[3])),
            (54, 4) => p.server = Some(Ipv4Addr::new(val[0], val[1], val[2], val[3])),
            (12, 1..) if val.iter().all(|b| b.is_ascii_graphic()) => p.host_name = String::from_utf8(val.to_vec()).ok(),
            _ => {}
        }
        i += 2 + len;
    }
    Some(p)
}

/// Who is asking, before picking a lease for them.
pub fn client(req: &[u8]) -> Option<Client> {
    let p = parse(req)?;
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&req[28..34]);
    Some(Client { mac, msg_type: p.msg_type?, host_name: p.host_name })
}

/// Handle one DHCP request payload (UDP 68 -> 67). Returns the reply
/// payload, if any.
pub fn handle(req: &[u8], lease: &Lease) -> Option<Vec<u8>> {
    let Parsed { msg_type, requested, server, .. } = parse(req)?;
    let xid = &req[4..8];
    let flags = &req[10..12];
    let chaddr = &req[28..44];
    if server.is_some_and(|s| s != lease.server) {
        return None;
    }
    let reply_type = match msg_type? {
        DISCOVER => OFFER,
        REQUEST => {
            let ciaddr = Ipv4Addr::new(req[12], req[13], req[14], req[15]);
            let asked = requested.unwrap_or(ciaddr);
            if asked == lease.client || asked.is_unspecified() {
                ACK
            } else {
                NAK
            }
        }
        _ => return None,
    };

    let mut r = vec![0u8; 240];
    r[0] = OP_REPLY;
    r[1] = 1;
    r[2] = 6;
    r[4..8].copy_from_slice(xid);
    r[10..12].copy_from_slice(flags);
    if reply_type != NAK {
        r[16..20].copy_from_slice(&lease.client.octets());
    }
    r[20..24].copy_from_slice(&lease.server.octets());
    r[28..44].copy_from_slice(chaddr);
    r[236..240].copy_from_slice(&MAGIC);
    let mut opt = |code: u8, val: &[u8]| {
        r.push(code);
        r.push(val.len() as u8);
        r.extend_from_slice(val);
    };
    opt(53, &[reply_type]);
    opt(54, &lease.server.octets());
    if reply_type != NAK {
        opt(51, &lease.seconds.to_be_bytes());
        opt(58, &(lease.seconds / 2).to_be_bytes());
        opt(59, &(lease.seconds * 7 / 8).to_be_bytes());
        opt(1, &lease.mask.octets());
        if let Some(r) = lease.router {
            opt(3, &r.octets());
        }
        if let Some(d) = lease.dns {
            opt(6, &d.octets());
        }
        opt(26, &1500u16.to_be_bytes());
    }
    r.push(255);
    // Pad to the classic BOOTP minimum.
    if r.len() < 300 {
        r.resize(300, 0);
    }
    Some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease() -> Lease {
        Lease {
            client: Ipv4Addr::new(10, 0, 2, 15),
            server: Ipv4Addr::new(10, 0, 2, 2),
            mask: Ipv4Addr::new(255, 255, 255, 0),
            router: Some(Ipv4Addr::new(10, 0, 2, 2)),
            dns: Some(Ipv4Addr::new(10, 0, 2, 3)),
            seconds: 86400,
        }
    }

    /// The options of a reply, by code.
    fn options(r: &[u8]) -> Vec<u8> {
        let (mut i, mut out) = (240, Vec::new());
        while i < r.len() && r[i] != 255 {
            out.push(r[i]);
            i += 2 + r[i + 1] as usize;
        }
        out
    }

    fn request(ty: u8, want: Option<[u8; 4]>) -> Vec<u8> {
        let mut r = vec![0u8; 240];
        r[0] = 1;
        r[1] = 1;
        r[2] = 6;
        r[4..8].copy_from_slice(&[1, 2, 3, 4]);
        r[28..34].copy_from_slice(&[2, 0, 0, 0, 0, 9]);
        r[236..240].copy_from_slice(&MAGIC);
        r.extend_from_slice(&[53, 1, ty]);
        if let Some(w) = want {
            r.extend_from_slice(&[50, 4]);
            r.extend_from_slice(&w);
        }
        r.push(255);
        r
    }

    fn msg_type(r: &[u8]) -> u8 {
        assert_eq!(&r[240..242], &[53, 1]);
        r[242]
    }

    #[test]
    fn discover_request() {
        let o = handle(&request(DISCOVER, None), &lease()).unwrap();
        assert_eq!(msg_type(&o), OFFER);
        assert_eq!(&o[16..20], &[10, 0, 2, 15]);
        assert_eq!(&o[4..8], &[1, 2, 3, 4]);
        let a = handle(&request(REQUEST, Some([10, 0, 2, 15])), &lease()).unwrap();
        assert_eq!(msg_type(&a), ACK);
        let n = handle(&request(REQUEST, Some([10, 0, 2, 99])), &lease()).unwrap();
        assert_eq!(msg_type(&n), NAK);
    }

    #[test]
    fn who_asks_and_no_route() {
        let mut r = request(DISCOVER, None);
        r.pop();
        r.extend_from_slice(&[12, 5, b'd', b'e', b'b', b'-', b'1', 255]);
        assert_eq!(client(&r), Some(Client { mac: [2, 0, 0, 0, 0, 9], msg_type: DISCOVER, host_name: Some("deb-1".into()) }));
        let mut odd = request(DISCOVER, None);
        odd.pop();
        odd.extend_from_slice(&[12, 2, b'a', b'\n', 255]);
        assert_eq!(client(&odd).unwrap().host_name, None);
        assert!(options(&handle(&r, &lease()).unwrap()).contains(&3));
        let o = handle(&r, &Lease { router: None, dns: None, ..lease() }).unwrap();
        assert!(!options(&o).contains(&3) && !options(&o).contains(&6) && options(&o).contains(&1));
    }

    #[test]
    fn malformed_options() {
        let mut r = request(DISCOVER, None);
        r.truncate(240);
        r.extend_from_slice(&[53, 200]); // length runs off the end
        assert!(handle(&r, &lease()).is_none());
        for n in 0..300 {
            let full = request(REQUEST, Some([10, 0, 2, 15]));
            let _ = handle(&full[..n.min(full.len())], &lease());
        }
    }

    #[test]
    fn selected_server_and_renewal() {
        let mut r = request(REQUEST, Some([10, 0, 2, 15]));
        r.pop();
        r.extend_from_slice(&[54, 4, 10, 0, 2, 99, 255]);
        assert!(handle(&r, &lease()).is_none());
        let n = r.len();
        r[n - 2] = 2;
        assert_eq!(msg_type(&handle(&r, &lease()).unwrap()), ACK);
        let mut renewal = request(REQUEST, None);
        renewal[12..16].copy_from_slice(&[10, 0, 2, 15]);
        assert_eq!(msg_type(&handle(&renewal, &lease()).unwrap()), ACK);
    }
}

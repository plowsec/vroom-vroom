//! Tiny DHCP server: one guest, one fixed lease.

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
    pub dns: Ipv4Addr,
    pub seconds: u32,
}

/// Handle one DHCP request payload (UDP 68 -> 67). Returns the reply
/// payload, if any.
pub fn handle(req: &[u8], lease: &Lease) -> Option<Vec<u8>> {
    if req.len() < 240 || req[0] != OP_REQUEST || req[1] != 1 || req[2] != 6 || req[236..240] != MAGIC {
        return None;
    }
    let xid = &req[4..8];
    let flags = &req[10..12];
    let chaddr = &req[28..44];
    let mut msg_type = None;
    let mut requested = None;
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
            (53, 1) => msg_type = Some(val[0]),
            (50, 4) => requested = Some(Ipv4Addr::new(val[0], val[1], val[2], val[3])),
            _ => {}
        }
        i += 2 + len;
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
        opt(3, &lease.server.octets());
        opt(6, &lease.dns.octets());
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
            dns: Ipv4Addr::new(10, 0, 2, 3),
            seconds: 86400,
        }
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
}

//! Strict TCG2 SHA256 PCR7 replay. Consistency is not independent firmware trust.
use crate::{hash, require, Cursor, Result};
use std::collections::BTreeMap;

pub const MAX_LOG: usize = 1 << 20;
const MAX_EVENT: usize = 65536;
const SHA256: u16 = 0x000b;
const NO_ACTION: u32 = 3;
const EFI_VARIABLE_DRIVER_CONFIG: u32 = 0x8000_0001;

pub struct Replay {
    pub pcr: [u8; 32],
    pub events: usize,
    pub secure_boot_measured: bool,
}

fn secure_boot_event(data: &[u8]) -> Result<bool> {
    let mut c = Cursor::new(data);
    let guid = c.take(16)?;
    let name_len = c.le64()?;
    let data_len = c.le64()?;
    require(
        name_len <= 128 && data_len <= MAX_EVENT as u64,
        "efi_variable_length",
    )?;
    let name = c.take(name_len as usize * 2)?;
    let value = c.take(data_len as usize)?;
    c.finish()?;
    let global = [
        0x61, 0xdf, 0xe4, 0x8b, 0xca, 0x93, 0xd2, 0x11, 0xaa, 0x0d, 0x00, 0xe0, 0x98, 0x03, 0x2b,
        0x8c,
    ];
    let expected: Vec<u8> = "SecureBoot"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    if guid == global && name == expected {
        require(value == [1], "secure_boot_not_measured_enabled")?;
        Ok(true)
    } else {
        Ok(false)
    }
}

pub fn replay(log: &[u8]) -> Result<Replay> {
    require(log.len() <= MAX_LOG, "event_log_too_large")?;
    let mut c = Cursor::new(log);
    require(c.le32()? == 0 && c.le32()? == NO_ACTION, "missing_spec_id")?;
    require(c.take(20)? == [0; 20], "spec_id_digest")?;
    let spec_len = c.le32()? as usize;
    require(spec_len <= MAX_EVENT, "spec_id_length")?;
    let mut spec = Cursor::new(c.take(spec_len)?);
    require(spec.take(16)? == b"Spec ID Event03\0", "spec_id_signature")?;
    spec.le32()?; // platformClass
    let minor = spec.byte()?;
    let major = spec.byte()?;
    spec.byte()?; // errata
    let uintn = spec.byte()?;
    require(
        major == 2 && minor == 0 && matches!(uintn, 1 | 2),
        "spec_id_version",
    )?;
    let count = spec.le32()? as usize;
    require((1..=16).contains(&count), "spec_id_algorithms")?;
    let mut algorithms = BTreeMap::new();
    for _ in 0..count {
        let alg = spec.le16()?;
        let width = spec.le16()? as usize;
        require(
            (1..=64).contains(&width) && algorithms.insert(alg, width).is_none(),
            "spec_id_digest_width",
        )?;
        let known = match alg {
            0x0004 => Some(20),
            SHA256 => Some(32),
            0x000c => Some(48),
            0x000d => Some(64),
            0x0012 => Some(32),
            _ => None,
        };
        require(
            known.is_none_or(|n| n == width),
            "spec_id_known_digest_width",
        )?;
    }
    require(algorithms.get(&SHA256) == Some(&32), "no_sha256_event_bank")?;
    let vendor_size = spec.byte()? as usize;
    spec.take(vendor_size)?;
    spec.finish()?;
    let mut result = Replay {
        pcr: [0; 32],
        events: 0,
        secure_boot_measured: false,
    };
    let mut records = 0;
    while c.left() > 0 {
        records += 1;
        require(records <= 4096, "too_many_events")?;
        let pcr = c.le32()?;
        let event_type = c.le32()?;
        require(pcr < 24, "invalid_event_pcr")?;
        let count = c.le32()? as usize;
        require(count > 0 && count <= algorithms.len(), "event_digest_count")?;
        let mut seen = BTreeMap::new();
        let mut sha = None;
        for _ in 0..count {
            let alg = c.le16()?;
            let width = *algorithms
                .get(&alg)
                .ok_or_else(|| crate::fail("undeclared_digest_algorithm"))?;
            require(seen.insert(alg, ()).is_none(), "duplicate_event_digest")?;
            let digest = c.take(width)?;
            if alg == SHA256 {
                sha = Some(digest);
            }
        }
        let size = c.le32()? as usize;
        require(size <= MAX_EVENT, "event_data_too_large")?;
        let data = c.take(size)?;
        if pcr != 7 || event_type == NO_ACTION {
            continue;
        }
        let sha = sha.ok_or_else(|| crate::fail("missing_pcr7_sha256_digest"))?;
        // PCR7 firmware variable/action/separator events hash the event data.
        // Reject unknown event types rather than assuming that rule applies.
        require(
            matches!(event_type, 4 | 5 | EFI_VARIABLE_DRIVER_CONFIG | 0x8000_00e0),
            "unsupported_pcr7_event_type",
        )?;
        require(
            hash(data).as_slice() == sha,
            "pcr7_event_data_hash_mismatch",
        )?;
        if event_type == EFI_VARIABLE_DRIVER_CONFIG {
            result.secure_boot_measured |= secure_boot_event(data)?;
        }
        let mut extend = [0; 64];
        extend[..32].copy_from_slice(&result.pcr);
        extend[32..].copy_from_slice(sha);
        result.pcr = hash(&extend);
        result.events += 1;
    }
    require(
        result.events > 0 && result.secure_boot_measured,
        "missing_secure_boot_measurement",
    )?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec() -> Vec<u8> {
        let mut s = b"Spec ID Event03\0".to_vec();
        s.extend(0u32.to_le_bytes());
        s.extend([0, 2, 0, 2]);
        s.extend(2u32.to_le_bytes());
        s.extend(4u16.to_le_bytes());
        s.extend(20u16.to_le_bytes());
        s.extend(SHA256.to_le_bytes());
        s.extend(32u16.to_le_bytes());
        s.push(0);
        let mut v = 0u32.to_le_bytes().to_vec();
        v.extend(NO_ACTION.to_le_bytes());
        v.extend([0; 20]);
        v.extend((s.len() as u32).to_le_bytes());
        v.extend(s);
        v
    }
    fn event(v: &mut Vec<u8>, kind: u32, data: &[u8], sha: [u8; 32]) {
        v.extend(7u32.to_le_bytes());
        v.extend(kind.to_le_bytes());
        v.extend(2u32.to_le_bytes());
        v.extend(4u16.to_le_bytes());
        v.extend([7; 20]);
        v.extend(SHA256.to_le_bytes());
        v.extend(sha);
        v.extend((data.len() as u32).to_le_bytes());
        v.extend(data);
    }
    fn secure() -> Vec<u8> {
        let mut data = vec![
            0x61, 0xdf, 0xe4, 0x8b, 0xca, 0x93, 0xd2, 0x11, 0xaa, 0x0d, 0x00, 0xe0, 0x98, 0x03,
            0x2b, 0x8c,
        ];
        data.extend(10u64.to_le_bytes());
        data.extend(1u64.to_le_bytes());
        data.extend("SecureBoot".encode_utf16().flat_map(u16::to_le_bytes));
        data.push(1);
        data
    }
    fn valid() -> Vec<u8> {
        let mut v = spec();
        let data = secure();
        event(&mut v, EFI_VARIABLE_DRIVER_CONFIG, &data, hash(&data));
        v
    }
    #[test]
    fn spec_id_widths_pcr7_replay_and_no_action() {
        let mut v = valid();
        let digest = hash(&secure());
        let mut extended = [0; 64];
        extended[32..].copy_from_slice(&digest);
        let expected = hash(&extended);
        event(&mut v, NO_ACTION, b"not extended", [0; 32]);
        let r = replay(&v).unwrap();
        assert_eq!(r.pcr, expected);
        assert_eq!(r.events, 1);
        assert!(r.secure_boot_measured);
    }
    #[test]
    fn every_truncation_before_complete_event_is_rejected() {
        let v = valid();
        for size in 0..v.len() {
            assert!(replay(&v[..size]).is_err(), "accepted {size}");
        }
        let mut trailing = v;
        trailing.push(0);
        assert!(replay(&trailing).is_err());
    }
    #[test]
    fn disabled_secure_boot_bad_event_hash_and_unknown_event_rejected() {
        let mut disabled = secure();
        *disabled.last_mut().unwrap() = 0;
        let mut v = spec();
        event(
            &mut v,
            EFI_VARIABLE_DRIVER_CONFIG,
            &disabled,
            hash(&disabled),
        );
        assert!(replay(&v).is_err());
        let mut v = spec();
        event(&mut v, EFI_VARIABLE_DRIVER_CONFIG, &secure(), [0; 32]);
        assert!(replay(&v).is_err());
        let mut v = valid();
        event(&mut v, 0x1234, b"x", hash(b"x"));
        assert!(replay(&v).is_err());
    }
    #[test]
    fn algorithm_table_and_record_limits_fail_closed() {
        let mut v = valid();
        v[62..64].copy_from_slice(&31u16.to_le_bytes());
        assert!(replay(&v).is_err());
        let mut v = valid();
        v[56..58].copy_from_slice(&SHA256.to_le_bytes());
        assert!(replay(&v).is_err());
        assert!(replay(&vec![0; MAX_LOG + 1]).is_err());
        let mut v = spec();
        let start = v.len();
        event(
            &mut v,
            EFI_VARIABLE_DRIVER_CONFIG,
            &secure(),
            hash(&secure()),
        );
        v[start + 8..start + 12].copy_from_slice(&17u32.to_le_bytes());
        assert!(replay(&v).is_err());
    }
}

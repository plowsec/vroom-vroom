//! Numbers and times for people.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn bytes(b: u64) -> String {
    const G: f64 = (1u64 << 30) as f64;
    const M: f64 = (1u64 << 20) as f64;
    let f = b as f64;
    if f >= 1024.0 * G {
        format!("{:.1} TB", f / (1024.0 * G))
    } else if f >= 10.0 * G {
        format!("{:.0} GB", f / G)
    } else if f >= G {
        format!("{:.1} GB", f / G)
    } else if f >= M {
        format!("{:.0} MB", f / M)
    } else {
        format!("{:.0} KB", f / 1024.0)
    }
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// "just now", "5 min ago", "3 h ago", else a local date.
pub fn ago(unix: u64) -> String {
    let d = now().saturating_sub(unix);
    if d < 60 {
        "Just now".into()
    } else if d < 3600 {
        format!("{} min ago", d / 60)
    } else if d < 86_400 {
        format!("{} h ago", d / 3600)
    } else {
        local_date(unix)
    }
}

#[allow(unsafe_code)]
fn local_date(unix: u64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let t = unix as libc::time_t;
    // SAFETY: zeroed tm is valid plain data; localtime_r fills it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointers for the call.
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return format!("{unix}");
    }
    format!("{} {} {:02}:{:02}", MONTHS[tm.tm_mon.clamp(0, 11) as usize], tm.tm_mday, tm.tm_hour, tm.tm_min)
}

#[cfg(test)]
mod tests {
    #[test]
    fn sizes() {
        assert_eq!(super::bytes(64 << 30), "64 GB");
        assert_eq!(super::bytes(5_570_000_000), "5.2 GB");
        assert_eq!(super::bytes(300 << 20), "300 MB");
    }
}

//! Linux: who is on screen, for the service to serve only that user's
//! session helper.
//! - logind: seat0's `ACTIVE_UID`.
//! - Without it: the owner of the active VT's device (login hands
//!   `/dev/ttyN` to the user).
//! - Neither: nobody, and only root's helper is served.

/// seat0's active user from logind's `/run/systemd/seats/seat0`.
pub fn logind_uid(seat0: &str) -> Option<u32> {
    seat0.lines().find_map(|l| l.strip_prefix("ACTIVE_UID=")).and_then(|v| v.trim().parse().ok())
}

/// The active VT's device from `/sys/class/tty/tty0/active` ("tty2" ->
/// "/dev/tty2"). Nothing but `tty<digits>`.
pub fn vt_device(active: &str) -> Option<String> {
    let n = active.trim().strip_prefix("tty")?;
    (!n.is_empty() && n.len() <= 3 && n.bytes().all(|b| b.is_ascii_digit())).then(|| format!("/dev/tty{n}"))
}

/// The active VT's owner: root when nobody is logged in there.
fn vt_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    let dev = vt_device(&std::fs::read_to_string("/sys/class/tty/tty0/active").ok()?)?;
    std::fs::metadata(dev).ok().map(|m| m.uid())
}

/// The user on screen, if we can tell.
pub fn active_uid() -> Option<u32> {
    std::fs::read_to_string("/run/systemd/seats/seat0").ok().as_deref().and_then(logind_uid).or_else(vt_uid)
}

/// Whether to serve a helper run by `peer` with `active` on screen.
pub fn serves(peer: Option<u32>, active: Option<u32>) -> bool {
    peer == Some(0) || (peer.is_some() && peer == active)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_user_on_screen_is_served() {
        assert_eq!(logind_uid("# This is private data.\nACTIVE=c2\nACTIVE_UID=1000\n"), Some(1000));
        assert_eq!(logind_uid("ACTIVE=c1\n"), None);
        assert_eq!(vt_device("tty2\n").as_deref(), Some("/dev/tty2"));
        for bad in ["", "tty", "ttyS0", "tty1/../sda", "pts/0", "tty12345"] {
            assert_eq!(vt_device(bad), None, "{bad}");
        }
        assert!(serves(Some(1000), Some(1000)) && serves(Some(0), None) && serves(Some(0), Some(1000)));
        // Nobody known on screen: root only, not every local user.
        assert!(!serves(Some(1000), None) && !serves(None, None) && !serves(Some(1001), Some(1000)));
    }
}

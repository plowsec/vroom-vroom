use std::fmt;

/// A guest physical address. Newtype so we never mix it up with host pointers.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct GuestAddress(pub u64);

impl GuestAddress {
    pub const fn raw(self) -> u64 {
        self.0
    }

    pub fn checked_add(self, off: u64) -> Option<GuestAddress> {
        self.0.checked_add(off).map(GuestAddress)
    }

    pub fn checked_sub(self, other: GuestAddress) -> Option<u64> {
        self.0.checked_sub(other.0)
    }

    pub const fn unchecked_add(self, off: u64) -> GuestAddress {
        GuestAddress(self.0 + off)
    }

    pub fn align_up(self, align: u64) -> Option<GuestAddress> {
        debug_assert!(align.is_power_of_two());
        self.0
            .checked_add(align - 1)
            .map(|v| GuestAddress(v & !(align - 1)))
    }
}

impl fmt::Debug for GuestAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GPA({:#x})", self.0)
    }
}

impl fmt::Display for GuestAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

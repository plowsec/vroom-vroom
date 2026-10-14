/// Plain-old-data types that can be copied to and from guest memory.
///
/// # Safety
/// Implementors must have no padding, no invalid bit patterns and no
/// pointers. Any byte sequence of `size_of::<Self>()` must be a valid value.
pub unsafe trait ByteValued: Copy + Default + Send + Sync + 'static {
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: trait contract says every byte of Self is initialized POD.
        unsafe {
            std::slice::from_raw_parts(self as *const Self as *const u8, std::mem::size_of::<Self>())
        }
    }

    fn as_bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: trait contract says any byte pattern is a valid Self.
        unsafe {
            std::slice::from_raw_parts_mut(self as *mut Self as *mut u8, std::mem::size_of::<Self>())
        }
    }

    fn from_slice(b: &[u8]) -> Option<Self> {
        if b.len() != std::mem::size_of::<Self>() {
            return None;
        }
        let mut v = Self::default();
        v.as_bytes_mut().copy_from_slice(b);
        Some(v)
    }
}

macro_rules! impl_pod {
    ($($t:ty),*) => {
        // SAFETY: primitive integers have no padding and no invalid values.
        $(unsafe impl ByteValued for $t {})*
    };
}
impl_pod!(u8, u16, u32, u64, i8, i16, i32, i64, usize, isize);

// SAFETY: arrays of POD are POD.
unsafe impl<T: ByteValued, const N: usize> ByteValued for [T; N] where [T; N]: Default {}

//! Initial direct-boot profile. All RAM and boot data are below 4 GiB;
//! the LAPIC, IOAPIC and KVM's reserved pages are outside this RAM slot.

pub const RAM_SIZE: u64 = 512 << 20;
pub const PAGE_SIZE: u64 = 4096;
pub const LARGE_PAGE_SIZE: u64 = 2 << 20;
pub const PAGE_TABLES: u64 = 0x1_0000;
pub const PAGE_TABLES_SIZE: u64 = 3 * PAGE_SIZE;
pub const GDT: u64 = 0x1_4000;
pub const BOOT_PARAMS: u64 = 0x1_5000;
pub const COMMAND_LINE: u64 = 0x2_0000;
pub const COMMAND_LINE_MAX: usize = PAGE_SIZE as usize - 1;
pub const LOW_RAM_END: u64 = 0x9_fc00;
pub const HIGH_RAM_START: u64 = 1 << 20;
pub const KERNEL_MIN: u64 = 16 << 20;
pub const INITRD_MAX: usize = 128 << 20;

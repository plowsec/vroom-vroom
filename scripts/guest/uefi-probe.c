/* Freestanding x64 EFI boot smoke test; built by smoke-x86-uefi.sh. */
typedef unsigned char u8;
typedef unsigned short u16;
typedef unsigned int u32;
typedef unsigned long long u64;

struct config_table { u8 guid[16]; void *table; };
struct system_table {
    u64 header[3];
    void *vendor;
    u32 revision, padding;
    void *console_in_handle, *console_in, *console_out_handle, *console_out;
    void *stderr_handle, *stderr, *runtime_services, *boot_services;
    u64 entry_count;
    struct config_table *entries;
};

static void outb(u16 port, u8 value) {
    __asm__ volatile("outb %0, %1" : : "a"(value), "Nd"(port));
}
static void print(const char *s) { while (*s) outb(0x3f8, (u8)*s++); }
static u64 le(const u8 *p, unsigned n) {
    u64 value = 0;
    for (unsigned i = 0; i < n; i++) value |= (u64)p[i] << (i * 8);
    return value;
}
static int checksum(const u8 *p, u32 n) {
    u8 sum = 0;
    for (u32 i = 0; i < n; i++) sum += p[i];
    return sum == 0;
}
static int signature(const u8 *p, const char *s, unsigned n) {
    for (unsigned i = 0; i < n; i++) if (p[i] != (u8)s[i]) return 0;
    return 1;
}

struct efi_time {
    u16 year;
    u8 month, day, hour, minute, second, padding;
    u32 nanosecond;
    short timezone;
    u8 daylight, padding2;
};

static int check_rtc(struct system_table *st) {
    struct efi_time time = {0};
    /* Runtime services begin after the 24-byte EFI_TABLE_HEADER. */
    u64 (*get_time)(struct efi_time *, void *) =
        *(u64 (**)(struct efi_time *, void *))((u8 *)st->runtime_services + 24);
    return get_time(&time, 0) == 0 && time.year >= 1900 && time.year <= 9999
        && time.month >= 1 && time.month <= 12 && time.day >= 1 && time.day <= 31
        && time.hour < 24 && time.minute < 60 && time.second < 60
        && time.nanosecond < 1000000000;
}

static int check_acpi(struct system_table *st) {
    static const u8 guid[16] = {0x71,0xe8,0x68,0x88,0xf1,0xe4,0xd3,0x11,0xbc,0x22,0,0x80,0xc7,0x3c,0x88,0x81};
    const u8 *rsdp = 0;
    for (u64 i = 0; i < st->entry_count; i++) {
        unsigned j;
        for (j = 0; j < 16; j++) if (st->entries[i].guid[j] != guid[j]) break;
        if (j == 16) rsdp = st->entries[i].table;
    }
    if (!rsdp || !signature(rsdp, "RSD PTR ", 8) || !checksum(rsdp, 20) || !checksum(rsdp, 36)) return 0;
    const u8 *xsdt = (const u8 *)le(rsdp + 24, 8);
    if (!xsdt || !signature(xsdt, "XSDT", 4)) return 0;
    u32 size = (u32)le(xsdt + 4, 4);
    if (size < 36 || size > 65536 || !checksum(xsdt, size)) return 0;
    unsigned cpus = 0;
    for (u32 off = 36; off + 8 <= size; off += 8) {
        const u8 *table = (const u8 *)le(xsdt + off, 8);
        u32 len = (u32)le(table + 4, 4);
        if (len < 36 || len > 65536 || !checksum(table, len)) return 0;
        if (signature(table, "FACP", 4) && table[8] >= 5) {
            if (len < 220 || table[148] != 1 || table[149] != 32
                || table[172] != 1 || table[173] != 16
                || table[208] != 1 || table[209] != 32
                || le(table + 152, 8) != le(table + 56, 4)
                || le(table + 176, 8) != le(table + 64, 4)
                || le(table + 212, 8) != le(table + 76, 4)) return 0;
        }
        if (signature(table, "APIC", 4)) {
            for (u32 pos = 44; pos + 2 <= len;) {
                u8 entry_len = table[pos + 1];
                if (entry_len < 2 || pos + entry_len > len) return 0;
                if (table[pos] == 0 && entry_len == 8 && (le(table + pos + 4, 4) & 1)) cpus++;
                pos += entry_len;
            }
        }
    }
    return cpus == EXPECTED_CPUS;
}

u64 efi_main(void *image, struct system_table *st) {
    (void)image;
    outb(0x3fb, 0x80); outb(0x3f8, 1); outb(0x3f9, 0);
    outb(0x3fb, 3); outb(0x3fc, 0x0b);
    if (!check_acpi(st)) print("VROOM_X64_UEFI_ACPI_FAIL\r\n");
    else if (!check_rtc(st)) print("VROOM_X64_UEFI_RTC_FAIL\r\n");
    else print("VROOM_X64_UEFI_BOOT_OK\r\n");
    __asm__ volatile("outw %0, %1" : : "a"((u16)0x2000), "Nd"((u16)0xb004));
    return 0;
}

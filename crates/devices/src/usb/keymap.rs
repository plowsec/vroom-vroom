//! Linux evdev key codes -> USB HID keyboard usages (page 0x07).

pub fn hid_usage(evdev: u16) -> Option<u8> {
    Some(match evdev {
        1 => 0x29,                                // Esc
        2..=10 => 0x1e + (evdev - 2) as u8,       // 1..9
        11 => 0x27,                               // 0
        12 => 0x2d,
        13 => 0x2e,
        14 => 0x2a,                               // Backspace
        15 => 0x2b,                               // Tab
        16 => 0x14, 17 => 0x1a, 18 => 0x08, 19 => 0x15, 20 => 0x17, // q w e r t
        21 => 0x1c, 22 => 0x18, 23 => 0x0c, 24 => 0x12, 25 => 0x13, // y u i o p
        26 => 0x2f,
        27 => 0x30,
        28 => 0x28,                               // Enter
        30 => 0x04, 31 => 0x16, 32 => 0x07, 33 => 0x09, 34 => 0x0a, // a s d f g
        35 => 0x0b, 36 => 0x0d, 37 => 0x0e, 38 => 0x0f,             // h j k l
        39 => 0x33,
        40 => 0x34,
        41 => 0x35,
        43 => 0x31,
        44 => 0x1d, 45 => 0x1b, 46 => 0x06, 47 => 0x19, 48 => 0x05, // z x c v b
        49 => 0x11, 50 => 0x10,                                     // n m
        51 => 0x36,
        52 => 0x37,
        53 => 0x38,
        55 => 0x55,                               // KP *
        57 => 0x2c,                               // Space
        58 => 0x39,                               // Caps
        59..=68 => 0x3a + (evdev - 59) as u8,     // F1..F10
        69 => 0x53,                               // NumLock
        70 => 0x47,                               // ScrollLock
        71 => 0x5f, 72 => 0x60, 73 => 0x61, 74 => 0x56,
        75 => 0x5c, 76 => 0x5d, 77 => 0x5e, 78 => 0x57,
        79 => 0x59, 80 => 0x5a, 81 => 0x5b, 82 => 0x62, 83 => 0x63,
        86 => 0x64,                               // 102nd key
        87 => 0x44,                               // F11
        88 => 0x45,                               // F12
        96 => 0x58,                               // KP Enter
        98 => 0x54,                               // KP /
        99 => 0x46,                               // PrintScreen
        102 => 0x4a,                              // Home
        103 => 0x52,                              // Up
        104 => 0x4b,                              // PgUp
        105 => 0x50,                              // Left
        106 => 0x4f,                              // Right
        107 => 0x4d,                              // End
        108 => 0x51,                              // Down
        109 => 0x4e,                              // PgDn
        110 => 0x49,                              // Insert
        111 => 0x4c,                              // Delete
        113 => 0x7f,                              // Mute
        114 => 0x81,                              // Vol-
        115 => 0x80,                              // Vol+
        117 => 0x67,                              // KP =
        119 => 0x48,                              // Pause
        127 => 0x65,                              // Menu
        183..=185 => 0x68 + (evdev - 183) as u8,  // F13..F15
        _ => return None,
    })
}

/// Modifier bit for evdev modifier keys (byte 0 of the boot report).
pub fn modifier_bit(evdev: u16) -> Option<u8> {
    Some(match evdev {
        29 => 0x01,  // LCtrl
        42 => 0x02,  // LShift
        56 => 0x04,  // LAlt
        125 => 0x08, // LMeta
        97 => 0x10,  // RCtrl
        54 => 0x20,  // RShift
        100 => 0x40, // RAlt
        126 => 0x80, // RMeta
        _ => return None,
    })
}

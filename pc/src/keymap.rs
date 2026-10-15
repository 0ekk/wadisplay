//! Set-1 scancodes as WH_KEYBOARD_LL reports them -> HID keyboard usages.

/// HID usage for a key, 0 when unmapped. `ext` is LLKHF_EXTENDED (the E0 prefix).
pub fn usage(scan: u32, ext: bool, vk: u32) -> u8 {
    match vk {
        0x13 => return 0x48, // VK_PAUSE (E1-prefixed, scancode 0x45 like NumLock)
        0x90 => return 0x53, // VK_NUMLOCK
        _ => {}
    }
    if ext {
        return match scan {
            0x1C => 0x58, // keypad Enter
            0x1D => 0xE4, // right Ctrl
            0x20 => 0x7F, // mute
            0x2E => 0x81, // volume down
            0x30 => 0x80, // volume up
            0x35 => 0x54, // keypad /
            0x37 => 0x46, // PrintScreen
            0x38 => 0xE6, // right Alt
            0x47 => 0x4A, // Home
            0x48 => 0x52, // Up
            0x49 => 0x4B, // PageUp
            0x4B => 0x50, // Left
            0x4D => 0x4F, // Right
            0x4F => 0x4D, // End
            0x50 => 0x51, // Down
            0x51 => 0x4E, // PageDown
            0x52 => 0x49, // Insert
            0x53 => 0x4C, // Delete
            0x5B => 0xE3, // left GUI
            0x5C => 0xE7, // right GUI
            0x5D => 0x65, // Application
            _ => 0,
        };
    }
    match scan {
        0x01 => 0x29,                                       // Esc
        0x02..=0x0B => 0x1E + (scan - 0x02) as u8,          // 1..9, 0
        0x0C => 0x2D,
        0x0D => 0x2E,
        0x0E => 0x2A, // Backspace
        0x0F => 0x2B, // Tab
        0x10..=0x19 => [0x14, 0x1A, 0x08, 0x15, 0x17, 0x1C, 0x18, 0x0C, 0x12, 0x13][(scan - 0x10) as usize], // QWERTYUIOP
        0x1A => 0x2F,
        0x1B => 0x30,
        0x1C => 0x28, // Enter
        0x1D => 0xE0, // left Ctrl
        0x1E..=0x26 => [0x04, 0x16, 0x07, 0x09, 0x0A, 0x0B, 0x0D, 0x0E, 0x0F][(scan - 0x1E) as usize], // ASDFGHJKL
        0x27 => 0x33,
        0x28 => 0x34,
        0x29 => 0x35,
        0x2A => 0xE1, // left Shift
        0x2B => 0x31,
        0x2C..=0x32 => [0x1D, 0x1B, 0x06, 0x19, 0x05, 0x11, 0x10][(scan - 0x2C) as usize], // ZXCVBNM
        0x33 => 0x36,
        0x34 => 0x37,
        0x35 => 0x38,
        0x36 => 0xE5, // right Shift
        0x37 => 0x55, // keypad *
        0x38 => 0xE2, // left Alt
        0x39 => 0x2C, // Space
        0x3A => 0x39, // CapsLock
        0x3B..=0x44 => 0x3A + (scan - 0x3B) as u8,          // F1..F10
        0x46 => 0x47,                                       // ScrollLock
        0x47..=0x53 => [0x5F, 0x60, 0x61, 0x56, 0x5C, 0x5D, 0x5E, 0x57, 0x59, 0x5A, 0x5B, 0x62, 0x63]
            [(scan - 0x47) as usize],                       // keypad 7 8 9 - 4 5 6 + 1 2 3 0 .
        0x54 => 0x46,                                       // SysRq (Alt+PrintScreen)
        0x56 => 0x64,                                       // ISO \|
        0x57 => 0x44,                                       // F11
        0x58 => 0x45,                                       // F12
        0x70 => 0x88,                                       // Katakana/Hiragana
        0x73 => 0x87,                                       // Ro
        0x79 => 0x8A,                                       // Henkan
        0x7B => 0x8B,                                       // Muhenkan
        0x7D => 0x89,                                       // Yen
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::usage;

    #[test]
    fn common_keys() {
        assert_eq!(usage(0x1E, false, 0x41), 0x04); // A
        assert_eq!(usage(0x2C, false, 0x5A), 0x1D); // Z
        assert_eq!(usage(0x1C, false, 0x0D), 0x28); // Enter
        assert_eq!(usage(0x1D, true, 0xA3), 0xE4); // right Ctrl
        assert_eq!(usage(0x1D, false, 0xA2), 0xE0); // left Ctrl
        assert_eq!(usage(0x0B, false, 0x30), 0x27); // 0
        assert_eq!(usage(0x44, false, 0x79), 0x43); // F10
        assert_eq!(usage(0x53, false, 0x6E), 0x63); // keypad .
        assert_eq!(usage(0x48, true, 0x26), 0x52); // Up
        assert_eq!(usage(0x45, false, 0x13), 0x48); // Pause
        assert_eq!(usage(0x45, true, 0x90), 0x53); // NumLock
        assert_eq!(usage(0x2A, true, 0x10), 0); // fake shift around NumLock'd keypad keys
        assert_eq!(usage(0x21D, false, 0xA2), 0); // fake left Ctrl sent with AltGr
    }
}

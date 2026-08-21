//! PS/2 set-1 scan code -> USB HID keyboard usage.
//!
//! The wire protocol carries physical key identity, never characters (spec §14.1), so the
//! receiver can apply its own layout and modifier remap. Scan code is preferred over the virtual
//! key because it is layout independent: `Z` on a German keyboard must land on the same physical
//! key on the Mac.

use crate::protocol::modmask;

pub const HID_NONE: u16 = 0;

/// Non-extended scan codes 0x00..0x7F.
static SET1_TO_HID: [u16; 0x80] = {
    let mut t = [0u16; 0x80];
    t[0x01] = 0x29; // Escape
    t[0x02] = 0x1E; // 1
    t[0x03] = 0x1F;
    t[0x04] = 0x20;
    t[0x05] = 0x21;
    t[0x06] = 0x22;
    t[0x07] = 0x23;
    t[0x08] = 0x24;
    t[0x09] = 0x25;
    t[0x0A] = 0x26; // 9
    t[0x0B] = 0x27; // 0
    t[0x0C] = 0x2D; // -
    t[0x0D] = 0x2E; // =
    t[0x0E] = 0x2A; // Backspace
    t[0x0F] = 0x2B; // Tab
    t[0x10] = 0x14; // Q
    t[0x11] = 0x1A; // W
    t[0x12] = 0x08; // E
    t[0x13] = 0x15; // R
    t[0x14] = 0x17; // T
    t[0x15] = 0x1C; // Y
    t[0x16] = 0x18; // U
    t[0x17] = 0x0C; // I
    t[0x18] = 0x12; // O
    t[0x19] = 0x13; // P
    t[0x1A] = 0x2F; // [
    t[0x1B] = 0x30; // ]
    t[0x1C] = 0x28; // Enter
    t[0x1D] = 0xE0; // Left Control
    t[0x1E] = 0x04; // A
    t[0x1F] = 0x16; // S
    t[0x20] = 0x07; // D
    t[0x21] = 0x09; // F
    t[0x22] = 0x0A; // G
    t[0x23] = 0x0B; // H
    t[0x24] = 0x0D; // J
    t[0x25] = 0x0E; // K
    t[0x26] = 0x0F; // L
    t[0x27] = 0x33; // ;
    t[0x28] = 0x34; // '
    t[0x29] = 0x35; // `
    t[0x2A] = 0xE1; // Left Shift
    t[0x2B] = 0x31; // backslash
    t[0x2C] = 0x1D; // Z
    t[0x2D] = 0x1B; // X
    t[0x2E] = 0x06; // C
    t[0x2F] = 0x19; // V
    t[0x30] = 0x05; // B
    t[0x31] = 0x11; // N
    t[0x32] = 0x10; // M
    t[0x33] = 0x36; // ,
    t[0x34] = 0x37; // .
    t[0x35] = 0x38; // /
    t[0x36] = 0xE5; // Right Shift
    t[0x37] = 0x55; // Keypad *
    t[0x38] = 0xE2; // Left Alt
    t[0x39] = 0x2C; // Space
    t[0x3A] = 0x39; // Caps Lock
    t[0x3B] = 0x3A; // F1
    t[0x3C] = 0x3B;
    t[0x3D] = 0x3C;
    t[0x3E] = 0x3D;
    t[0x3F] = 0x3E;
    t[0x40] = 0x3F;
    t[0x41] = 0x40;
    t[0x42] = 0x41;
    t[0x43] = 0x42;
    t[0x44] = 0x43; // F10
    t[0x45] = 0x53; // Num Lock
    t[0x46] = 0x47; // Scroll Lock
    t[0x47] = 0x5F; // Keypad 7
    t[0x48] = 0x60;
    t[0x49] = 0x61;
    t[0x4A] = 0x56; // Keypad -
    t[0x4B] = 0x5C;
    t[0x4C] = 0x5D;
    t[0x4D] = 0x5E;
    t[0x4E] = 0x57; // Keypad +
    t[0x4F] = 0x59;
    t[0x50] = 0x5A;
    t[0x51] = 0x5B;
    t[0x52] = 0x62; // Keypad 0
    t[0x53] = 0x63; // Keypad .
    t[0x56] = 0x64; // ISO backslash (102nd key)
    t[0x57] = 0x44; // F11
    t[0x58] = 0x45; // F12
    t[0x59] = 0x67; // Keypad =
    t[0x64] = 0x68; // F13
    t[0x65] = 0x69; // F14
    t[0x66] = 0x6A; // F15
    t[0x67] = 0x6B; // F16
    t[0x68] = 0x6C; // F17
    t[0x69] = 0x6D; // F18
    t[0x6A] = 0x6E; // F19
    t[0x6B] = 0x6F; // F20
    t[0x70] = 0x88; // Katakana/Hiragana (JIS)
    t[0x73] = 0x87; // International 1 (JIS _)
    t[0x79] = 0x8A; // Henkan
    t[0x7B] = 0x8B; // Muhenkan
    t[0x7D] = 0x89; // International 3 (JIS yen)
    t
};

/// Extended (`E0`-prefixed) scan codes.
fn extended_to_hid(scan: u16) -> u16 {
    match scan {
        0x1C => 0x58, // Keypad Enter
        0x1D => 0xE4, // Right Control
        0x35 => 0x54, // Keypad /
        0x37 => 0x46, // Print Screen
        0x38 => 0xE6, // Right Alt
        0x46 => 0x48, // Pause (Ctrl+Break)
        0x47 => 0x4A, // Home
        0x48 => 0x52, // Up
        0x49 => 0x4B, // Page Up
        0x4B => 0x50, // Left
        0x4D => 0x4F, // Right
        0x4F => 0x4D, // End
        0x50 => 0x51, // Down
        0x51 => 0x4E, // Page Down
        0x52 => 0x49, // Insert
        0x53 => 0x4C, // Delete
        0x5B => 0xE3, // Left GUI (Windows key)
        0x5C => 0xE7, // Right GUI
        0x5D => 0x65, // Application / Menu
        0x5E => 0x66, // Power
        _ => HID_NONE,
    }
}

/// Last-resort mapping for devices that report a scan code of 0 (some virtual keyboards and
/// KVM switches do). Only covers keys that matter for control flow.
fn vk_to_hid(vk: u16) -> u16 {
    match vk {
        0x08 => 0x2A, // Backspace
        0x09 => 0x2B, // Tab
        0x0D => 0x28, // Enter
        0x13 => 0x48, // Pause
        0x14 => 0x39, // Caps Lock
        0x1B => 0x29, // Escape
        0x20 => 0x2C, // Space
        0x21 => 0x4B,
        0x22 => 0x4E,
        0x23 => 0x4D,
        0x24 => 0x4A,
        0x25 => 0x50, // Left
        0x26 => 0x52, // Up
        0x27 => 0x4F, // Right
        0x28 => 0x51, // Down
        0x2D => 0x49,
        0x2E => 0x4C,
        0x30..=0x39 => {
            if vk == 0x30 {
                0x27
            } else {
                0x1E + (vk - 0x31)
            }
        }
        0x41..=0x5A => {
            // A..Z -> HID usages, which are not in alphabetical order.
            const LETTERS: [u16; 26] = [
                0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10,
                0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D,
            ];
            LETTERS[(vk - 0x41) as usize]
        }
        0x5B => 0xE3,
        0x5C => 0xE7,
        0x5D => 0x65,
        0x70..=0x87 => 0x3A + (vk - 0x70), // F1..F24
        0xA0 => 0xE1,
        0xA1 => 0xE5,
        0xA2 => 0xE0,
        0xA3 => 0xE4,
        0xA4 => 0xE2,
        0xA5 => 0xE6,
        _ => HID_NONE,
    }
}

/// `scan` is `RAWKEYBOARD::MakeCode`, `e0` is the `RI_KEY_E0` flag, `vk` is `RAWKEYBOARD::VKey`.
pub fn hid_usage(scan: u16, e0: bool, vk: u16) -> u16 {
    // Raw Input emits a synthetic shift alongside some extended keys on legacy keyboards; it is
    // not a real key press and must never reach the Mac.
    if e0 && (scan == 0x2A || scan == 0x36) {
        return HID_NONE;
    }
    let usage = if e0 {
        extended_to_hid(scan)
    } else if (scan as usize) < SET1_TO_HID.len() {
        SET1_TO_HID[scan as usize]
    } else {
        HID_NONE
    };
    if usage != HID_NONE {
        usage
    } else {
        vk_to_hid(vk)
    }
}

/// Protocol modifier bit for a HID usage, or 0 for a non-modifier key.
pub fn modifier_bit(hid: u16) -> u16 {
    match hid {
        0xE0 => modmask::L_CTRL,
        0xE1 => modmask::L_SHIFT,
        0xE2 => modmask::L_ALT,
        0xE3 => modmask::L_GUI,
        0xE4 => modmask::R_CTRL,
        0xE5 => modmask::R_SHIFT,
        0xE6 => modmask::R_ALT,
        0xE7 => modmask::R_GUI,
        _ => 0,
    }
}

/// Virtual-key code a HID usage corresponds to, used to match configured hotkeys against the
/// raw-input stream without depending on the hook's view of the world.
pub fn hid_to_vk(hid: u16) -> u16 {
    match hid {
        0x04..=0x1D => {
            const LETTER_VK: [u16; 26] = [
                b'A' as u16, b'B' as u16, b'C' as u16, b'D' as u16, b'E' as u16, b'F' as u16,
                b'G' as u16, b'H' as u16, b'I' as u16, b'J' as u16, b'K' as u16, b'L' as u16,
                b'M' as u16, b'N' as u16, b'O' as u16, b'P' as u16, b'Q' as u16, b'R' as u16,
                b'S' as u16, b'T' as u16, b'U' as u16, b'V' as u16, b'W' as u16, b'X' as u16,
                b'Y' as u16, b'Z' as u16,
            ];
            LETTER_VK[(hid - 0x04) as usize]
        }
        0x1E..=0x26 => 0x31 + (hid - 0x1E), // 1..9
        0x27 => 0x30,                       // 0
        0x28 => 0x0D,                       // Enter
        0x29 => 0x1B,                       // Escape
        0x2A => 0x08,                       // Backspace
        0x2B => 0x09,                       // Tab
        0x2C => 0x20,                       // Space
        0x39 => 0x14,                       // Caps Lock
        0x3A..=0x45 => 0x70 + (hid - 0x3A), // F1..F12
        0x46 => 0x2C,                       // Print Screen
        0x47 => 0x91,                       // Scroll Lock
        0x48 => 0x13,                       // Pause
        0x49 => 0x2D,                       // Insert
        0x4A => 0x24,                       // Home
        0x4B => 0x21,                       // Page Up
        0x4C => 0x2E,                       // Delete
        0x4D => 0x23,                       // End
        0x4E => 0x22,                       // Page Down
        0x4F => 0x27,                       // Right
        0x50 => 0x25,                       // Left
        0x51 => 0x28,                       // Down
        0x52 => 0x26,                       // Up
        0x53 => 0x90,                       // Num Lock
        0x65 => 0x5D,                       // Application
        0x68..=0x6F => 0x7C + (hid - 0x68), // F13..F20
        0xE0 => 0xA2,
        0xE1 => 0xA0,
        0xE2 => 0xA4,
        0xE3 => 0x5B,
        0xE4 => 0xA3,
        0xE5 => 0xA1,
        0xE6 => 0xA5,
        0xE7 => 0x5C,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_arrows_map_correctly() {
        assert_eq!(hid_usage(0x1E, false, b'A' as u16), 0x04); // A
        assert_eq!(hid_usage(0x2C, false, b'Z' as u16), 0x1D); // Z on QWERTY position
        assert_eq!(hid_usage(0x4B, true, 0x25), 0x50); // Left arrow
        assert_eq!(hid_usage(0x1D, false, 0xA2), 0xE0); // Left Ctrl
        assert_eq!(hid_usage(0x1D, true, 0xA3), 0xE4); // Right Ctrl
        assert_eq!(hid_usage(0x5B, true, 0x5B), 0xE3); // Left Windows key
    }

    #[test]
    fn fake_shift_is_dropped() {
        assert_eq!(hid_usage(0x2A, true, 0xA0), HID_NONE);
        assert_eq!(hid_usage(0x36, true, 0xA1), HID_NONE);
    }

    #[test]
    fn falls_back_to_virtual_key_when_scan_code_is_missing() {
        assert_eq!(hid_usage(0, false, 0x25), 0x50); // Left
        assert_eq!(hid_usage(0, false, b'Q' as u16), 0x14);
    }

    #[test]
    fn hid_to_vk_round_trips_hotkey_relevant_keys() {
        for vk in [0x25u16, 0x26, 0x27, 0x28, 0x1B, b'A' as u16, b'Q' as u16, 0x70, 0x7B] {
            let hid = vk_to_hid(vk);
            assert_ne!(hid, HID_NONE, "vk {vk:#x} should map to a usage");
            assert_eq!(hid_to_vk(hid), vk, "round trip failed for vk {vk:#x}");
        }
    }

    #[test]
    fn modifier_bits_cover_all_eight_modifiers() {
        let bits: Vec<u16> = (0xE0..=0xE7).map(modifier_bit).collect();
        assert!(bits.iter().all(|b| *b != 0));
        assert_eq!(bits.iter().fold(0, |a, b| a | b), 0x00FF);
    }
}

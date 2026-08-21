import CoreGraphics

/// USB HID keyboard usage -> macOS virtual key code.
///
/// The sender transmits physical key identity, so this table is the only place layout matters:
/// HID usage 0x14 is "the key at the QWERTY Q position", and macOS virtual key 12 is the same
/// physical key, whatever character the active layout produces.
enum Keymap {
    /// Which physical modifier a usage represents, before the user's remap is applied.
    enum PhysicalModifier {
        case control(right: Bool)
        case shift(right: Bool)
        case alt(right: Bool)
        case gui(right: Bool)
    }

    static func physicalModifier(for usage: UInt16) -> PhysicalModifier? {
        switch usage {
        case 0xE0: return .control(right: false)
        case 0xE1: return .shift(right: false)
        case 0xE2: return .alt(right: false)
        case 0xE3: return .gui(right: false)
        case 0xE4: return .control(right: true)
        case 0xE5: return .shift(right: true)
        case 0xE6: return .alt(right: true)
        case 0xE7: return .gui(right: true)
        default: return nil
        }
    }

    static func virtualKey(for usage: UInt16) -> CGKeyCode? {
        table[usage]
    }

    private static let table: [UInt16: CGKeyCode] = [
        // Letters
        0x04: 0, 0x05: 11, 0x06: 8, 0x07: 2, 0x08: 14, 0x09: 3, 0x0A: 5, 0x0B: 4,
        0x0C: 34, 0x0D: 38, 0x0E: 40, 0x0F: 37, 0x10: 46, 0x11: 45, 0x12: 31, 0x13: 35,
        0x14: 12, 0x15: 15, 0x16: 1, 0x17: 17, 0x18: 32, 0x19: 9, 0x1A: 13, 0x1B: 7,
        0x1C: 16, 0x1D: 6,
        // Digits
        0x1E: 18, 0x1F: 19, 0x20: 20, 0x21: 21, 0x22: 23, 0x23: 22, 0x24: 26, 0x25: 28,
        0x26: 25, 0x27: 29,
        // Control and punctuation
        0x28: 36, // Return
        0x29: 53, // Escape
        0x2A: 51, // Backspace -> Delete
        0x2B: 48, // Tab
        0x2C: 49, // Space
        0x2D: 27, // -
        0x2E: 24, // =
        0x2F: 33, // [
        0x30: 30, // ]
        0x31: 42, // backslash
        0x32: 42, // non-US #, same physical position on ISO boards
        0x33: 41, // ;
        0x34: 39, // '
        0x35: 50, // `
        0x36: 43, // ,
        0x37: 47, // .
        0x38: 44, // /
        0x39: 57, // Caps Lock
        // Function keys
        0x3A: 122, 0x3B: 120, 0x3C: 99, 0x3D: 118, 0x3E: 96, 0x3F: 97,
        0x40: 98, 0x41: 100, 0x42: 101, 0x43: 109, 0x44: 103, 0x45: 111,
        // Print Screen / Scroll Lock / Pause have no Mac equivalent; F13-F15 occupy the same
        // physical positions on an Apple extended keyboard, which is the least surprising choice.
        0x46: 105, 0x47: 107, 0x48: 113,
        0x49: 114, // Insert -> Help
        0x4A: 115, // Home
        0x4B: 116, // Page Up
        0x4C: 117, // Delete forward
        0x4D: 119, // End
        0x4E: 121, // Page Down
        0x4F: 124, // Right
        0x50: 123, // Left
        0x51: 125, // Down
        0x52: 126, // Up
        // Keypad
        0x53: 71, // Num Lock -> Clear
        0x54: 75, 0x55: 67, 0x56: 78, 0x57: 69, 0x58: 76,
        0x59: 83, 0x5A: 84, 0x5B: 85, 0x5C: 86, 0x5D: 87, 0x5E: 88, 0x5F: 89,
        0x60: 91, 0x61: 92, 0x62: 82, 0x63: 65,
        0x64: 10, // ISO extra backslash (102nd key) -> Section
        0x67: 81, // Keypad =
        // F13..F20
        0x68: 105, 0x69: 107, 0x6A: 113, 0x6B: 106, 0x6C: 64, 0x6D: 79, 0x6E: 80, 0x6F: 90,
        // JIS
        0x87: 94, 0x88: 104, 0x89: 93, 0x8A: 104, 0x8B: 102,
    ]
}

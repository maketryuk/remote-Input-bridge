import Foundation

/// Wire protocol v1 - see docs/PROTOCOL.md. Kept byte-for-byte in step with the Rust sender.
enum Proto {
    static let version: UInt32 = 1
    static let udpMagic: UInt16 = 0x5249
    static let udpVersion: UInt8 = 1
    static let udpTypeMouseMove: UInt8 = 1
    static let udpPacketLength = 44
    static let maxFrameLength = 65536
    static let tagLength = 16

    static let capabilities = [
        "mouse_move_udp", "mouse_buttons", "scroll_hires", "keyboard", "heartbeat", "edge_switch",
    ]

    enum MessageType: UInt8 {
        case sessionStart = 0x01
        case ping = 0x02
        case pong = 0x03
        case targetActive = 0x04
        case mouseButton = 0x05
        case scroll = 0x06
        case key = 0x07
        case modifierSync = 0x08
        case releaseAll = 0x09
        case mouseMoveRelative = 0x0A
        case bye = 0x0B
        case edgeHit = 0x0C
    }

    /// Physical mouse buttons as numbered by the sender.
    enum Button: UInt8 {
        case left = 0, right = 1, middle = 2, back = 3, forward = 4
    }

    /// Left/right aware physical modifier bits.
    struct ModifierMask: OptionSet {
        let rawValue: UInt16
        static let leftControl = ModifierMask(rawValue: 1 << 0)
        static let leftShift = ModifierMask(rawValue: 1 << 1)
        static let leftAlt = ModifierMask(rawValue: 1 << 2)
        static let leftGUI = ModifierMask(rawValue: 1 << 3)
        static let rightControl = ModifierMask(rawValue: 1 << 4)
        static let rightShift = ModifierMask(rawValue: 1 << 5)
        static let rightAlt = ModifierMask(rawValue: 1 << 6)
        static let rightGUI = ModifierMask(rawValue: 1 << 7)
    }

    /// Decoded reliable message.
    enum Message {
        case sessionStart(mouseIntervalMicros: UInt32, flags: UInt8)
        case ping(sentMicros: UInt64)
        case pong(sentMicros: UInt64, appliedEvents: UInt64, udpReceived: UInt32, udpDropped: UInt32)
        case targetActive(Bool)
        case mouseButton(button: UInt8, down: Bool)
        case scroll(unitsX: Int32, unitsY: Int32)
        case key(hidUsage: UInt16, down: Bool, repeatPress: Bool)
        case modifierSync(ModifierMask)
        case releaseAll
        case mouseMoveRelative(dx: Int32, dy: Int32)
        case bye(reason: UInt8)
        case edgeHit(edge: UInt8)

        var type: MessageType {
            switch self {
            case .sessionStart: return .sessionStart
            case .ping: return .ping
            case .pong: return .pong
            case .targetActive: return .targetActive
            case .mouseButton: return .mouseButton
            case .scroll: return .scroll
            case .key: return .key
            case .modifierSync: return .modifierSync
            case .releaseAll: return .releaseAll
            case .mouseMoveRelative: return .mouseMoveRelative
            case .bye: return .bye
            case .edgeHit: return .edgeHit
            }
        }

        func encodeBody() -> Data {
            var body = Data()
            switch self {
            case let .sessionStart(interval, flags):
                body.append(bigEndian: interval)
                body.append(flags)
            case let .ping(sent):
                body.append(bigEndian: sent)
            case let .pong(sent, applied, received, dropped):
                body.append(bigEndian: sent)
                body.append(bigEndian: applied)
                body.append(bigEndian: received)
                body.append(bigEndian: dropped)
            case let .targetActive(active):
                body.append(active ? 1 : 0)
            case let .mouseButton(button, down):
                body.append(button)
                body.append(down ? 1 : 0)
            case let .scroll(x, y):
                body.append(bigEndian: x)
                body.append(bigEndian: y)
            case let .key(usage, down, repeatPress):
                body.append(bigEndian: usage)
                body.append(down ? 1 : 0)
                body.append(repeatPress ? 1 : 0)
            case let .modifierSync(mask):
                body.append(bigEndian: mask.rawValue)
            case .releaseAll:
                break
            case let .mouseMoveRelative(dx, dy):
                body.append(bigEndian: dx)
                body.append(bigEndian: dy)
            case let .bye(reason):
                body.append(reason)
            case let .edgeHit(edge):
                body.append(edge)
            }
            return body
        }

        static func decode(type: UInt8, body: Data) -> Message? {
            guard let kind = MessageType(rawValue: type) else { return nil }
            switch kind {
            case .sessionStart:
                guard let interval: UInt32 = body.readBigEndian(UInt32.self, at: 0),
                      body.count >= 5 else { return nil }
                return .sessionStart(mouseIntervalMicros: interval, flags: body[body.startIndex + 4])
            case .ping:
                guard let sent: UInt64 = body.readBigEndian(UInt64.self, at: 0) else { return nil }
                return .ping(sentMicros: sent)
            case .pong:
                guard let sent: UInt64 = body.readBigEndian(UInt64.self, at: 0),
                      let applied: UInt64 = body.readBigEndian(UInt64.self, at: 8),
                      let received: UInt32 = body.readBigEndian(UInt32.self, at: 16),
                      let dropped: UInt32 = body.readBigEndian(UInt32.self, at: 20) else { return nil }
                return .pong(sentMicros: sent, appliedEvents: applied, udpReceived: received, udpDropped: dropped)
            case .targetActive:
                guard body.count >= 1 else { return nil }
                return .targetActive(body[body.startIndex] != 0)
            case .mouseButton:
                guard body.count >= 2 else { return nil }
                return .mouseButton(button: body[body.startIndex], down: body[body.startIndex + 1] != 0)
            case .scroll:
                guard let x: Int32 = body.readBigEndian(Int32.self, at: 0),
                      let y: Int32 = body.readBigEndian(Int32.self, at: 4) else { return nil }
                return .scroll(unitsX: x, unitsY: y)
            case .key:
                guard body.count >= 4, let usage: UInt16 = body.readBigEndian(UInt16.self, at: 0) else { return nil }
                return .key(
                    hidUsage: usage,
                    down: body[body.startIndex + 2] != 0,
                    repeatPress: body[body.startIndex + 3] != 0
                )
            case .modifierSync:
                guard let mask: UInt16 = body.readBigEndian(UInt16.self, at: 0) else { return nil }
                return .modifierSync(ModifierMask(rawValue: mask))
            case .releaseAll:
                return .releaseAll
            case .mouseMoveRelative:
                guard let dx: Int32 = body.readBigEndian(Int32.self, at: 0),
                      let dy: Int32 = body.readBigEndian(Int32.self, at: 4) else { return nil }
                return .mouseMoveRelative(dx: dx, dy: dy)
            case .bye:
                return .bye(reason: body.isEmpty ? 0 : body[body.startIndex])
            case .edgeHit:
                guard body.count >= 1 else { return nil }
                return .edgeHit(edge: body[body.startIndex])
            }
        }
    }

    /// Realtime mouse packet: cumulative totals plus a truncated HMAC over the header.
    struct MouseMovePacket {
        var sessionID: UInt64
        var sequence: UInt64
        var timestampMicros: UInt64
        var totalX: Int32
        var totalY: Int32

        /// Returns nil for anything that is not a well-formed, correctly signed packet.
        static func parse(_ data: Data, udpKey: Data) -> MouseMovePacket? {
            guard data.count == udpPacketLength,
                  let magic: UInt16 = data.readBigEndian(UInt16.self, at: 0), magic == udpMagic,
                  data[data.startIndex + 2] == udpVersion,
                  data[data.startIndex + 3] == udpTypeMouseMove,
                  let sessionID: UInt64 = data.readBigEndian(UInt64.self, at: 4),
                  let sequence: UInt64 = data.readBigEndian(UInt64.self, at: 12),
                  let timestamp: UInt64 = data.readBigEndian(UInt64.self, at: 20),
                  let totalX: Int32 = data.readBigEndian(Int32.self, at: 28),
                  let totalY: Int32 = data.readBigEndian(Int32.self, at: 32)
            else { return nil }
            let signed = data.subdata(in: data.startIndex..<(data.startIndex + 36))
            let tag = data.subdata(in: (data.startIndex + 36)..<(data.startIndex + 44))
            let expected = Crypto.hmac(key: udpKey, data: signed).prefix(8)
            guard Crypto.constantTimeEquals(Data(expected), tag) else { return nil }
            return MouseMovePacket(
                sessionID: sessionID,
                sequence: sequence,
                timestampMicros: timestamp,
                totalX: totalX,
                totalY: totalY
            )
        }
    }
}

/// Wrapped delta between two cumulative totals: survives Int32 overflow after a very long session.
func wrappingDelta(_ current: Int32, _ previous: Int32) -> Int32 {
    Int32(bitPattern: UInt32(bitPattern: current) &- UInt32(bitPattern: previous))
}

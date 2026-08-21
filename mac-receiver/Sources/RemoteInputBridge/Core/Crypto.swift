import CryptoKit
import Foundation

/// Mirror of the sender's `crypto` module. Both sides must derive byte-identical keys, so the
/// salts, infos and concatenation order here are part of the wire protocol.
enum Crypto {
    static let pairSalt = Data("rib-pair-v1".utf8)
    static let pairInfo = Data("pairing".utf8)
    static let wrapInfo = Data("wrap".utf8)
    static let sessionInfoTCP = Data("rib-session-v1|tcp".utf8)
    static let sessionInfoUDP = Data("rib-session-v1|udp".utf8)

    static func hmac(key: Data, data: Data) -> Data {
        Data(HMAC<SHA256>.authenticationCode(for: data, using: SymmetricKey(data: key)))
    }

    static func hkdf32(ikm: Data, salt: Data, info: Data) -> Data {
        let derived = HKDF<SHA256>.deriveKey(
            inputKeyMaterial: SymmetricKey(data: ikm),
            salt: salt,
            info: info,
            outputByteCount: 32
        )
        return derived.withUnsafeBytes { Data($0) }
    }

    /// Case folded, separators stripped: `abcd-efgh` and `ABCD EFGH` pair identically.
    static func normalizePairingCode(_ code: String) -> String {
        String(code.unicodeScalars.filter { CharacterSet.alphanumerics.contains($0) })
            .uppercased()
    }

    static func pairingKey(code: String) -> Data {
        hkdf32(ikm: Data(normalizePairingCode(code).utf8), salt: pairSalt, info: pairInfo)
    }

    static func wrapMask(pairingKey: Data) -> Data {
        hkdf32(ikm: pairingKey, salt: pairSalt, info: wrapInfo)
    }

    static func sessionKeys(deviceKey: Data, clientNonce: Data, serverNonce: Data) -> (tcp: Data, udp: Data) {
        let salt = clientNonce + serverNonce
        return (
            hkdf32(ikm: deviceKey, salt: salt, info: sessionInfoTCP),
            hkdf32(ikm: deviceKey, salt: salt, info: sessionInfoUDP)
        )
    }

    static func pairProof(pairingKey: Data, clientNonce: Data, serverNonce: Data) -> Data {
        hmac(key: pairingKey, data: Data("pair".utf8) + clientNonce + serverNonce)
    }

    static func wrapTag(pairingKey: Data, clientNonce: Data, serverNonce: Data, wrapped: Data) -> Data {
        hmac(key: pairingKey, data: Data("wrap".utf8) + clientNonce + serverNonce + wrapped)
    }

    static func authProof(deviceKey: Data, clientNonce: Data, serverNonce: Data) -> Data {
        hmac(key: deviceKey, data: Data("auth".utf8) + clientNonce + serverNonce)
    }

    static func authAckProof(deviceKey: Data, clientNonce: Data, serverNonce: Data, sessionID: UInt64) -> Data {
        var data = Data("auth-ack".utf8) + clientNonce + serverNonce
        data.append(bigEndian: sessionID)
        return hmac(key: deviceKey, data: data)
    }

    /// Constant-time comparison; input tags come from the network.
    static func constantTimeEquals(_ a: Data, _ b: Data) -> Bool {
        guard a.count == b.count else { return false }
        var difference: UInt8 = 0
        for (x, y) in zip(a, b) { difference |= x ^ y }
        return difference == 0
    }

    static func randomData(_ count: Int) -> Data {
        var bytes = [UInt8](repeating: 0, count: count)
        let status = SecRandomCopyBytes(kSecRandomDefault, count, &bytes)
        precondition(status == errSecSuccess, "the system entropy source failed")
        return Data(bytes)
    }

    static func xorBytes(_ a: Data, _ b: Data) -> Data {
        Data(zip(a, b).map { $0 ^ $1 })
    }

    /// Excludes 0/O/1/I/L/U so a code read off a screen cannot be mistyped into a different code.
    private static let codeAlphabet = Array("23456789ABCDEFGHJKMNPQRSTVWXYZ")

    static func generatePairingCode() -> String {
        let bytes = randomData(8)
        let characters = bytes.map { codeAlphabet[Int($0) % codeAlphabet.count] }
        return String(characters[0..<4]) + "-" + String(characters[4..<8])
    }
}

extension Data {
    var hexString: String {
        map { String(format: "%02x", $0) }.joined()
    }

    init?(hexString: String) {
        let characters = Array(hexString.utf8)
        guard characters.count % 2 == 0 else { return nil }
        var bytes = [UInt8]()
        bytes.reserveCapacity(characters.count / 2)
        func nibble(_ c: UInt8) -> UInt8? {
            switch c {
            case 0x30...0x39: return c - 0x30
            case 0x61...0x66: return c - 0x61 + 10
            case 0x41...0x46: return c - 0x41 + 10
            default: return nil
            }
        }
        for index in stride(from: 0, to: characters.count, by: 2) {
            guard let high = nibble(characters[index]), let low = nibble(characters[index + 1]) else {
                return nil
            }
            bytes.append(high << 4 | low)
        }
        self = Data(bytes)
    }

    mutating func append<T: FixedWidthInteger>(bigEndian value: T) {
        for shift in stride(from: (MemoryLayout<T>.size - 1) * 8, through: 0, by: -8) {
            append(UInt8(truncatingIfNeeded: value >> T(shift)))
        }
    }

    /// Byte-by-byte so it works on `Data` slices with a non-zero `startIndex` and never traps
    /// on a signed value whose sign bit is set.
    func readBigEndian<T: FixedWidthInteger>(_ type: T.Type, at offset: Int) -> T? {
        let size = MemoryLayout<T>.size
        guard offset >= 0, offset + size <= count else { return nil }
        var value: T = 0
        for index in 0..<size {
            value = (value << 8) | T(truncatingIfNeeded: self[startIndex + offset + index])
        }
        return value
    }
}

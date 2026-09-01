import Foundation
import SystemConfiguration

/// Answers "is there a Remote Input Bridge on this network?" so the Windows side never has to be
/// told an address that a new DHCP lease will invalidate.
///
/// A plain BSD socket rather than `NWListener`: this has to receive broadcast datagrams, which is
/// exactly the case Network.framework is least clear about, and a UDP socket bound to the wildcard
/// address has no such ambiguity.
///
/// What it gives away: a name, a `.local` name and two port numbers, to anyone already on the
/// network - who could have found the ports with a scan. It grants nothing: pairing still needs the
/// code shown on this Mac.
final class DiscoveryResponder {
    /// Its own port, so a stray probe can never be mistaken for movement data.
    static let defaultPort: UInt16 = 47823

    /// `RIBD`, big endian.
    private static let magic: UInt32 = 0x5249_4244
    private static let version: UInt8 = 1
    /// Probes are padded to this. A short question with a long answer is a reflection amplifier -
    /// spoof the source address and every responder helps flood someone else - so anything shorter
    /// is ignored, which makes answering useless for that.
    private static let minimumProbeBytes = 256

    private var socketHandle: Int32 = -1
    private var source: DispatchSourceRead?
    private let queue = DispatchQueue(label: "studio.lince.rib.discovery")
    private var describe: () -> (name: String, tcp: UInt16, udp: UInt16) = { ("Mac", 47821, 47822) }

    /// The advertised description is read at answer time, so renaming this Mac or changing its
    /// ports takes effect without a restart.
    func start(port: UInt16 = defaultPort, describe: @escaping () -> (name: String, tcp: UInt16, udp: UInt16)) {
        stop()
        self.describe = describe

        let handle = socket(AF_INET, SOCK_DGRAM, 0)
        guard handle >= 0 else {
            Log.warn("discovery: cannot open a socket (\(errno))")
            return
        }
        var yes: Int32 = 1
        setsockopt(handle, SOL_SOCKET, SO_REUSEADDR, &yes, socklen_t(MemoryLayout<Int32>.size))
        setsockopt(handle, SOL_SOCKET, SO_REUSEPORT, &yes, socklen_t(MemoryLayout<Int32>.size))

        var address = sockaddr_in()
        address.sin_family = sa_family_t(AF_INET)
        address.sin_port = port.bigEndian
        address.sin_addr.s_addr = INADDR_ANY
        let bound = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(handle, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        guard bound == 0 else {
            Log.warn("discovery: cannot bind port \(port) (\(errno)); the Windows side will have to be given an address by hand")
            close(handle)
            return
        }

        socketHandle = handle
        let source = DispatchSource.makeReadSource(fileDescriptor: handle, queue: queue)
        source.setEventHandler { [weak self] in self?.readOne() }
        source.setCancelHandler { close(handle) }
        source.resume()
        self.source = source
        Log.info("discovery: answering probes on UDP \(port)")
    }

    func stop() {
        source?.cancel()
        source = nil
        socketHandle = -1
    }

    private func readOne() {
        var buffer = [UInt8](repeating: 0, count: 1024)
        var from = sockaddr_storage()
        var fromLength = socklen_t(MemoryLayout<sockaddr_storage>.size)
        let read = withUnsafeMutablePointer(to: &from) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { address in
                recvfrom(socketHandle, &buffer, buffer.count, 0, address, &fromLength)
            }
        }
        guard read >= Self.minimumProbeBytes else { return }
        guard buffer[0] == UInt8((Self.magic >> 24) & 0xFF),
              buffer[1] == UInt8((Self.magic >> 16) & 0xFF),
              buffer[2] == UInt8((Self.magic >> 8) & 0xFF),
              buffer[3] == UInt8(Self.magic & 0xFF),
              buffer[4] == Self.version
        else { return }

        guard let reply = makeReply() else { return }
        _ = reply.withUnsafeBytes { bytes in
            withUnsafePointer(to: &from) { pointer in
                pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { address in
                    sendto(socketHandle, bytes.baseAddress, bytes.count, 0, address, fromLength)
                }
            }
        }
    }

    private func makeReply() -> Data? {
        let described = describe()
        // Truncated so the answer can never grow past the probe, however this Mac is named.
        let name = String(described.name.prefix(64))
        let host = Self.localHostName().map { String($0.prefix(64)) + ".local" } ?? ""
        let payload: [String: Any] = [
            "name": name,
            "host": host,
            "tcp": Int(described.tcp),
            "udp": Int(described.udp),
            "version": Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "",
        ]
        guard let json = try? JSONSerialization.data(withJSONObject: payload) else { return nil }
        var reply = Data([
            UInt8((Self.magic >> 24) & 0xFF),
            UInt8((Self.magic >> 16) & 0xFF),
            UInt8((Self.magic >> 8) & 0xFF),
            UInt8(Self.magic & 0xFF),
            Self.version,
        ])
        reply.append(json)
        guard reply.count <= Self.minimumProbeBytes else {
            Log.warn("discovery: the reply is longer than the probe; not answering")
            return nil
        }
        return reply
    }

    /// The name this Mac answers to over multicast DNS - `scutil --get LocalHostName`. It is worth
    /// more than the address to the other side, because it survives a new lease.
    private static func localHostName() -> String? {
        guard let name = SCDynamicStoreCopyLocalHostName(nil) as String? , !name.isEmpty else {
            return nil
        }
        return name
    }
}

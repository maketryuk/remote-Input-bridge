import Foundation

/// Receiver-side counters. Reported back to Windows in every PONG so the sender can show real
/// packet loss instead of guessing (spec §38, §39).
final class Telemetry {
    private let lock = NSLock()

    private(set) var udpReceived: UInt32 = 0
    private(set) var udpRejected: UInt32 = 0
    private(set) var udpOutOfOrder: UInt32 = 0
    /// Datagrams the sender clearly sent but we never saw, inferred from sequence gaps.
    private(set) var udpMissing: UInt32 = 0
    private(set) var appliedEvents: UInt64 = 0
    private(set) var coalescedEvents: UInt64 = 0
    private(set) var reliableEvents: UInt64 = 0

    func countReceived(sequenceGap: UInt64) {
        lock.lock()
        udpReceived &+= 1
        if sequenceGap > 1 { udpMissing &+= UInt32(min(sequenceGap - 1, 1_000_000)) }
        lock.unlock()
    }

    func countRejected() { lock.lock(); udpRejected &+= 1; lock.unlock() }
    func countOutOfOrder() { lock.lock(); udpOutOfOrder &+= 1; lock.unlock() }
    func countApplied() { lock.lock(); appliedEvents &+= 1; lock.unlock() }
    func countCoalesced() { lock.lock(); coalescedEvents &+= 1; lock.unlock() }
    func countReliable() { lock.lock(); reliableEvents &+= 1; lock.unlock() }

    struct Snapshot {
        var udpReceived: UInt32 = 0
        var udpRejected: UInt32 = 0
        var udpOutOfOrder: UInt32 = 0
        var udpMissing: UInt32 = 0
        var appliedEvents: UInt64 = 0
        var coalescedEvents: UInt64 = 0
        var reliableEvents: UInt64 = 0
    }

    var snapshot: Snapshot {
        lock.lock(); defer { lock.unlock() }
        return Snapshot(
            udpReceived: udpReceived,
            udpRejected: udpRejected,
            udpOutOfOrder: udpOutOfOrder,
            udpMissing: udpMissing,
            appliedEvents: appliedEvents,
            coalescedEvents: coalescedEvents,
            reliableEvents: reliableEvents
        )
    }

    func resetSession() {
        lock.lock()
        udpReceived = 0
        udpRejected = 0
        udpOutOfOrder = 0
        udpMissing = 0
        appliedEvents = 0
        coalescedEvents = 0
        reliableEvents = 0
        lock.unlock()
    }
}

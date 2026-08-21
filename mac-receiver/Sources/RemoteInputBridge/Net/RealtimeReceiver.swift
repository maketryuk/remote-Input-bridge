import Foundation
import Network

/// UDP movement channel (spec §22, §30).
///
/// Every datagram is authenticated and carries the *cumulative* pointer total, which is what
/// makes packet loss cost a single frame of smoothness instead of a permanent cursor offset: the
/// next datagram to arrive re-establishes the truth on its own.
final class RealtimeReceiver {
    private let queue = DispatchQueue(label: "studio.rib.realtime", qos: .userInteractive)
    private let scheduler: EventScheduler
    private let telemetry: Telemetry

    private var listener: NWListener?
    /// Retained so ARC does not release the flow the datagrams arrive on.
    private var flows: [ObjectIdentifier: NWConnection] = [:]

    private var sessionID: UInt64 = 0
    private var udpKey = Data()
    private var active = false
    private var needsBaseline = true
    private var lastSequence: UInt64 = 0
    private var lastTotalX: Int32 = 0
    private var lastTotalY: Int32 = 0
    private var port: UInt16 = 47822

    private(set) var isListening = false
    private(set) var lastError: String?

    /// Read from the control queue when deciding whether to apply an input event.
    private let stateLock = NSLock()
    private var activeFlag = false

    var isActive: Bool {
        stateLock.lock(); defer { stateLock.unlock() }
        return activeFlag
    }

    var currentPort: UInt16 {
        stateLock.lock(); defer { stateLock.unlock() }
        return port
    }

    init(scheduler: EventScheduler, telemetry: Telemetry) {
        self.scheduler = scheduler
        self.telemetry = telemetry
    }

    func start(port: UInt16) {
        queue.async { [self] in
            stopLocked()
            stateLock.lock()
            self.port = port
            stateLock.unlock()
            guard let nwPort = NWEndpoint.Port(rawValue: port) else {
                lastError = "invalid UDP port \(port)"
                return
            }
            let parameters = NWParameters.udp
            parameters.allowLocalEndpointReuse = true
            do {
                let listener = try NWListener(using: parameters, on: nwPort)
                listener.newConnectionHandler = { [weak self] flow in
                    self?.accept(flow)
                }
                listener.stateUpdateHandler = { [weak self] state in
                    guard let self else { return }
                    switch state {
                    case .ready:
                        isListening = true
                        lastError = nil
                        Log.info("realtime channel listening on UDP \(port)")
                    case let .failed(error):
                        isListening = false
                        lastError = "UDP listener failed: \(error)"
                        Log.error(lastError!)
                    case .cancelled:
                        isListening = false
                    default:
                        break
                    }
                }
                listener.start(queue: queue)
                self.listener = listener
            } catch {
                lastError = "cannot listen on UDP \(port): \(error)"
                Log.error(lastError!)
            }
        }
    }

    func stop() {
        queue.async { [self] in stopLocked() }
    }

    private func stopLocked() {
        for flow in flows.values { flow.cancel() }
        flows.removeAll()
        listener?.cancel()
        listener = nil
        isListening = false
    }

    func beginSession(id: UInt64, key: Data) {
        queue.async { [self] in
            sessionID = id
            udpKey = key
            lastSequence = 0
            needsBaseline = true
        }
    }

    func endSession() {
        stateLock.lock()
        activeFlag = false
        stateLock.unlock()
        queue.async { [self] in
            sessionID = 0
            udpKey = Data()
            active = false
            needsBaseline = true
        }
    }

    /// Called when the sender hands the input over or takes it back. Re-baselining here is what
    /// stops the Mac cursor from jumping by everything that moved while Windows had the input.
    func setActive(_ value: Bool) {
        stateLock.lock()
        activeFlag = value
        stateLock.unlock()
        queue.async { [self] in
            active = value
            needsBaseline = true
        }
    }

    private func accept(_ flow: NWConnection) {
        flows[ObjectIdentifier(flow)] = flow
        flow.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            switch state {
            case .ready:
                receive(on: flow)
            case .failed, .cancelled:
                flows.removeValue(forKey: ObjectIdentifier(flow))
            default:
                break
            }
        }
        flow.start(queue: queue)
    }

    private func receive(on flow: NWConnection) {
        flow.receiveMessage { [weak self] data, _, _, error in
            guard let self else { return }
            if let data { handle(data) }
            if error == nil {
                receive(on: flow)
            } else {
                flows.removeValue(forKey: ObjectIdentifier(flow))
            }
        }
    }

    private func handle(_ data: Data) {
        guard sessionID != 0, !udpKey.isEmpty else {
            telemetry.countRejected()
            return
        }
        guard let packet = Proto.MouseMovePacket.parse(data, udpKey: udpKey) else {
            // Wrong magic, wrong length or a forged tag: never reaches the cursor.
            telemetry.countRejected()
            return
        }
        guard packet.sessionID == sessionID else {
            telemetry.countRejected()
            return
        }
        guard packet.sequence > lastSequence else {
            // Out of order or duplicate. Cumulative payloads make this cheap to discard: the
            // newest packet always carries the whole truth.
            telemetry.countOutOfOrder()
            return
        }
        let gap = packet.sequence - lastSequence
        lastSequence = packet.sequence
        telemetry.countReceived(sequenceGap: gap)

        if needsBaseline {
            needsBaseline = false
            lastTotalX = packet.totalX
            lastTotalY = packet.totalY
            return
        }
        let dx = wrappingDelta(packet.totalX, lastTotalX)
        let dy = wrappingDelta(packet.totalY, lastTotalY)
        lastTotalX = packet.totalX
        lastTotalY = packet.totalY

        guard active else { return }
        if Log.shared.enabled(.trace) {
            Log.trace("mouse seq=\(packet.sequence) delta=(\(dx),\(dy)) gap=\(gap)")
        }
        scheduler.submit(dx: dx, dy: dy)
    }
}

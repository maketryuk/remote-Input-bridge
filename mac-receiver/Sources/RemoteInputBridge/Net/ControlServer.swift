import Foundation
import Network

struct SessionInfo {
    var sessionID: UInt64
    var udpKey: Data
    var clientName: String
    var clientID: String
}

protocol ControlServerDelegate: AnyObject {
    func controlServer(_ server: ControlServer, didStart session: SessionInfo)
    func controlServer(_ server: ControlServer, didEnd reason: String)
    func controlServer(_ server: ControlServer, didReceive message: Proto.Message)
    func controlServerTelemetry(_ server: ControlServer) -> Telemetry.Snapshot
    func controlServerDidChangeStatus(_ server: ControlServer)
}

/// TCP control channel: handshake, pairing, authentication, reliable input, heartbeat.
///
/// Everything runs on one serial queue, so the per-connection state machine needs no locks. The
/// queue is `userInitiated` rather than `userInteractive`: keystrokes and clicks are latency
/// sensitive but arrive at human rates, and leaving the top priority band to the movement path
/// keeps the two from competing.
final class ControlServer {
    weak var delegate: ControlServerDelegate?

    private let queue = DispatchQueue(label: "studio.rib.control", qos: .userInitiated)
    private var listener: NWListener?
    private var watchdog: DispatchSourceTimer?
    private var config: Config
    private var keyStore: KeyStore

    private final class Peer {
        enum Phase { case hello, pairOrAuth, authenticated, closed }
        let nw: NWConnection
        var phase: Phase = .hello
        var clientNonce = Data()
        var serverNonce = Data()
        var clientID = ""
        var clientName = ""
        var deviceKey: Data?
        var tcpKey = Data()
        var udpKey = Data()
        var sessionID: UInt64 = 0
        var sendCounter: UInt64 = 0
        var receiveCounter: UInt64 = 0
        var lastFrame = Date()
        init(_ nw: NWConnection) { self.nw = nw }
    }

    private var peers: [ObjectIdentifier: Peer] = [:]
    private var session: Peer?
    private var pairingCodeValue: String?
    private var pairingExpiry = Date.distantPast

    // Status published to the UI.
    private(set) var isListening = false
    private(set) var connectedClientName: String?
    private(set) var connectedClientAddress: String?
    private(set) var lastError: String?

    init(config: Config, keyStore: KeyStore) {
        self.config = config
        self.keyStore = keyStore
    }

    var pairingCode: String? {
        queue.sync { pairingExpiry > Date() ? pairingCodeValue : nil }
    }

    // MARK: - Lifecycle

    func start() {
        queue.async { [self] in
            stopLocked(reason: "restarting")
            let parameters = NWParameters.tcp
            parameters.allowLocalEndpointReuse = true
            // Nagle would turn a keystroke into a 40 ms wait.
            if let tcp = parameters.defaultProtocolStack.internetProtocol as? NWProtocolTCP.Options {
                tcp.noDelay = true
                tcp.enableKeepalive = true
                tcp.keepaliveIdle = 2
            }
            guard let port = NWEndpoint.Port(rawValue: config.tcpPort) else {
                lastError = "invalid TCP port \(config.tcpPort)"
                return
            }
            do {
                let listener = try NWListener(using: parameters, on: port)
                listener.newConnectionHandler = { [weak self] connection in
                    self?.accept(connection)
                }
                listener.stateUpdateHandler = { [weak self] state in
                    guard let self else { return }
                    switch state {
                    case .ready:
                        isListening = true
                        lastError = nil
                        Log.info("control channel listening on TCP \(config.tcpPort)")
                    case let .failed(error):
                        isListening = false
                        lastError = "TCP listener failed: \(error)"
                        Log.error(lastError!)
                    case .cancelled:
                        isListening = false
                    default:
                        break
                    }
                    delegate?.controlServerDidChangeStatus(self)
                }
                listener.start(queue: queue)
                self.listener = listener
                startWatchdogLocked()
            } catch {
                lastError = "cannot listen on TCP \(config.tcpPort): \(error)"
                Log.error(lastError!)
                delegate?.controlServerDidChangeStatus(self)
            }
        }
    }

    func stop(reason: String = "stopped") {
        queue.async { [self] in stopLocked(reason: reason) }
    }

    private func stopLocked(reason: String) {
        watchdog?.cancel()
        watchdog = nil
        for peer in peers.values { close(peer, reason: reason, notify: false) }
        peers.removeAll()
        if session != nil {
            session = nil
            delegate?.controlServer(self, didEnd: reason)
        }
        listener?.cancel()
        listener = nil
        isListening = false
        connectedClientName = nil
        connectedClientAddress = nil
        delegate?.controlServerDidChangeStatus(self)
    }

    func update(config: Config) {
        queue.async { [self] in
            let needsRestart = config.tcpPort != self.config.tcpPort
            self.config = config
            if needsRestart && listener != nil {
                Log.info("TCP port changed; restarting the listener")
                start()
            }
        }
    }

    func update(keyStore: KeyStore) {
        queue.async { [self] in self.keyStore = keyStore }
    }

    // MARK: - Pairing

    @discardableResult
    func beginPairing(validFor seconds: TimeInterval = 180) -> String {
        let code = Crypto.generatePairingCode()
        queue.sync {
            pairingCodeValue = code
            pairingExpiry = Date().addingTimeInterval(seconds)
        }
        Log.info("pairing mode enabled for \(Int(seconds))s")
        delegate?.controlServerDidChangeStatus(self)
        return code
    }

    func cancelPairing() {
        queue.async { [self] in
            pairingCodeValue = nil
            pairingExpiry = .distantPast
            delegate?.controlServerDidChangeStatus(self)
        }
    }

    func forgetAllDevices() {
        queue.async { [self] in
            keyStore = KeyStore()
            keyStore.save()
            if let session { close(session, reason: "devices unpaired", notify: true) }
        }
    }

    // MARK: - Sending

    func send(_ message: Proto.Message) {
        queue.async { [self] in
            guard let session, session.phase == .authenticated else { return }
            sendReliable(session, message)
        }
    }

    func disconnectClient() {
        queue.async { [self] in
            guard let session else { return }
            sendReliable(session, .bye(reason: 0))
            close(session, reason: "disconnected by the user", notify: true)
        }
    }

    // MARK: - Connection handling

    private func accept(_ connection: NWConnection) {
        let peer = Peer(connection)
        peers[ObjectIdentifier(connection)] = peer
        connection.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            switch state {
            case .ready:
                Log.debug("incoming control connection from \(Self.describe(connection))")
                readFrame(peer)
            case let .failed(error):
                close(peer, reason: "connection failed: \(error)", notify: true)
            case .cancelled:
                close(peer, reason: "connection cancelled", notify: true)
            default:
                break
            }
        }
        connection.start(queue: queue)
    }

    private static func describe(_ connection: NWConnection) -> String {
        switch connection.endpoint {
        case let .hostPort(host, port):
            return "\(host):\(port)"
        default:
            return "\(connection.endpoint)"
        }
    }

    private func close(_ peer: Peer, reason: String, notify: Bool) {
        guard peer.phase != .closed else { return }
        peer.phase = .closed
        peer.nw.cancel()
        peers.removeValue(forKey: ObjectIdentifier(peer.nw))
        if session === peer {
            session = nil
            connectedClientName = nil
            connectedClientAddress = nil
            Log.warn("session ended: \(reason)")
            if notify { delegate?.controlServer(self, didEnd: reason) }
            delegate?.controlServerDidChangeStatus(self)
        }
    }

    private func readFrame(_ peer: Peer) {
        peer.nw.receive(minimumIncompleteLength: 4, maximumLength: 4) { [weak self] data, _, isComplete, error in
            guard let self else { return }
            if let error {
                close(peer, reason: "read failed: \(error)", notify: true)
                return
            }
            guard let data, data.count == 4 else {
                close(peer, reason: isComplete ? "peer closed the connection" : "short header", notify: true)
                return
            }
            let length = Int(data.readBigEndian(UInt32.self, at: 0) ?? 0)
            guard length > 0, length <= Proto.maxFrameLength else {
                close(peer, reason: "bad frame length \(length)", notify: true)
                return
            }
            peer.nw.receive(minimumIncompleteLength: length, maximumLength: length) { [weak self] body, _, isComplete, error in
                guard let self else { return }
                if let error {
                    close(peer, reason: "read failed: \(error)", notify: true)
                    return
                }
                guard let body, body.count == length else {
                    close(peer, reason: isComplete ? "peer closed the connection" : "short frame", notify: true)
                    return
                }
                peer.lastFrame = Date()
                handle(peer, frame: body)
                if peer.phase != .closed {
                    readFrame(peer)
                }
            }
        }
    }

    private func handle(_ peer: Peer, frame: Data) {
        switch peer.phase {
        case .hello:
            handleHello(peer, frame: frame)
        case .pairOrAuth:
            handlePairOrAuth(peer, frame: frame)
        case .authenticated:
            handleReliable(peer, frame: frame)
        case .closed:
            break
        }
    }

    // MARK: - Handshake

    private func json(_ data: Data) -> [String: Any]? {
        (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
    }

    private func sendJSON(_ peer: Peer, _ object: [String: Any]) {
        guard let payload = try? JSONSerialization.data(withJSONObject: object) else { return }
        var frame = Data()
        frame.append(bigEndian: UInt32(payload.count))
        frame.append(payload)
        peer.nw.send(content: frame, completion: .contentProcessed { _ in })
    }

    private func sendError(_ peer: Peer, code: String, message: String) {
        Log.warn("rejecting client: \(code) - \(message)")
        sendJSON(peer, ["t": "ERROR", "code": code, "message": message])
        lastError = message
        // Give the frame a moment to leave before tearing the socket down.
        queue.asyncAfter(deadline: .now() + 0.1) { [weak self] in
            self?.close(peer, reason: code, notify: false)
        }
        delegate?.controlServerDidChangeStatus(self)
    }

    private func handleHello(_ peer: Peer, frame: Data) {
        guard let object = json(frame), (object["t"] as? String) == "HELLO" else {
            sendError(peer, code: "PROTOCOL_ERROR", message: "expected HELLO")
            return
        }
        let version = (object["protocol_version"] as? NSNumber)?.uint32Value ?? 0
        guard version == Proto.version else {
            sendError(
                peer,
                code: "VERSION_MISMATCH",
                message: "this Mac speaks protocol \(Proto.version), the sender speaks \(version)"
            )
            return
        }
        guard let clientID = object["client_id"] as? String, !clientID.isEmpty,
              let nonceHex = object["client_nonce"] as? String,
              let nonce = Data(hexString: nonceHex), nonce.count == 32
        else {
            sendError(peer, code: "PROTOCOL_ERROR", message: "HELLO is missing an id or nonce")
            return
        }
        peer.clientID = clientID
        peer.clientName = (object["client_name"] as? String) ?? "Windows PC"
        peer.clientNonce = nonce
        peer.serverNonce = Crypto.randomData(32)
        peer.deviceKey = keyStore.deviceKey(clientID: clientID)
        peer.phase = .pairOrAuth

        let pairingActive = pairingExpiry > Date() && pairingCodeValue != nil
        sendJSON(peer, [
            "t": "HELLO_ACK",
            "protocol_version": Proto.version,
            "server_name": config.deviceName,
            "server_nonce": peer.serverNonce.hexString,
            "known_client": peer.deviceKey != nil,
            "pairing_mode": pairingActive,
            "capabilities": Proto.capabilities,
        ])
        Log.info("HELLO from \(peer.clientName) (\(Self.describe(peer.nw))), known: \(peer.deviceKey != nil)")
    }

    private func handlePairOrAuth(_ peer: Peer, frame: Data) {
        guard let object = json(frame), let type = object["t"] as? String else {
            sendError(peer, code: "PROTOCOL_ERROR", message: "expected PAIR_REQUEST or AUTH")
            return
        }
        switch type {
        case "PAIR_REQUEST":
            guard let code = pairingCodeValue, pairingExpiry > Date() else {
                sendError(peer, code: "PAIRING_DISABLED", message: "this Mac is not in pairing mode")
                return
            }
            let pairingKey = Crypto.pairingKey(code: code)
            let expected = Crypto.pairProof(
                pairingKey: pairingKey,
                clientNonce: peer.clientNonce,
                serverNonce: peer.serverNonce
            )
            let provided = Data(hexString: object["proof"] as? String ?? "") ?? Data()
            guard Crypto.constantTimeEquals(expected, provided) else {
                sendError(peer, code: "BAD_PROOF", message: "wrong pairing code")
                return
            }
            let deviceKey = Crypto.randomData(32)
            let wrapped = Crypto.xorBytes(deviceKey, Crypto.wrapMask(pairingKey: pairingKey))
            let tag = Crypto.wrapTag(
                pairingKey: pairingKey,
                clientNonce: peer.clientNonce,
                serverNonce: peer.serverNonce,
                wrapped: wrapped
            )
            keyStore.setDeviceKey(clientID: peer.clientID, name: peer.clientName, key: deviceKey)
            keyStore.save()
            peer.deviceKey = deviceKey
            // One code pairs one machine; leaving it live would let a second host in.
            pairingCodeValue = nil
            pairingExpiry = .distantPast
            sendJSON(peer, ["t": "PAIR_RESPONSE", "wrapped_key": wrapped.hexString, "tag": tag.hexString])
            Log.info("paired with \(peer.clientName)")
            delegate?.controlServerDidChangeStatus(self)

        case "AUTH":
            guard let deviceKey = peer.deviceKey else {
                sendError(peer, code: "NOT_PAIRED", message: "this Windows PC is not paired yet")
                return
            }
            let expected = Crypto.authProof(
                deviceKey: deviceKey,
                clientNonce: peer.clientNonce,
                serverNonce: peer.serverNonce
            )
            let provided = Data(hexString: object["proof"] as? String ?? "") ?? Data()
            guard Crypto.constantTimeEquals(expected, provided) else {
                sendError(peer, code: "BAD_PROOF", message: "authentication failed")
                return
            }
            var sessionID: UInt64 = 0
            let random = Crypto.randomData(8)
            sessionID = random.readBigEndian(UInt64.self, at: 0) ?? 1
            if sessionID == 0 { sessionID = 1 }

            let keys = Crypto.sessionKeys(
                deviceKey: deviceKey,
                clientNonce: peer.clientNonce,
                serverNonce: peer.serverNonce
            )
            peer.sessionID = sessionID
            peer.tcpKey = keys.tcp
            peer.udpKey = keys.udp
            peer.phase = .authenticated

            let serverProof = Crypto.authAckProof(
                deviceKey: deviceKey,
                clientNonce: peer.clientNonce,
                serverNonce: peer.serverNonce,
                sessionID: sessionID
            )
            sendJSON(peer, [
                "t": "AUTH_OK",
                "session_id": sessionID,
                "server_proof": serverProof.hexString,
                "udp_port": config.udpPort,
            ])

            // Only now is the previous session replaced: an unauthenticated peer must never be
            // able to kick a working session.
            if let previous = session, previous !== peer {
                sendReliable(previous, .bye(reason: 3))
                close(previous, reason: "replaced by a new session", notify: false)
            }
            session = peer
            connectedClientName = peer.clientName
            connectedClientAddress = Self.describe(peer.nw)
            lastError = nil
            Log.info(String(format: "session 0x%llx established with %@", sessionID, peer.clientName))
            delegate?.controlServer(
                self,
                didStart: SessionInfo(
                    sessionID: sessionID,
                    udpKey: keys.udp,
                    clientName: peer.clientName,
                    clientID: peer.clientID
                )
            )
            delegate?.controlServerDidChangeStatus(self)

        default:
            sendError(peer, code: "PROTOCOL_ERROR", message: "unexpected frame \(type)")
        }
    }

    // MARK: - Session traffic

    private func sendReliable(_ peer: Peer, _ message: Proto.Message) {
        peer.sendCounter += 1
        var signed = Data()
        signed.append(bigEndian: peer.sendCounter)
        signed.append(message.type.rawValue)
        signed.append(message.encodeBody())
        let tag = Crypto.hmac(key: peer.tcpKey, data: signed).prefix(Proto.tagLength)
        var frame = Data()
        frame.append(bigEndian: UInt32(signed.count + Proto.tagLength))
        frame.append(signed)
        frame.append(tag)
        peer.nw.send(content: frame, completion: .contentProcessed { _ in })
    }

    private func handleReliable(_ peer: Peer, frame: Data) {
        guard frame.count >= 8 + 1 + Proto.tagLength else {
            close(peer, reason: "short session frame", notify: true)
            return
        }
        let split = frame.count - Proto.tagLength
        let signed = frame.prefix(split)
        let tag = frame.suffix(Proto.tagLength)
        let expected = Crypto.hmac(key: peer.tcpKey, data: Data(signed)).prefix(Proto.tagLength)
        guard Crypto.constantTimeEquals(Data(expected), Data(tag)) else {
            close(peer, reason: "bad frame authentication tag", notify: true)
            return
        }
        guard let counter: UInt64 = signed.readBigEndian(UInt64.self, at: 0), counter > peer.receiveCounter else {
            close(peer, reason: "replayed or reordered control frame", notify: true)
            return
        }
        peer.receiveCounter = counter
        let type = signed[signed.startIndex + 8]
        let body = Data(signed.dropFirst(9))
        guard let message = Proto.Message.decode(type: type, body: body) else {
            Log.debug(String(format: "ignoring unknown reliable message 0x%02X", type))
            return
        }
        if case let .ping(sent) = message {
            let snapshot = delegate?.controlServerTelemetry(self) ?? Telemetry.Snapshot()
            sendReliable(peer, .pong(
                sentMicros: sent,
                appliedEvents: snapshot.appliedEvents,
                udpReceived: snapshot.udpReceived,
                udpDropped: snapshot.udpMissing
            ))
            return
        }
        if case let .bye(reason) = message {
            close(peer, reason: "sender said goodbye (reason \(reason))", notify: true)
            return
        }
        delegate?.controlServer(self, didReceive: message)
    }

    // MARK: - Watchdog

    private func startWatchdogLocked() {
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + 0.25, repeating: 0.25)
        timer.setEventHandler { [weak self] in
            guard let self, let session else { return }
            let silence = Date().timeIntervalSince(session.lastFrame) * 1000
            if silence > config.heartbeatTimeoutMs {
                // Spec §18: silence is indistinguishable from a dead sender, so treat it as one
                // and let the receiver release everything.
                close(session, reason: "heartbeat timeout after \(Int(silence)) ms", notify: true)
            }
        }
        timer.resume()
        watchdog = timer
    }
}

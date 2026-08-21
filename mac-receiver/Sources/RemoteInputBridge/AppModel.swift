import AppKit
import Combine
import Foundation
import SwiftUI
import ServiceManagement

/// Coordinator: owns the receiver stack and publishes everything the UI shows.
///
/// Delegate callbacks arrive on the network queues, so every published mutation is bounced to the
/// main queue - the network path must never wait on the UI (spec §34).
final class AppModel: ObservableObject, ControlServerDelegate {
    @Published var config: Config
    @Published private(set) var listening = false
    @Published private(set) var connectedClient: String?
    @Published private(set) var connectedAddress: String?
    @Published private(set) var inputActive = false
    @Published private(set) var pairingCode: String?
    @Published private(set) var lastError: String?
    @Published private(set) var diagnostics = "no traffic yet"
    @Published private(set) var canPostEvents = Permissions.canPostEvents

    let telemetry = Telemetry()
    let injector = InputInjector()
    private let scheduler: EventScheduler
    private let realtime: RealtimeReceiver
    private let control: ControlServer
    private var keyStore = KeyStore.load()

    private var senderWantsEdgeSwitch = false
    private var lastEdgeHit = Date.distantPast
    private var diagnosticsTimer: Timer?
    private var permissionTimer: Timer?
    private var previousSnapshot = Telemetry.Snapshot()
    private var previousSampleTime = Date()

    init() {
        let config = Config.load()
        self.config = config
        Log.shared.setLevel(LogLevel.named(config.logLevel))
        scheduler = EventScheduler(injector: injector, telemetry: telemetry)
        realtime = RealtimeReceiver(scheduler: scheduler, telemetry: telemetry)
        control = ControlServer(config: config, keyStore: keyStore)
        control.delegate = self
        injector.apply(config: config)
        scheduler.apply(config: config)

        scheduler.onRightEdge = { [weak self] in self?.reportRightEdge() }

        NotificationCenter.default.addObserver(
            forName: NSApplication.didChangeScreenParametersNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            self?.injector.refreshDisplays()
            self?.scheduler.apply(config: self?.config ?? Config())
        }
        // Spec §43: after a wake the sender will reconnect; make sure nothing is left held.
        NSWorkspace.shared.notificationCenter.addObserver(
            forName: NSWorkspace.willSleepNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            self?.injector.releaseAll(reason: "this Mac is going to sleep")
        }
        NSWorkspace.shared.notificationCenter.addObserver(
            forName: NSWorkspace.didWakeNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            guard let self else { return }
            Log.info("this Mac woke up; restarting the listeners")
            injector.refreshDisplays()
            if config.receiverEnabled { restart() }
        }
    }

    // MARK: - Lifecycle

    func start() {
        guard config.receiverEnabled else {
            Log.info("receiver is disabled in settings")
            return
        }
        scheduler.start()
        control.update(config: config)
        control.start()
        realtime.start(port: config.udpPort)
        startTimers()
        refreshStatus()
    }

    func stop(reason: String = "receiver disabled") {
        control.stop(reason: reason)
        realtime.stop()
        scheduler.stop()
        injector.releaseAll(reason: reason)
        inputActive = false
        refreshStatus()
    }

    func restart() {
        stop(reason: "restarting")
        start()
    }

    func setReceiverEnabled(_ enabled: Bool) {
        config.receiverEnabled = enabled
        config.save()
        if enabled { start() } else { stop() }
    }

    private func startTimers() {
        diagnosticsTimer?.invalidate()
        let diagnostics = Timer(timeInterval: 1, repeats: true) { [weak self] _ in
            self?.sampleDiagnostics()
        }
        RunLoop.main.add(diagnostics, forMode: .common)
        diagnosticsTimer = diagnostics

        permissionTimer?.invalidate()
        let permissions = Timer(timeInterval: 2, repeats: true) { [weak self] _ in
            guard let self else { return }
            let granted = Permissions.canPostEvents
            if granted != canPostEvents { canPostEvents = granted }
        }
        RunLoop.main.add(permissions, forMode: .common)
        permissionTimer = permissions
    }

    // MARK: - Configuration

    func binding<T>(_ keyPath: WritableKeyPath<Config, T>) -> Binding<T> {
        Binding(
            get: { self.config[keyPath: keyPath] },
            set: { newValue in
                self.config[keyPath: keyPath] = newValue
                self.configDidChange()
            }
        )
    }

    func configDidChange() {
        let sanitized = config.sanitized()
        Log.shared.setLevel(LogLevel.named(sanitized.logLevel))
        injector.apply(config: sanitized)
        scheduler.apply(config: sanitized)
        control.update(config: sanitized)
        if sanitized.udpPort != realtime.currentPort {
            realtime.start(port: sanitized.udpPort)
        }
        sanitized.save()
    }

    func applyStartAtLogin(_ enabled: Bool) {
        config.startAtLogin = enabled
        config.save()
        guard #available(macOS 13.0, *) else { return }
        do {
            if enabled {
                if SMAppService.mainApp.status != .enabled { try SMAppService.mainApp.register() }
            } else {
                if SMAppService.mainApp.status == .enabled { try SMAppService.mainApp.unregister() }
            }
        } catch {
            // Unsigned local builds cannot register a login item; say so instead of failing quietly.
            Log.warn("could not change the login item: \(error.localizedDescription)")
            lastError = "Login item needs a signed app bundle: \(error.localizedDescription)"
        }
    }

    // MARK: - Pairing

    func beginPairing() {
        let code = control.beginPairing()
        pairingCode = code
        Log.info("pairing code: \(code)")
    }

    func cancelPairing() {
        control.cancelPairing()
        pairingCode = nil
    }

    func forgetDevices() {
        control.forgetAllDevices()
        keyStore = KeyStore()
        refreshStatus()
    }

    func disconnectClient() {
        control.disconnectClient()
    }

    var pairedDeviceNames: [String] {
        Array(KeyStore.load().names.values).sorted()
    }

    // MARK: - Diagnostics

    private func sampleDiagnostics() {
        let snapshot = telemetry.snapshot
        let now = Date()
        let elapsed = max(now.timeIntervalSince(previousSampleTime), 0.001)
        previousSampleTime = now

        let received = Double(snapshot.udpReceived &- previousSnapshot.udpReceived) / elapsed
        let applied = Double(snapshot.appliedEvents &- previousSnapshot.appliedEvents) / elapsed
        let coalesced = Double(snapshot.coalescedEvents &- previousSnapshot.coalescedEvents) / elapsed
        let missing = Double(snapshot.udpMissing &- previousSnapshot.udpMissing) / elapsed
        let reliable = Double(snapshot.reliableEvents &- previousSnapshot.reliableEvents) / elapsed
        previousSnapshot = snapshot

        let total = received + missing
        let loss = total > 0 ? missing / total * 100 : 0
        let text = String(
            format: "udp in %.0f Hz | events %.0f Hz | coalesced %.0f Hz | loss %.1f%% | reliable %.0f Hz",
            received, applied, coalesced, loss, reliable
        )
        diagnostics = text
        if config.diagnostics {
            // Through the logger, not print: a bundle launched from Finder has no stdout, and
            // this line is the whole point of the diagnostics switch.
            Log.info("diag \(text)")
        }
    }

    private func refreshStatus() {
        listening = control.isListening && realtime.isListening
        connectedClient = control.connectedClientName
        connectedAddress = control.connectedClientAddress
        pairingCode = control.pairingCode
        if let error = control.lastError ?? realtime.lastError {
            lastError = error
        } else if control.isListening {
            lastError = nil
        }
    }

    var statusLine: String {
        if !canPostEvents { return "Accessibility permission required" }
        if !config.receiverEnabled { return "Receiver disabled" }
        guard listening else { return lastError ?? "Not listening" }
        guard let client = connectedClient else { return "Waiting for Windows" }
        return inputActive ? "Receiving input from \(client)" : "Connected to \(client)"
    }

    private func reportRightEdge() {
        guard config.edgeSwitch, senderWantsEdgeSwitch else { return }
        let now = Date()
        guard now.timeIntervalSince(lastEdgeHit) > 0.7 else { return }
        lastEdgeHit = now
        Log.info("cursor hit the right edge; asking Windows to take the input back")
        control.send(.edgeHit(edge: 1))
    }

    // MARK: - ControlServerDelegate

    func controlServer(_ server: ControlServer, didStart session: SessionInfo) {
        telemetry.resetSession()
        realtime.beginSession(id: session.sessionID, key: session.udpKey)
        injector.releaseAll(reason: "new session")
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            inputActive = false
            refreshStatus()
        }
    }

    func controlServer(_ server: ControlServer, didEnd reason: String) {
        realtime.endSession()
        scheduler.flushPending()
        // Fail-safe (spec §18/§51): whatever went wrong, nothing may stay held on this Mac.
        injector.releaseAll(reason: reason)
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            inputActive = false
            senderWantsEdgeSwitch = false
            refreshStatus()
        }
    }

    func controlServer(_ server: ControlServer, didReceive message: Proto.Message) {
        telemetry.countReliable()
        switch message {
        case let .sessionStart(interval, flags):
            senderWantsEdgeSwitch = flags & 1 != 0
            Log.info("session start: sender coalesces every \(interval) us, edge switching \(senderWantsEdgeSwitch ? "on" : "off")")

        case let .targetActive(active):
            if active {
                injector.rebaseCursor()
            } else {
                scheduler.flushPending()
                injector.releaseAll(reason: "Windows took the input back")
            }
            realtime.setActive(active)
            Log.info("input target: \(active ? "this Mac" : "Windows")")
            DispatchQueue.main.async { [weak self] in
                self?.inputActive = active
                self?.refreshStatus()
            }

        case .releaseAll:
            injector.releaseAll(reason: "requested by the sender")

        case let .modifierSync(mask):
            injector.syncModifiers(mask)

        case let .mouseButton(button, down):
            guard inputActiveUnsafe else { return }
            injector.mouseButton(button, down: down)

        case let .scroll(x, y):
            guard inputActiveUnsafe else { return }
            injector.scroll(unitsX: x, unitsY: y)

        case let .key(usage, down, repeatPress):
            guard inputActiveUnsafe else { return }
            injector.key(usage: usage, down: down, repeatPress: repeatPress)

        case let .mouseMoveRelative(dx, dy):
            guard inputActiveUnsafe else { return }
            scheduler.submit(dx: dx, dy: dy)

        default:
            break
        }
    }

    /// `inputActive` is published on the main queue; reliable messages arrive on the control
    /// queue, so this mirrors the flag without a hop.
    private var inputActiveUnsafe: Bool {
        realtime.isActive
    }

    func controlServerTelemetry(_ server: ControlServer) -> Telemetry.Snapshot {
        telemetry.snapshot
    }

    func controlServerDidChangeStatus(_ server: ControlServer) {
        DispatchQueue.main.async { [weak self] in self?.refreshStatus() }
    }
}

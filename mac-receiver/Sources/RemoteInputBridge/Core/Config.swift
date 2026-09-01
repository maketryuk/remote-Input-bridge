import Foundation
import Security

enum SchedulerMode: String, Codable, CaseIterable, Identifiable {
    /// Post a CGEvent the instant a datagram is parsed. Lowest latency, most exposed to bursts.
    case immediate
    /// Coalesce anything that arrives inside `minEventIntervalMs` into one event (default).
    case coalesced
    /// Post on a fixed cadence, by default the display refresh rate.
    case paced
    /// Spread each arrival over a few milliseconds and keep moving through the gap that follows.
    /// Costs a little latency, removes the tearing a jittery Wi-Fi link produces (default).
    case smoothed

    var id: String { rawValue }
    var label: String {
        switch self {
        case .immediate: return "Immediate"
        case .coalesced: return "Coalesced (recommended)"
        case .paced: return "Paced to display"
        case .smoothed: return "Smoothed (recommended)"
        }
    }
}

enum ScrollMode: String, Codable, CaseIterable, Identifiable {
    /// Continuous pixel scrolling: smooth, trackpad-like, keeps high-resolution wheel deltas.
    case pixel
    /// Classic line scrolling: matches an old-school notched wheel.
    case line

    var id: String { rawValue }
    var label: String { self == .pixel ? "Pixels (smooth)" : "Lines (classic)" }
}

/// What a physical Windows modifier becomes on macOS (spec §15).
enum ModifierRole: String, Codable, CaseIterable, Identifiable {
    case control, option, command, none

    var id: String { rawValue }
    var label: String {
        switch self {
        case .control: return "Control"
        case .option: return "Option"
        case .command: return "Command"
        case .none: return "Ignore"
        }
    }
}

struct ModifierMapping: Codable, Equatable {
    var control: ModifierRole = .control
    var alt: ModifierRole = .option
    var gui: ModifierRole = .command
    /// Shift is never remapped: no sane mapping exists and it would break every text selection.
    static let `default` = ModifierMapping()
}

struct Config: Codable, Equatable {
    var tcpPort: UInt16 = 47821
    var udpPort: UInt16 = 47822
    var deviceName: String = Host.current().localizedName ?? "Mac"
    var receiverEnabled = true

    /// Windows sends raw, unaccelerated counts, so this is the only place pointer speed is
    /// scaled - which is exactly how double acceleration is avoided (spec §55).
    var pointerScale: Double = 1.0
    var schedulerMode: SchedulerMode = .smoothed
    var minEventIntervalMs: Double = 1.0
    /// How long the smoothed scheduler takes to play out one arrival. Roughly the amount of
    /// network jitter it can absorb, and roughly the latency it adds.
    var smoothingMs: Double = 10
    /// 0 means "follow the display refresh rate".
    var pacedRateHz: Double = 0

    var scrollMode: ScrollMode = .pixel
    var scrollLinesPerNotch: Double = 3
    var scrollPixelsPerLine: Double = 10
    var naturalScrolling = false

    var modifiers = ModifierMapping.default

    /// Report right-edge hits back to Windows so it can take the input back (spec §6.4).
    var edgeSwitch = false
    var heartbeatTimeoutMs: Double = 1000
    var logLevel = "INFO"
    var startAtLogin = false
    /// Look for a new release once a day and offer it in the settings window. Only ever a check and
    /// a download the user confirms - nothing installs itself.
    var autoCheckUpdates = true
    var diagnostics = false

    static func directory() -> URL {
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSHomeDirectory()).appendingPathComponent("Library/Application Support")
        return base.appendingPathComponent("RemoteInputBridge", isDirectory: true)
    }

    static func fileURL() -> URL { directory().appendingPathComponent("config.json") }

    static func load() -> Config {
        guard let data = try? Data(contentsOf: fileURL()) else { return Config() }
        do {
            // Layer the file on top of the encoded defaults instead of decoding it directly.
            // Swift's synthesised decoder treats every missing key as a hard error, which would
            // mean a hand-edited file - or one written by a build that had fewer settings -
            // silently reverts every other setting the user chose.
            let stored = (try JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
            let defaultsData = try JSONEncoder().encode(Config())
            var merged = (try JSONSerialization.jsonObject(with: defaultsData)) as? [String: Any] ?? [:]
            for (key, value) in stored {
                if var base = merged[key] as? [String: Any], let nested = value as? [String: Any] {
                    for (nestedKey, nestedValue) in nested { base[nestedKey] = nestedValue }
                    merged[key] = base
                } else {
                    merged[key] = value
                }
            }
            let mergedData = try JSONSerialization.data(withJSONObject: merged)
            let config = try JSONDecoder().decode(Config.self, from: mergedData).sanitized()
            let unknown = Set(stored.keys).subtracting(merged.keys)
            if !unknown.isEmpty {
                Log.warn("ignoring unknown settings in config.json: \(unknown.sorted())")
            }
            return config
        } catch {
            Log.warn("config.json could not be read (\(error)); using defaults")
            return Config()
        }
    }

    func save() {
        do {
            try FileManager.default.createDirectory(at: Config.directory(), withIntermediateDirectories: true)
            let encoder = JSONEncoder()
            encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
            try encoder.encode(self).write(to: Config.fileURL(), options: .atomic)
        } catch {
            Log.warn("could not write config.json: \(error)")
        }
    }

    func sanitized() -> Config {
        var copy = self
        copy.pointerScale = min(max(pointerScale, 0.1), 10)
        copy.minEventIntervalMs = min(max(minEventIntervalMs, 0), 16)
        copy.smoothingMs = min(max(smoothingMs, 0), 60)
        copy.pacedRateHz = min(max(pacedRateHz, 0), 1000)
        copy.scrollLinesPerNotch = min(max(scrollLinesPerNotch, 0.1), 20)
        copy.scrollPixelsPerLine = min(max(scrollPixelsPerLine, 1), 100)
        copy.heartbeatTimeoutMs = min(max(heartbeatTimeoutMs, 200), 30_000)
        if copy.tcpPort == 0 { copy.tcpPort = 47821 }
        if copy.udpPort == 0 { copy.udpPort = 47822 }
        if copy.deviceName.trimmingCharacters(in: .whitespaces).isEmpty {
            copy.deviceName = Host.current().localizedName ?? "Mac"
        }
        return copy
    }
}

/// Device keys handed out during pairing, one per Windows install (spec §28).
struct KeyStore: Codable {
    /// clientID (hex) -> device key (hex)
    var devices: [String: String] = [:]
    var names: [String: String] = [:]
    /// This Mac's own identity, handed to senders so they can recognise it again.
    ///
    /// Without it a sender has nothing to file its device key under but the address it was told to
    /// dial, and an address is not an identity: a new DHCP lease, or simply switching from
    /// 192.168.1.5 to the .local name, made an already-paired Mac look like a stranger and asked
    /// for the pairing code again.
    var serverID: String = ""

    /// Created once and kept, so it survives everything except deleting keys.json.
    static func loadWithIdentity() -> KeyStore {
        var store = load()
        if store.serverID.isEmpty {
            var bytes = [UInt8](repeating: 0, count: 16)
            _ = SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes)
            store.serverID = Data(bytes).hexString
            store.save()
        }
        return store
    }

    static func fileURL() -> URL { Config.directory().appendingPathComponent("keys.json") }

    /// Decoded key by key, because the synthesised decoder treats a missing one as a hard error
    /// even when the property has a default. This file holds credentials: adding a field to it
    /// must never be able to make the previous version unreadable, and adding `serverID` did
    /// exactly that once - every paired PC was forgotten and had to be paired again.
    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        devices = try container.decodeIfPresent([String: String].self, forKey: .devices) ?? [:]
        names = try container.decodeIfPresent([String: String].self, forKey: .names) ?? [:]
        serverID = try container.decodeIfPresent(String.self, forKey: .serverID) ?? ""
    }

    init() {}

    static func load() -> KeyStore {
        guard let data = try? Data(contentsOf: fileURL()) else { return KeyStore() }
        do {
            return try JSONDecoder().decode(KeyStore.self, from: data)
        } catch {
            // Never write over something we could not read: the keys in it are not reproducible,
            // and a copy costs nothing next to pairing every machine again.
            let rescued = fileURL().appendingPathExtension("unreadable")
            try? FileManager.default.removeItem(at: rescued)
            try? FileManager.default.copyItem(at: fileURL(), to: rescued)
            Log.error("keys.json could not be read (\(error)); a copy is at \(rescued.path)")
            return KeyStore()
        }
    }

    func save() {
        do {
            try FileManager.default.createDirectory(at: Config.directory(), withIntermediateDirectories: true)
            let encoder = JSONEncoder()
            encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
            try encoder.encode(self).write(to: KeyStore.fileURL(), options: .atomic)
            // The device key is a credential: keep it out of reach of other users on this Mac.
            try FileManager.default.setAttributes(
                [.posixPermissions: 0o600], ofItemAtPath: KeyStore.fileURL().path
            )
        } catch {
            Log.warn("could not write keys.json: \(error)")
        }
    }

    func deviceKey(clientID: String) -> Data? {
        guard let hex = devices[clientID] else { return nil }
        return Data(hexString: hex)
    }

    mutating func setDeviceKey(clientID: String, name: String, key: Data) {
        devices[clientID] = key.hexString
        names[clientID] = name
    }
}

import Foundation

/// Levelled logging (spec §40). Mouse packets are only ever logged at `.trace`, and the level
/// check is a plain integer compare so the hot path costs nothing when tracing is off.
enum LogLevel: Int, Comparable, CaseIterable {
    case error = 0, warn, info, debug, trace

    var name: String {
        switch self {
        case .error: return "ERROR"
        case .warn: return "WARN"
        case .info: return "INFO"
        case .debug: return "DEBUG"
        case .trace: return "TRACE"
        }
    }

    static func named(_ text: String) -> LogLevel {
        LogLevel.allCases.first { $0.name == text.uppercased() } ?? .info
    }

    static func < (lhs: LogLevel, rhs: LogLevel) -> Bool { lhs.rawValue < rhs.rawValue }
}

final class Log {
    static let shared = Log()
    private let lock = NSLock()
    private var level: LogLevel = .info
    private let started = Date()

    var currentLevel: LogLevel {
        lock.lock(); defer { lock.unlock() }
        return level
    }

    func setLevel(_ level: LogLevel) {
        lock.lock()
        self.level = level
        lock.unlock()
    }

    func enabled(_ level: LogLevel) -> Bool { level <= currentLevel }

    private func emit(_ level: LogLevel, _ message: String) {
        guard enabled(level) else { return }
        let uptime = Date().timeIntervalSince(started)
        print(String(format: "[%9.3fs] %-5@ %@", uptime, level.name as NSString, message as NSString))
    }

    static func error(_ message: String) { shared.emit(.error, message) }
    static func warn(_ message: String) { shared.emit(.warn, message) }
    static func info(_ message: String) { shared.emit(.info, message) }
    static func debug(_ message: String) { shared.emit(.debug, message) }
    static func trace(_ message: String) { shared.emit(.trace, message) }
}

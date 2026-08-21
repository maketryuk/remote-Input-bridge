import AppKit
import ApplicationServices

/// Accessibility permission handling (spec §23). Without it `CGEventPost` silently does nothing,
/// which is the single most confusing failure mode this app can have - so it is surfaced in the
/// menu, in the settings window, and in the log.
enum Permissions {
    static var canPostEvents: Bool {
        CGPreflightPostEventAccess()
    }

    static var isTrusted: Bool {
        AXIsProcessTrusted()
    }

    /// Triggers the system prompt. Only shows something the first time; afterwards the user has
    /// to toggle the entry in System Settings by hand.
    @discardableResult
    static func request() -> Bool {
        let granted = CGRequestPostEventAccess()
        if !granted {
            let options = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true]
            AXIsProcessTrustedWithOptions(options as CFDictionary)
        }
        return granted
    }

    static func openSystemSettings() {
        let url = URL(
            string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
        )!
        NSWorkspace.shared.open(url)
    }

    static let explanation = """
        Remote Input Bridge requires Accessibility permission
        to control mouse and keyboard.
        """
}

import AppKit
import Combine
import SwiftUI

/// Menu bar app (spec §37). No main window is required; the settings window is created on demand.
final class MenuBarController: NSObject, NSMenuDelegate {
    private let model: AppModel
    private let statusItem: NSStatusItem
    private var settingsWindow: NSWindow?
    private var cancellables = Set<AnyCancellable>()

    init(model: AppModel) {
        self.model = model
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        super.init()

        let menu = NSMenu()
        menu.delegate = self
        statusItem.menu = menu
        updateButton()

        // Redraw the icon whenever anything user-visible changes.
        for publisher in [
            model.$listening.map { _ in () }.eraseToAnyPublisher(),
            model.$connectedClient.map { _ in () }.eraseToAnyPublisher(),
            model.$inputActive.map { _ in () }.eraseToAnyPublisher(),
            model.$canPostEvents.map { _ in () }.eraseToAnyPublisher(),
        ] {
            publisher
                .receive(on: DispatchQueue.main)
                .sink { [weak self] _ in self?.updateButton() }
                .store(in: &cancellables)
        }
    }

    private func updateButton() {
        guard let button = statusItem.button else { return }
        let symbol: String
        let description: String
        if !model.canPostEvents {
            symbol = "exclamationmark.triangle"
            description = "Accessibility permission required"
        } else if model.inputActive {
            symbol = "cursorarrow.rays"
            description = "Receiving input"
        } else if model.connectedClient != nil {
            symbol = "cursorarrow"
            description = "Connected"
        } else if model.listening {
            symbol = "cursorarrow.slash"
            description = "Waiting for Windows"
        } else {
            symbol = "cursorarrow.slash"
            description = "Not listening"
        }
        button.image = NSImage(
            systemSymbolName: symbol,
            accessibilityDescription: "Remote Input Bridge - \(description)"
        )
        button.toolTip = "Remote Input Bridge - \(model.statusLine)"
    }

    // MARK: - Menu

    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        addDisabled(menu, "Remote Input Bridge")
        addDisabled(menu, (model.inputActive || model.connectedClient != nil ? "● " : "○ ") + model.statusLine)
        if let address = model.connectedAddress {
            addDisabled(menu, "Windows: \(address)")
        }
        addDisabled(menu, model.diagnostics)
        if let error = model.lastError, !error.isEmpty {
            addDisabled(menu, error)
        }
        menu.addItem(.separator())

        if !model.canPostEvents {
            add(menu, "Grant Accessibility permission…", #selector(grantPermission))
            menu.addItem(.separator())
        }

        let receiver = add(menu, "Receiver enabled", #selector(toggleReceiver))
        receiver.state = model.config.receiverEnabled ? .on : .off

        if let code = model.pairingCode {
            addDisabled(menu, "Pairing code: \(code)")
            add(menu, "Cancel pairing", #selector(cancelPairing))
        } else {
            add(menu, "Show pairing code…", #selector(beginPairing))
        }
        if model.connectedClient != nil {
            add(menu, "Disconnect", #selector(disconnect))
        }
        menu.addItem(.separator())
        add(menu, "Settings…", #selector(openSettings))
        menu.addItem(.separator())
        add(menu, "Quit Remote Input Bridge", #selector(quit))
    }

    @discardableResult
    private func add(_ menu: NSMenu, _ title: String, _ action: Selector) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: action, keyEquivalent: "")
        item.target = self
        menu.addItem(item)
        return item
    }

    private func addDisabled(_ menu: NSMenu, _ title: String) {
        let item = NSMenuItem(title: title, action: nil, keyEquivalent: "")
        item.isEnabled = false
        menu.addItem(item)
    }

    // MARK: - Actions

    @objc private func toggleReceiver() {
        model.setReceiverEnabled(!model.config.receiverEnabled)
    }

    @objc private func beginPairing() {
        model.beginPairing()
        openSettings()
    }

    @objc private func cancelPairing() {
        model.cancelPairing()
    }

    @objc private func disconnect() {
        model.disconnectClient()
    }

    @objc private func grantPermission() {
        Permissions.request()
        Permissions.openSystemSettings()
    }

    @objc private func openSettings() {
        if let window = settingsWindow {
            window.makeKeyAndOrderFront(nil)
            NSApp.activate(ignoringOtherApps: true)
            return
        }
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 520, height: 640),
            styleMask: [.titled, .closable, .miniaturizable],
            backing: .buffered,
            defer: false
        )
        window.title = "Remote Input Bridge"
        window.isReleasedWhenClosed = false
        window.center()
        window.contentView = NSHostingView(rootView: SettingsView(model: model))
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        settingsWindow = window
    }

    @objc private func quit() {
        model.stop(reason: "quitting")
        NSApp.terminate(nil)
    }
}

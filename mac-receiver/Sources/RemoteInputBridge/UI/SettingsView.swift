import SwiftUI

/// Status + settings in one window (spec §38). Everything applies immediately; there is no Save
/// button to forget to press.
struct SettingsView: View {
    @ObservedObject var model: AppModel
    @ObservedObject private var updater = Updater.shared

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                status
                Divider()
                pairing
                Divider()
                network
                Divider()
                pointer
                Divider()
                scrolling
                Divider()
                modifiers
                Divider()
                behaviour
                Divider()
                updates
            }
            .padding(20)
        }
        .frame(minWidth: 480, minHeight: 560)
    }

    private var updates: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Updates").font(.headline)
            Text(updater.summary).font(.callout)
            HStack {
                Button(updater.actionTitle) { updater.act() }
                    .disabled(!updater.actionEnabled)
                Toggle("Check automatically", isOn: model.binding(\.autoCheckUpdates))
                    .padding(.leading, 12)
            }
            Text("Updates are downloaded from GitHub releases and checked against the digest published with them. Nothing else is sent.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private var status: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Status").font(.headline)
            HStack(spacing: 8) {
                Circle()
                    .fill(model.inputActive ? Color.green : (model.connectedClient != nil ? .blue : .secondary))
                    .frame(width: 10, height: 10)
                Text(model.statusLine)
            }
            Text(model.diagnostics)
                .font(.system(.caption, design: .monospaced))
                .foregroundStyle(.secondary)
            if let error = model.lastError, !error.isEmpty {
                Text(error).font(.caption).foregroundStyle(.orange)
            }
            if !model.canPostEvents {
                VStack(alignment: .leading, spacing: 6) {
                    Text(Permissions.explanation).font(.callout)
                    Button("Open System Settings") {
                        Permissions.request()
                        Permissions.openSystemSettings()
                    }
                }
                .padding(10)
                .background(Color.orange.opacity(0.12))
                .cornerRadius(8)
            }
            HStack {
                Toggle("Receiver enabled", isOn: Binding(
                    get: { model.config.receiverEnabled },
                    set: { model.setReceiverEnabled($0) }
                ))
                Spacer()
                Button("Restart listeners") { model.restart() }
                if model.connectedClient != nil {
                    Button("Disconnect") { model.disconnectClient() }
                }
            }
        }
    }

    private var pairing: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Pairing").font(.headline)
            if let code = model.pairingCode {
                Text(code)
                    .font(.system(size: 30, weight: .semibold, design: .monospaced))
                    .textSelection(.enabled)
                Text("Type this code on Windows, then press Pair there.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Button("Cancel pairing") { model.cancelPairing() }
            } else {
                Text("Paired PCs: " + (model.pairedDeviceNames.isEmpty
                    ? "none yet"
                    : model.pairedDeviceNames.joined(separator: ", ")))
                    .font(.callout)
                HStack {
                    Button("Show pairing code") { model.beginPairing() }
                    Button("Forget all paired PCs") { model.forgetDevices() }
                }
            }
        }
    }

    private var network: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Network").font(.headline)
            HStack {
                Text("TCP port").frame(width: 150, alignment: .leading)
                TextField("47821", value: model.binding(\.tcpPort), format: .number)
                    .frame(width: 90)
            }
            HStack {
                Text("UDP port").frame(width: 150, alignment: .leading)
                TextField("47822", value: model.binding(\.udpPort), format: .number)
                    .frame(width: 90)
            }
            HStack {
                Text("This Mac's name").frame(width: 150, alignment: .leading)
                TextField("Mac", text: model.binding(\.deviceName)).frame(width: 220)
            }
            Text("Both ports must be reachable from Windows on the local network.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private var pointer: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Pointer").font(.headline)
            HStack {
                Text("Speed").frame(width: 150, alignment: .leading)
                Slider(value: model.binding(\.pointerScale), in: 0.25...3, step: 0.05)
                    .frame(width: 200)
                Text(String(format: "%.2fx", model.config.pointerScale))
                    .font(.system(.body, design: .monospaced))
            }
            Text("Windows sends raw device counts, so this is the only pointer scaling applied - there is no double acceleration.")
                .font(.caption).foregroundStyle(.secondary)
            HStack {
                Text("Event scheduling").frame(width: 150, alignment: .leading)
                Picker("", selection: model.binding(\.schedulerMode)) {
                    ForEach(SchedulerMode.allCases) { mode in Text(mode.label).tag(mode) }
                }
                .labelsHidden()
                .frame(width: 220)
            }
            if model.config.schedulerMode == .smoothed {
                HStack {
                    Text("Smoothing").frame(width: 150, alignment: .leading)
                    Slider(value: model.binding(\.smoothingMs), in: 0...40, step: 1)
                        .frame(width: 200)
                    Text(String(format: "%.0f ms", model.config.smoothingMs))
                        .font(.system(.body, design: .monospaced))
                }
                Text("Absorbs about this much network jitter, and adds about this much latency. Lower it on Ethernet, raise it on a busy Wi-Fi link.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            if model.config.schedulerMode == .coalesced || model.config.schedulerMode == .smoothed {
                HStack {
                    Text("Min event interval").frame(width: 150, alignment: .leading)
                    Slider(value: model.binding(\.minEventIntervalMs), in: 0...8, step: 0.5)
                        .frame(width: 200)
                    Text(String(format: "%.1f ms", model.config.minEventIntervalMs))
                        .font(.system(.body, design: .monospaced))
                }
            }
            if model.config.schedulerMode == .paced {
                HStack {
                    Text("Pace").frame(width: 150, alignment: .leading)
                    TextField("0 = display refresh", value: model.binding(\.pacedRateHz), format: .number)
                        .frame(width: 90)
                    Text("Hz").foregroundStyle(.secondary)
                }
            }
        }
    }

    private var scrolling: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Scrolling").font(.headline)
            HStack {
                Text("Mode").frame(width: 150, alignment: .leading)
                Picker("", selection: model.binding(\.scrollMode)) {
                    ForEach(ScrollMode.allCases) { mode in Text(mode.label).tag(mode) }
                }
                .labelsHidden()
                .frame(width: 200)
            }
            HStack {
                Text("Lines per notch").frame(width: 150, alignment: .leading)
                TextField("3", value: model.binding(\.scrollLinesPerNotch), format: .number)
                    .frame(width: 70)
                if model.config.scrollMode == .pixel {
                    Text("Pixels per line").padding(.leading, 12)
                    TextField("10", value: model.binding(\.scrollPixelsPerLine), format: .number)
                        .frame(width: 70)
                }
            }
            Toggle("Natural scrolling (invert direction)", isOn: model.binding(\.naturalScrolling))
        }
    }

    private var modifiers: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Modifier mapping").font(.headline)
            modifierRow("Windows Ctrl", model.binding(\.modifiers.control))
            modifierRow("Windows Alt", model.binding(\.modifiers.alt))
            modifierRow("Windows key", model.binding(\.modifiers.gui))
            Text("Shift always maps to Shift. A keyboard switched into its own Mac mode already sends Command beside the space bar, and these defaults pass it through as Command — nothing here needs changing for that.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private func modifierRow(_ title: String, _ binding: Binding<ModifierRole>) -> some View {
        HStack {
            Text(title).frame(width: 150, alignment: .leading)
            Picker("", selection: binding) {
                ForEach(ModifierRole.allCases) { role in Text(role.label).tag(role) }
            }
            .labelsHidden()
            .frame(width: 160)
        }
    }

    private var behaviour: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Behaviour").font(.headline)
            Toggle("Hand input back when the cursor hits the right edge", isOn: model.binding(\.edgeSwitch))
            Toggle("Start at login", isOn: Binding(
                get: { model.config.startAtLogin },
                set: { model.applyStartAtLogin($0) }
            ))
            Toggle("Print the diagnostics line every second", isOn: model.binding(\.diagnostics))
            HStack {
                Text("Log level").frame(width: 150, alignment: .leading)
                Picker("", selection: model.binding(\.logLevel)) {
                    ForEach(LogLevel.allCases, id: \.rawValue) { level in
                        Text(level.name).tag(level.name)
                    }
                }
                .labelsHidden()
                .frame(width: 140)
            }
            HStack {
                Text("Heartbeat timeout").frame(width: 150, alignment: .leading)
                TextField("1000", value: model.binding(\.heartbeatTimeoutMs), format: .number)
                    .frame(width: 80)
                Text("ms").foregroundStyle(.secondary)
            }
            Text("Config file: \(Config.fileURL().path)")
                .font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
        }
    }
}

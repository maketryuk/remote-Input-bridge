import AppKit

struct Options {
    var beginPairing = false
    var showPermissionPrompt = true
    var logLevel: String?
}

/// Menu bar only: no dock icon, no main window (spec §7 "menu bar app").
final class AppDelegate: NSObject, NSApplicationDelegate {
    private var model: AppModel?
    private var menuBar: MenuBarController?
    private let options: Options

    init(options: Options) {
        self.options = options
        super.init()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        let model = AppModel()
        self.model = model
        menuBar = MenuBarController(model: model)
        if let level = options.logLevel {
            Log.shared.setLevel(LogLevel.named(level))
        }

        Log.info("Remote Input Bridge receiver starting")
        // Start listening first: the permission dialog is modal, and a receiver that only opens
        // its ports after someone clicks a button is a receiver that looks broken.
        model.start()
        if !Permissions.canPostEvents {
            Log.warn("Accessibility permission missing: input cannot be injected yet")
            Permissions.request()
            if options.showPermissionPrompt {
                DispatchQueue.main.async { [weak self] in self?.presentPermissionAlert() }
            }
        }
        if options.beginPairing {
            model.beginPairing()
            // Printed rather than only shown in the UI so a headless or scripted setup works.
            print("PAIRING CODE: \(model.pairingCode ?? "?")")
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        model?.stop(reason: "receiver quitting")
    }

    /// Spec §23: an explicit, actionable message instead of a silently dead cursor.
    private func presentPermissionAlert() {
        let alert = NSAlert()
        alert.messageText = "Accessibility permission required"
        alert.informativeText = Permissions.explanation
        alert.addButton(withTitle: "Open System Settings")
        alert.addButton(withTitle: "Later")
        if alert.runModal() == .alertFirstButtonReturn {
            Permissions.openSystemSettings()
        }
    }
}

/// Same vectors as the Rust `matches_reference_vectors` test and
/// `scripts/test-sender.py --vectors`. Any divergence in the key schedule would otherwise show up
/// only as an unexplained authentication failure at pairing time.
func runCryptoSelfTest() -> Int32 {
    var failures = 0
    func check(_ label: String, _ actual: String, _ expected: String) {
        let ok = actual == expected
        if !ok { failures += 1 }
        print("\(ok ? "ok  " : "FAIL") \(label)\n     got \(actual)\n     want \(expected)")
    }
    let pairing = Crypto.pairingKey(code: "A4C9-K2MN")
    check("pairing_key", pairing.hexString,
          "dfbd3be82b70e4ca211992bb8b397a876d6ef9e33ac61bfd690f235d6ff53886")
    check("wrap_mask", Crypto.wrapMask(pairingKey: pairing).hexString,
          "a9a0f1a7ea6572f1388a07b725b20cb0923bf09bc19880a1b8f034afd8f13fcf")
    let deviceKey = Data((0..<32).map { UInt8($0) })
    let keys = Crypto.sessionKeys(
        deviceKey: deviceKey,
        clientNonce: Data("client-nonce".utf8),
        serverNonce: Data("server-nonce".utf8)
    )
    check("session_tcp", keys.tcp.hexString,
          "6400cb4f1e4490fd1cec12c08a9c7d56be1420fc3dbcea0940bb54716434f8da")
    check("session_udp", keys.udp.hexString,
          "5461f982d118dead4af8ed61c969bb1c65e7e5db4c92e5f5dc2abc17224aa8ce")
    check("hmac", Crypto.hmac(key: Data(repeating: 1, count: 32), data: Data("abc".utf8)).hexString,
          "73860612aa6aadf68985b6e9c4233357cedd5f24a221eba740b2aa8350276cfc")

    // Framing and packet-parsing round trips.
    let udpKey = Data(repeating: 7, count: 32)
    var packet = Data()
    packet.append(bigEndian: Proto.udpMagic)
    packet.append(Proto.udpVersion)
    packet.append(Proto.udpTypeMouseMove)
    packet.append(bigEndian: UInt64(0x0102_0304_0506_0708))
    packet.append(bigEndian: UInt64(42))
    packet.append(bigEndian: UInt64(1_000_000))
    packet.append(bigEndian: Int32(-5))
    packet.append(bigEndian: Int32(9))
    packet.append(Crypto.hmac(key: udpKey, data: packet).prefix(8))
    if let parsed = Proto.MouseMovePacket.parse(packet, udpKey: udpKey),
       parsed.totalX == -5, parsed.totalY == 9, parsed.sequence == 42 {
        print("ok   udp packet parse")
    } else {
        failures += 1
        print("FAIL udp packet parse")
    }
    var tampered = packet
    tampered[tampered.count - 1] ^= 0xFF
    if Proto.MouseMovePacket.parse(tampered, udpKey: udpKey) == nil {
        print("ok   forged udp packet rejected")
    } else {
        failures += 1
        print("FAIL forged udp packet accepted")
    }
    if wrappingDelta(Int32.min, Int32.max) == 1 {
        print("ok   cumulative total wraps")
    } else {
        failures += 1
        print("FAIL cumulative total wrap")
    }
    print(failures == 0 ? "\nall self tests passed" : "\n\(failures) self test(s) failed")
    return failures == 0 ? 0 : 1
}

if CommandLine.arguments.contains("--crypto-selftest") {
    exit(runCryptoSelfTest())
}

func parseOptions() -> Options {
    var options = Options()
    var arguments = Array(CommandLine.arguments.dropFirst()).makeIterator()
    while let argument = arguments.next() {
        switch argument {
        case "--begin-pairing":
            options.beginPairing = true
        case "--no-prompt":
            options.showPermissionPrompt = false
        case "--log":
            options.logLevel = arguments.next()
        case "-h", "--help":
            print("""
                Remote Input Bridge receiver

                USAGE: RemoteInputBridge [OPTIONS]

                OPTIONS:
                  --begin-pairing     enable pairing mode at startup and print the code
                  --no-prompt         do not open the Accessibility alert on launch
                  --log <level>       ERROR | WARN | INFO | DEBUG | TRACE
                  --crypto-selftest   verify the key schedule against the reference vectors
                  -h, --help          print this help

                Settings live in \(Config.fileURL().path)
                """)
            exit(0)
        default:
            FileHandle.standardError.write(Data("unknown argument: \(argument)\n".utf8))
            exit(2)
        }
    }
    return options
}

// Line buffering so logs stream when stdout is a pipe or a file, not just a terminal.
setvbuf(stdout, nil, _IOLBF, 0)

let application = NSApplication.shared
let delegate = AppDelegate(options: parseOptions())
application.delegate = delegate
application.setActivationPolicy(.accessory)
application.run()

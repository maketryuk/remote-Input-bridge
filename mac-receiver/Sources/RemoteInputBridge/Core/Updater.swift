import AppKit
import Combine
import CryptoKit

/// Self-update for the receiver, reading the same `latest.json` the Windows sender reads so the
/// two halves are always offered the same release.
///
/// The bundle is replaced in place by a detached shell that waits for this process to exit: a
/// running app cannot reliably overwrite itself, and the alternative - an updater helper app -
/// would need its own signature and its own permission grants.
///
/// What leaves the machine: an HTTPS GET, then a download. No identifiers, no configuration, no
/// host name. Integrity comes from TLS to github.com plus the digest in the manifest, which catches
/// a corrupted or swapped asset but is not a substitute for a code signature.
///
/// One caveat this cannot fix on its own: macOS ties the Accessibility grant to the bundle's
/// signature. An unsigned or ad-hoc-signed release gets a new identity every build, so the grant
/// has to be given again after each update. Releases built with a stable signing identity keep it
/// (see README).
@MainActor
final class Updater: ObservableObject {
    static let shared = Updater()

    enum Stage: Equatable {
        case idle
        case checking
        case upToDate
        case available(String)
        case downloading(Double?)
        case installing
        case failed(String)
    }

    @Published private(set) var stage: Stage = .idle

    /// Kept in one place so a fork only has to change this line. Must match `MANIFEST_URL` in the
    /// Windows sender's `update.rs`.
    static let manifestURL = URL(
        string: "https://github.com/maketryuk/remote-Input-bridge/releases/latest/download/latest.json"
    )!

    static var currentVersion: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "0.0.0"
    }

    private static let checkInterval: TimeInterval = 24 * 60 * 60
    private static let firstCheckDelay: TimeInterval = 20
    private static let maxManifestBytes = 64 * 1024
    private static let maxDownloadBytes: Int64 = 512 * 1024 * 1024

    private var pending: (version: String, artifact: Artifact)?
    private var busy = false
    private var timer: Timer?

    // MARK: - Manifest

    struct Manifest: Decodable {
        var version: String
        var notesURL: String?
        var windows: Artifact?
        var macos: Artifact?

        enum CodingKeys: String, CodingKey {
            case version
            case notesURL = "notes_url"
            case windows
            case macos
        }
    }

    struct Artifact: Decodable, Equatable {
        var url: String
        var sha256: String
        var size: Int64?
    }

    /// Dotted numeric comparison, matching `is_newer` on the Windows side. Anything unparseable
    /// counts as zero, so a malformed manifest can never look newer than the running build.
    static func isNewer(_ candidate: String, than current: String) -> Bool {
        func parts(_ text: String) -> [Int] {
            text.trimmingCharacters(in: .whitespaces)
                .drop(while: { $0 == "v" })
                .split(whereSeparator: { $0 == "." || $0 == "-" || $0 == "+" })
                .map { Int($0) ?? 0 }
        }
        let left = parts(candidate), right = parts(current)
        for index in 0..<max(left.count, right.count) {
            let a = index < left.count ? left[index] : 0
            let b = index < right.count ? right[index] : 0
            if a != b { return a > b }
        }
        return false
    }

    // MARK: - Presentation

    var summary: String {
        switch stage {
        case .idle: return "Version \(Self.currentVersion)"
        case .checking: return "Version \(Self.currentVersion) — checking for updates…"
        case .upToDate: return "Version \(Self.currentVersion) — up to date"
        case .available(let version):
            return "Version \(version) is available (you have \(Self.currentVersion))"
        case .downloading(let fraction):
            guard let fraction else { return "Downloading the update…" }
            return "Downloading the update… \(Int(fraction * 100))%"
        case .installing: return "Installing — the app will restart"
        case .failed(let reason): return "Update failed: \(reason)"
        }
    }

    var actionTitle: String {
        if case .available(let version) = stage { return "Install \(version)" }
        return "Check for Updates"
    }

    var actionEnabled: Bool {
        switch stage {
        case .checking, .downloading, .installing: return false
        default: return true
        }
    }

    var updateReady: Bool {
        if case .available = stage { return true }
        return false
    }

    /// One click to look, one click to install.
    func act() {
        if updateReady {
            install()
        } else {
            check(manual: true)
        }
    }

    // MARK: - Checking

    /// Starts the daily background check. Re-reads the setting each time, so switching it off takes
    /// effect without a restart.
    func startAutomaticChecks(isEnabled: @escaping () -> Bool) {
        timer?.invalidate()
        let tick = { [weak self] in
            guard isEnabled() else { return }
            self?.check(manual: false)
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.firstCheckDelay) { tick() }
        let timer = Timer(timeInterval: Self.checkInterval, repeats: true) { _ in
            Task { @MainActor in tick() }
        }
        RunLoop.main.add(timer, forMode: .common)
        self.timer = timer
    }

    func check(manual: Bool) {
        guard !busy else { return }
        busy = true
        stage = .checking
        var request = URLRequest(url: Self.manifestURL)
        // Version only: enough for a server-side error to be actionable, nothing that identifies
        // the machine.
        request.setValue(
            "RemoteInputBridge/\(Self.currentVersion) (macOS)",
            forHTTPHeaderField: "User-Agent"
        )
        request.timeoutInterval = 30
        // A cached manifest would report "up to date" for as long as the cache lived.
        request.cachePolicy = .reloadIgnoringLocalCacheData

        URLSession.shared.dataTask(with: request) { [weak self] data, response, error in
            Task { @MainActor in
                guard let self else { return }
                self.busy = false
                do {
                    self.finishCheck(try Self.parse(data: data, response: response, error: error))
                } catch {
                    let reason = (error as? Failure)?.message ?? error.localizedDescription
                    if manual {
                        Log.warn("update check failed: \(reason)")
                        self.stage = .failed(reason)
                    } else {
                        Log.info("background update check failed: \(reason)")
                        self.stage = .idle
                    }
                }
            }
        }.resume()
    }

    private struct Failure: Error {
        let message: String
    }

    private static func parse(data: Data?, response: URLResponse?, error: Error?) throws -> Manifest {
        if let error { throw Failure(message: error.localizedDescription) }
        if let http = response as? HTTPURLResponse, http.statusCode != 200 {
            throw Failure(message: "the server answered HTTP \(http.statusCode)")
        }
        guard let data, !data.isEmpty else { throw Failure(message: "the release manifest is empty") }
        guard data.count <= maxManifestBytes else {
            throw Failure(message: "the release manifest is implausibly large")
        }
        do {
            return try JSONDecoder().decode(Manifest.self, from: data)
        } catch {
            throw Failure(message: "the release manifest is not valid JSON")
        }
    }

    private func finishCheck(_ manifest: Manifest) {
        guard Self.isNewer(manifest.version, than: Self.currentVersion) else {
            pending = nil
            Log.info("no update: \(Self.currentVersion) is the latest release")
            stage = .upToDate
            return
        }
        guard let artifact = manifest.macos else {
            stage = .failed("release \(manifest.version) has no macOS build")
            return
        }
        guard artifact.sha256.count == 64,
              artifact.sha256.allSatisfy(\.isHexDigit) else {
            stage = .failed("the manifest carries no usable SHA-256 for the macOS build")
            return
        }
        guard let url = URL(string: artifact.url), url.scheme == "https" else {
            stage = .failed("the manifest points at a non-HTTPS download")
            return
        }
        if let notes = manifest.notesURL, !notes.isEmpty {
            Log.info("release notes: \(notes)")
        }
        pending = (manifest.version, artifact)
        Log.info("update available: \(manifest.version)")
        stage = .available(manifest.version)
    }

    // MARK: - Installing

    func install() {
        guard let pending, !busy, let url = URL(string: pending.artifact.url) else { return }
        let bundle = Bundle.main.bundleURL
        // Checked before anything is downloaded: an app in a folder this user cannot write to has
        // to be replaced by hand, and finding that out after a 10 MB download is rude.
        guard FileManager.default.isWritableFile(atPath: bundle.deletingLastPathComponent().path) else {
            stage = .failed("\(bundle.deletingLastPathComponent().path) is not writable — reinstall by hand")
            return
        }
        busy = true
        stage = .downloading(nil)

        let expectedDigest = pending.artifact.sha256.lowercased()
        let expectedSize = pending.artifact.size
        var request = URLRequest(url: url)
        request.setValue(
            "RemoteInputBridge/\(Self.currentVersion) (macOS)",
            forHTTPHeaderField: "User-Agent"
        )
        request.timeoutInterval = 300

        let task = URLSession.shared.downloadTask(with: request) { [weak self] location, response, error in
            Task { @MainActor in
                guard let self else { return }
                self.busy = false
                do {
                    guard let location else {
                        throw Failure(message: error?.localizedDescription ?? "the download failed")
                    }
                    if let http = response as? HTTPURLResponse, http.statusCode != 200 {
                        throw Failure(message: "the server answered HTTP \(http.statusCode)")
                    }
                    try self.replaceBundle(
                        downloadedTo: location,
                        expectedDigest: expectedDigest,
                        expectedSize: expectedSize
                    )
                } catch {
                    let reason = (error as? Failure)?.message ?? error.localizedDescription
                    Log.warn("update failed: \(reason)")
                    self.stage = .failed(reason)
                }
            }
        }
        // The progress observation is dropped when the task finishes; holding it any longer would
        // keep reporting a percentage after the download is done.
        progressObservation = task.progress.observe(\.fractionCompleted) { [weak self] progress, _ in
            Task { @MainActor in
                guard let self, case .downloading = self.stage else { return }
                self.stage = .downloading(progress.fractionCompleted)
            }
        }
        task.resume()
    }

    private var progressObservation: NSKeyValueObservation?

    private func replaceBundle(
        downloadedTo location: URL,
        expectedDigest: String,
        expectedSize: Int64?
    ) throws {
        let work = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("RemoteInputBridge-update-\(ProcessInfo.processInfo.processIdentifier)")
        try? FileManager.default.removeItem(at: work)
        try FileManager.default.createDirectory(at: work, withIntermediateDirectories: true)

        let archive = work.appendingPathComponent("update.zip")
        try FileManager.default.moveItem(at: location, to: archive)

        let attributes = try FileManager.default.attributesOfItem(atPath: archive.path)
        let size = (attributes[.size] as? NSNumber)?.int64Value ?? 0
        guard size > 0, size <= Self.maxDownloadBytes else {
            throw Failure(message: "the download is \(size) bytes, which is not plausible")
        }
        if let expectedSize, expectedSize > 0, expectedSize != size {
            throw Failure(message: "the download is \(size) bytes, the manifest promised \(expectedSize)")
        }
        let digest = try Self.sha256(of: archive)
        guard digest == expectedDigest else {
            try? FileManager.default.removeItem(at: work)
            throw Failure(message: "the download does not match the manifest digest; it was discarded")
        }
        Log.info("verified \(archive.lastPathComponent) (\(digest))")

        try run("/usr/bin/ditto", ["-x", "-k", archive.path, work.path])
        let unpacked = try FileManager.default
            .contentsOfDirectory(at: work, includingPropertiesForKeys: nil)
            .first { $0.pathExtension == "app" }
        guard let unpacked else { throw Failure(message: "the archive contains no .app bundle") }

        // Refuse anything that is not this application, however it got into the archive.
        let identifier = Bundle(url: unpacked)?.bundleIdentifier
        guard identifier == Bundle.main.bundleIdentifier else {
            throw Failure(message: "the download is \(identifier ?? "an unknown app"), not this app")
        }

        // Nothing here sets the quarantine flag - URLSession is not a quarantining downloader - but
        // an archive built elsewhere can carry stale attributes, and a quarantined bundle would be
        // refused at launch with a message about damaged software.
        try? run("/usr/bin/xattr", ["-dr", "com.apple.quarantine", unpacked.path])

        stage = .installing
        try handOver(newBundle: unpacked, work: work)
    }

    /// Swaps the bundles from a detached shell, after this process is gone.
    private func handOver(newBundle: URL, work: URL) throws {
        let target = Bundle.main.bundleURL
        let script = work.appendingPathComponent("install.sh")
        // `ditto` rather than `mv` so ownership and extended attributes come out right, and the old
        // bundle is kept until the new one is in place.
        let body = """
        #!/bin/bash
        set -u
        target=\(shellQuote(target.path))
        staged=\(shellQuote(newBundle.path))
        work=\(shellQuote(work.path))
        # Wait for the running app to exit; give up waiting after 30 s and go ahead anyway.
        for _ in $(seq 1 300); do
            pgrep -f "$target/Contents/MacOS/" >/dev/null 2>&1 || break
            sleep 0.1
        done
        backup="$work/previous.app"
        if [ -d "$target" ] && ! mv "$target" "$backup"; then
            open "$target" 2>/dev/null || true
            exit 1
        fi
        if ditto "$staged" "$target"; then
            rm -rf "$backup"
        else
            # Put back what was working rather than leaving nothing installed.
            rm -rf "$target"
            [ -d "$backup" ] && mv "$backup" "$target"
        fi
        open "$target"
        rm -rf "$work"
        """
        try body.write(to: script, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: script.path)

        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/bash")
        process.arguments = [script.path]
        try process.run()

        Log.info("handing over to the installer script")
        // Through the app delegate so the receiver closes its ports and releases every held key
        // before the bundle is replaced.
        NSApp.terminate(nil)
    }

    private func shellQuote(_ text: String) -> String {
        "'" + text.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    private func run(_ executable: String, _ arguments: [String]) throws {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: executable)
        process.arguments = arguments
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        try process.run()
        process.waitUntilExit()
        guard process.terminationStatus == 0 else {
            throw Failure(message: "\(executable) failed with status \(process.terminationStatus)")
        }
    }

    /// Streamed rather than read into memory: a release archive is tens of megabytes, and there is
    /// no reason for all of it to be resident at once.
    private static func sha256(of file: URL) throws -> String {
        let handle = try FileHandle(forReadingFrom: file)
        defer { try? handle.close() }
        var hasher = SHA256()
        while let chunk = try handle.read(upToCount: 1024 * 1024), !chunk.isEmpty {
            hasher.update(data: chunk)
        }
        return hasher.finalize().map { String(format: "%02x", $0) }.joined()
    }
}

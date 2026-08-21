import AppKit
import Foundation

/// Decouples "a datagram arrived" from "post a CGEvent" (spec §33).
///
/// Datagrams do not arrive on a perfect cadence: Wi-Fi delivers them in bursts. Feeding a burst
/// straight into `CGEventPost` produces exactly the stutter this project exists to remove, so the
/// scheduler keeps only the *newest* accumulated delta and posts at most one event per interval.
/// Because movement is cumulative, collapsing a burst loses no distance at all - it just removes
/// the intermediate positions the user could never have seen anyway.
final class EventScheduler {
    private let injector: InputInjector
    private let telemetry: Telemetry
    private let condition = NSCondition()

    private var thread: Thread?
    private var stopped = false
    /// Bumped on every stop so a thread that has not woken up yet retires instead of racing the
    /// thread a subsequent start() created.
    private var generation: UInt64 = 0
    private var pendingX: Int32 = 0
    private var pendingY: Int32 = 0
    private var hasPending = false
    private var mode: SchedulerMode = .coalesced
    private var intervalNanos: UInt64 = 1_000_000
    private var lastPostNanos: UInt64 = 0

    /// Invoked when the cursor is pinned against the right edge of the rightmost display.
    var onRightEdge: (() -> Void)?

    init(injector: InputInjector, telemetry: Telemetry) {
        self.injector = injector
        self.telemetry = telemetry
    }

    func apply(config: Config) {
        let interval: Double
        switch config.schedulerMode {
        case .immediate:
            interval = 0
        case .coalesced:
            interval = config.minEventIntervalMs / 1000.0
        case .paced:
            let hz = config.pacedRateHz > 0 ? config.pacedRateHz : Double(displayRefreshRate())
            interval = 1.0 / max(hz, 1)
        }
        condition.lock()
        mode = config.schedulerMode
        intervalNanos = UInt64(max(interval, 0) * 1_000_000_000)
        condition.unlock()
        Log.info(
            "event scheduler: \(config.schedulerMode.rawValue), "
                + String(format: "%.2f ms between events", interval * 1000)
        )
    }

    private func displayRefreshRate() -> Int {
        NSScreen.screens.map(\.maximumFramesPerSecond).max() ?? 60
    }

    func start() {
        condition.lock()
        stopped = false
        generation += 1
        let myGeneration = generation
        condition.unlock()
        guard thread == nil else { return }
        let thread = Thread { [weak self] in self?.loop(generation: myGeneration) }
        thread.name = "rib.event-scheduler"
        // Movement injection is the one thing that must never be late.
        thread.qualityOfService = .userInteractive
        thread.stackSize = 512 * 1024
        self.thread = thread
        thread.start()
    }

    func stop() {
        condition.lock()
        stopped = true
        generation += 1
        condition.broadcast()
        condition.unlock()
        thread = nil
    }

    /// Called from the realtime receive path. Never blocks on event posting.
    func submit(dx: Int32, dy: Int32) {
        if dx == 0 && dy == 0 { return }
        condition.lock()
        if mode == .immediate {
            condition.unlock()
            post(dx: dx, dy: dy)
            return
        }
        if hasPending {
            telemetry.countCoalesced()
        }
        pendingX = pendingX &+ dx
        pendingY = pendingY &+ dy
        hasPending = true
        condition.signal()
        condition.unlock()
    }

    /// Drops queued movement, e.g. when the target switches away mid-burst.
    func flushPending() {
        condition.lock()
        pendingX = 0
        pendingY = 0
        hasPending = false
        condition.unlock()
    }

    private func loop(generation myGeneration: UInt64) {
        while true {
            condition.lock()
            while !hasPending && !stopped && generation == myGeneration {
                condition.wait()
            }
            if stopped || generation != myGeneration {
                condition.unlock()
                return
            }
            // Respect the cadence, but never sleep when the last event was long ago: a slow hand
            // movement must not be delayed by the rate limiter.
            let interval = intervalNanos
            if interval > 0 {
                var now = DispatchTime.now().uptimeNanoseconds
                let deadline = lastPostNanos &+ interval
                while now < deadline && !stopped && generation == myGeneration {
                    let remaining = Double(deadline - now) / 1_000_000_000
                    condition.wait(until: Date(timeIntervalSinceNow: remaining))
                    now = DispatchTime.now().uptimeNanoseconds
                }
            }
            if stopped || generation != myGeneration {
                condition.unlock()
                return
            }
            let dx = pendingX
            let dy = pendingY
            pendingX = 0
            pendingY = 0
            hasPending = false
            lastPostNanos = DispatchTime.now().uptimeNanoseconds
            condition.unlock()

            post(dx: dx, dy: dy)
        }
    }

    private func post(dx: Int32, dy: Int32) {
        let result = injector.move(dx: dx, dy: dy)
        telemetry.countApplied()
        if result.pinnedRight, dx > 0 {
            onRightEdge?()
        }
    }
}

import AppKit
import Foundation

/// Decouples "a datagram arrived" from "post a CGEvent" (spec §33).
///
/// Datagrams do not arrive on a perfect cadence. Over Wi-Fi they arrive in clumps: measured on a
/// real link, a third of them land inside the same millisecond as their predecessor, with 15 ms of
/// jitter between clumps. Feeding that straight into `CGEventPost` produces exactly the tearing
/// this project exists to remove, because the cursor teleports by a clump's worth of distance and
/// then waits.
///
/// Two mechanisms deal with it:
///
///  * **Coalescing** collapses a clump into one event. Because movement is cumulative, no distance
///    is lost - only the intermediate positions the user could never have seen anyway.
///  * **Smoothing** (the default) additionally spreads that distance over the next few
///    milliseconds instead of applying it in one jump, and keeps the cursor moving during the gap
///    that follows. It trades a few milliseconds of latency for uniform motion, which is the
///    trade the specification asks for: smoothness first, absolute latency fourth.
final class EventScheduler {
    private let injector: InputInjector
    private let telemetry: Telemetry
    private let condition = NSCondition()

    private var thread: Thread?
    private var stopped = false
    /// Bumped on every stop so a thread that has not woken up yet retires instead of racing the
    /// thread a subsequent start() created.
    private var generation: UInt64 = 0

    /// Movement that arrived but has not been posted yet.
    private var pendingX: Int32 = 0
    private var pendingY: Int32 = 0
    private var hasPending = false
    /// Movement accepted for playout, carried at sub-pixel precision.
    private var remainingX: Double = 0
    private var remainingY: Double = 0

    private var mode: SchedulerMode = .smoothed
    private var intervalNanos: UInt64 = 1_000_000
    private var smoothingSeconds: Double = 0.010
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
        case .smoothed:
            // Fine-grained playout: the smoothing window, not the tick, decides the feel.
            interval = max(config.minEventIntervalMs, 1) / 1000.0
        }
        condition.lock()
        mode = config.schedulerMode
        intervalNanos = UInt64(max(interval, 0) * 1_000_000_000)
        smoothingSeconds = max(config.smoothingMs, 0) / 1000.0
        condition.unlock()
        Log.info(
            "event scheduler: \(config.schedulerMode.rawValue), "
                + String(format: "tick %.2f ms", interval * 1000)
                + (config.schedulerMode == .smoothed
                    ? String(format: ", smoothing %.1f ms", config.smoothingMs) : "")
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
        remainingX = 0
        remainingY = 0
        condition.unlock()
    }

    private func loop(generation myGeneration: UInt64) {
        while true {
            condition.lock()
            while !hasPending && !hasResidualLocked && !stopped && generation == myGeneration {
                condition.wait()
            }
            if stopped || generation != myGeneration {
                condition.unlock()
                return
            }
            acceptPendingLocked()

            // Respect the cadence, but never sleep when the last event was long ago: a slow hand
            // movement must not be delayed by the rate limiter.
            let interval = intervalNanos
            if interval > 0 {
                var now = DispatchTime.now().uptimeNanoseconds
                let deadline = lastPostNanos &+ interval
                while now < deadline && !stopped && generation == myGeneration {
                    let waitSeconds = Double(deadline - now) / 1_000_000_000
                    condition.wait(until: Date(timeIntervalSinceNow: waitSeconds))
                    // Anything that arrived while waiting joins this step - that is the
                    // coalescing.
                    acceptPendingLocked()
                    now = DispatchTime.now().uptimeNanoseconds
                }
            }
            if stopped || generation != myGeneration {
                condition.unlock()
                return
            }
            let step = takeStepLocked()
            lastPostNanos = DispatchTime.now().uptimeNanoseconds
            condition.unlock()

            if step.dx != 0 || step.dy != 0 {
                post(dx: step.dx, dy: step.dy)
            }
        }
    }

    private var hasResidualLocked: Bool {
        abs(remainingX) >= 0.5 || abs(remainingY) >= 0.5
    }

    private func acceptPendingLocked() {
        guard hasPending else { return }
        remainingX += Double(pendingX)
        remainingY += Double(pendingY)
        pendingX = 0
        pendingY = 0
        hasPending = false
    }

    /// How much of the outstanding movement to post now. Sub-pixel remainders are carried, so
    /// nothing is ever rounded away, and a leftover of one pixel or more always makes progress -
    /// the cursor can never stall short of where the sender says it should be.
    private func takeStepLocked() -> (dx: Int32, dy: Int32) {
        func consumeAll(_ value: inout Double) -> Int32 {
            let step = value.rounded()
            value -= step
            return Int32(clamping: Int(step))
        }
        guard mode == .smoothed else {
            return (consumeAll(&remainingX), consumeAll(&remainingY))
        }
        let tick = Double(intervalNanos) / 1_000_000_000
        let alpha = min(1.0, max(tick, 0.0005) / max(smoothingSeconds, max(tick, 0.0005)))
        func consumeFraction(_ value: inout Double) -> Int32 {
            var step = (value * alpha).rounded()
            if step == 0, abs(value) >= 1 {
                step = value > 0 ? 1 : -1
            }
            value -= step
            return Int32(clamping: Int(step))
        }
        return (consumeFraction(&remainingX), consumeFraction(&remainingY))
    }

    private func post(dx: Int32, dy: Int32) {
        let result = injector.move(dx: dx, dy: dy)
        telemetry.countApplied()
        if result.pinnedRight, dx > 0 {
            onRightEdge?()
        }
    }
}

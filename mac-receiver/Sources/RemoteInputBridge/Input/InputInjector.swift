import AppKit
import CoreGraphics

/// Turns protocol events into native macOS events via CoreGraphics (spec §22).
///
/// Design notes:
///  * The receiver owns a *virtual* cursor position and posts absolute moves with the relative
///    delta attached (spec §56). Absolute posting is what keeps the Mac cursor from drifting
///    after a lost datagram, while the delta fields keep games and pointer-lock apps happy.
///  * No pointer acceleration is applied here beyond a linear scale: Windows already sends raw
///    device counts, so anything else would be double acceleration (spec §55).
///  * Every press is tracked so `releaseAll` can put the machine back to a clean state on
///    disconnect, target switch or heartbeat timeout (spec §51).
final class InputInjector {
    private let lock = NSLock()
    private let source = CGEventSource(stateID: .hidSystemState)

    private var pressedUsages: Set<UInt16> = []
    private var pressedButtons: Set<UInt8> = []
    private var flags: CGEventFlags = []
    private var modifierMap = ModifierMapping.default
    private var pointerScale: Double = 1.0
    private var scrollConfig = ScrollSettings()

    private var virtualPosition = CGPoint.zero
    private var displays: [CGRect] = []

    /// Fractional remainder so a 0.5x scale does not silently discard every other count.
    private var residualX: Double = 0
    private var residualY: Double = 0

    /// Set once we have proof that posted events are being dropped by the system.
    private var injectionVerified = false
    private var injectionWarned = false
    private var postsSinceCheck = 0
    private var frozenSamples = 0
    private var lastActualPosition = CGPoint.zero
    private var lastExpectedPosition = CGPoint.zero

    private var lastClickTime: [UInt8: TimeInterval] = [:]
    private var lastClickPosition: [UInt8: CGPoint] = [:]
    private var clickCounts: [UInt8: Int64] = [:]

    struct ScrollSettings {
        var mode: ScrollMode = .pixel
        var linesPerNotch: Double = 3
        var pixelsPerLine: Double = 10
        var natural = false
    }

    init() {
        refreshDisplays()
        virtualPosition = currentCursorPosition()
    }

    // MARK: - Configuration

    func apply(config: Config) {
        lock.lock()
        modifierMap = config.modifiers
        pointerScale = config.pointerScale
        scrollConfig = ScrollSettings(
            mode: config.scrollMode,
            linesPerNotch: config.scrollLinesPerNotch,
            pixelsPerLine: config.scrollPixelsPerLine,
            natural: config.naturalScrolling
        )
        lock.unlock()
    }

    func refreshDisplays() {
        var rects: [CGRect] = []
        var count: UInt32 = 0
        if CGGetActiveDisplayList(0, nil, &count) == .success, count > 0 {
            var ids = [CGDirectDisplayID](repeating: 0, count: Int(count))
            if CGGetActiveDisplayList(count, &ids, &count) == .success {
                rects = ids.prefix(Int(count)).map { CGDisplayBounds($0) }
            }
        }
        if rects.isEmpty {
            rects = [CGRect(x: 0, y: 0, width: 1920, height: 1080)]
        }
        lock.lock()
        displays = rects
        lock.unlock()
    }

    /// Re-anchors the virtual cursor on the real one. Called whenever Windows hands over the
    /// input, so the pointer continues from wherever the user left it on the Mac.
    func rebaseCursor() {
        let position = currentCursorPosition()
        lock.lock()
        virtualPosition = position
        residualX = 0
        residualY = 0
        lock.unlock()
    }

    private func currentCursorPosition() -> CGPoint {
        CGEvent(source: nil)?.location ?? .zero
    }

    // MARK: - Movement

    /// Applies a raw delta and returns the resulting position plus whether the cursor is pinned
    /// against the right edge (used for edge switching).
    @discardableResult
    func move(dx: Int32, dy: Int32) -> (position: CGPoint, pinnedRight: Bool) {
        lock.lock()
        let scaledX = Double(dx) * pointerScale + residualX
        let scaledY = Double(dy) * pointerScale + residualY
        let stepX = scaledX.rounded(.towardZero)
        let stepY = scaledY.rounded(.towardZero)
        residualX = scaledX - stepX
        residualY = scaledY - stepY

        let requested = CGPoint(x: virtualPosition.x + stepX, y: virtualPosition.y + stepY)
        let clamped = clampToDisplays(requested)
        let pinnedRight = requested.x > clamped.x + 0.5
        virtualPosition = clamped
        let deltaX = Int64(stepX)
        let deltaY = Int64(stepY)
        let type = dragTypeLocked() ?? .mouseMoved
        let button = dragButtonLocked()
        let currentFlags = flags
        lock.unlock()

        guard let event = CGEvent(
            mouseEventSource: source,
            mouseType: type,
            mouseCursorPosition: clamped,
            mouseButton: button
        ) else {
            return (clamped, pinnedRight)
        }
        // Apps that read deltas (games, 3D viewports) need these even for an absolute move.
        event.setIntegerValueField(.mouseEventDeltaX, value: deltaX)
        event.setIntegerValueField(.mouseEventDeltaY, value: deltaY)
        event.flags = currentFlags
        event.post(tap: .cghidEventTap)
        verifyInjection(expected: clamped)
        return (clamped, pinnedRight)
    }

    /// Posting a CGEvent without Accessibility permission fails *silently*: no error, no
    /// exception, just a cursor that never moves. This turns that into an actionable log line.
    ///
    /// The signal is a *frozen* cursor, not a lagging one: the real pointer updates a fraction of
    /// a millisecond behind the post, so any position tolerance would produce false alarms at
    /// several hundred events per second. A cursor that reads back byte-identical across two
    /// samples while we asked it to travel a long way has genuinely not moved.
    private func verifyInjection(expected: CGPoint) {
        lock.lock()
        if injectionVerified || injectionWarned {
            lock.unlock()
            return
        }
        postsSinceCheck += 1
        guard postsSinceCheck >= 30 else {
            lock.unlock()
            return
        }
        postsSinceCheck = 0
        let previousActual = lastActualPosition
        let previousExpected = lastExpectedPosition
        lock.unlock()

        let actual = currentCursorPosition()
        let requestedTravel = abs(expected.x - previousExpected.x) + abs(expected.y - previousExpected.y)
        let observedTravel = abs(actual.x - previousActual.x) + abs(actual.y - previousActual.y)

        lock.lock()
        lastActualPosition = actual
        lastExpectedPosition = expected
        if previousExpected == .zero {
            lock.unlock()
            return // first sample only establishes the baseline
        }
        if observedTravel > 0 {
            injectionVerified = true
            lock.unlock()
            return
        }
        guard requestedTravel > 20 else {
            lock.unlock()
            return // not enough movement asked for to conclude anything
        }
        frozenSamples += 1
        let warn = frozenSamples >= 2
        if warn { injectionWarned = true }
        lock.unlock()

        if warn {
            Log.error("""
                the cursor is not moving even though \(30 * 2) mouse events were posted \
                (asked for \(Int(requestedTravel)) points of travel). This is exactly what a \
                missing Accessibility permission looks like: CGEventPost fails silently. Grant it \
                to this app bundle in System Settings > Privacy & Security > Accessibility, then \
                relaunch the receiver.
                """)
        }
    }

    private func dragTypeLocked() -> CGEventType? {
        if pressedButtons.contains(Proto.Button.left.rawValue) { return .leftMouseDragged }
        if pressedButtons.contains(Proto.Button.right.rawValue) { return .rightMouseDragged }
        if !pressedButtons.isEmpty { return .otherMouseDragged }
        return nil
    }

    private func dragButtonLocked() -> CGMouseButton {
        if pressedButtons.contains(Proto.Button.left.rawValue) { return .left }
        if pressedButtons.contains(Proto.Button.right.rawValue) { return .right }
        if pressedButtons.contains(Proto.Button.middle.rawValue) { return .center }
        return .left
    }

    /// Keeps the cursor inside a real display. Clamping to the bounding box of all displays
    /// would let it sit in the empty gap between two differently sized screens.
    private func clampToDisplays(_ point: CGPoint) -> CGPoint {
        if displays.contains(where: { $0.contains(point) }) { return point }
        var best = point
        var bestDistance = Double.greatestFiniteMagnitude
        for rect in displays {
            let x = min(max(point.x, rect.minX), rect.maxX - 1)
            let y = min(max(point.y, rect.minY), rect.maxY - 1)
            let candidate = CGPoint(x: x, y: y)
            let distance = (candidate.x - point.x) * (candidate.x - point.x)
                + (candidate.y - point.y) * (candidate.y - point.y)
            if distance < bestDistance {
                bestDistance = distance
                best = candidate
            }
        }
        return best
    }

    // MARK: - Buttons

    func mouseButton(_ raw: UInt8, down: Bool) {
        guard let button = Proto.Button(rawValue: raw) else { return }
        lock.lock()
        if down {
            pressedButtons.insert(raw)
        } else if pressedButtons.remove(raw) == nil {
            // A release for a button we never saw pressed: dropping it is what prevents a
            // phantom click after a reconnect.
            lock.unlock()
            return
        }
        let position = virtualPosition
        let currentFlags = flags
        let clickCount = updateClickCountLocked(raw, down: down, at: position)
        lock.unlock()

        let (type, cgButton): (CGEventType, CGMouseButton)
        switch button {
        case .left: (type, cgButton) = (down ? .leftMouseDown : .leftMouseUp, .left)
        case .right: (type, cgButton) = (down ? .rightMouseDown : .rightMouseUp, .right)
        case .middle: (type, cgButton) = (down ? .otherMouseDown : .otherMouseUp, .center)
        case .back: (type, cgButton) = (down ? .otherMouseDown : .otherMouseUp, CGMouseButton(rawValue: 3)!)
        case .forward: (type, cgButton) = (down ? .otherMouseDown : .otherMouseUp, CGMouseButton(rawValue: 4)!)
        }
        guard let event = CGEvent(
            mouseEventSource: source,
            mouseType: type,
            mouseCursorPosition: position,
            mouseButton: cgButton
        ) else { return }
        event.flags = currentFlags
        // Without a click count macOS never sees a double click.
        event.setIntegerValueField(.mouseEventClickState, value: clickCount)
        event.post(tap: .cghidEventTap)
    }

    private func updateClickCountLocked(_ button: UInt8, down: Bool, at position: CGPoint) -> Int64 {
        guard down else { return clickCounts[button] ?? 1 }
        let now = Date().timeIntervalSinceReferenceDate
        let interval = NSEvent.doubleClickInterval
        let previousTime = lastClickTime[button] ?? 0
        let previousPosition = lastClickPosition[button] ?? .zero
        let moved = abs(previousPosition.x - position.x) > 5 || abs(previousPosition.y - position.y) > 5
        let count = (now - previousTime <= interval && !moved) ? (clickCounts[button] ?? 1) + 1 : 1
        lastClickTime[button] = now
        lastClickPosition[button] = position
        clickCounts[button] = count
        return count
    }

    // MARK: - Scroll

    func scroll(unitsX: Int32, unitsY: Int32) {
        lock.lock()
        let settings = scrollConfig
        let currentFlags = flags
        lock.unlock()

        let notchesX = Double(unitsX) / 120.0
        let notchesY = Double(unitsY) / 120.0
        let sign: Double = settings.natural ? -1 : 1
        let event: CGEvent?
        switch settings.mode {
        case .pixel:
            let pixelsY = notchesY * settings.linesPerNotch * settings.pixelsPerLine * sign
            let pixelsX = notchesX * settings.linesPerNotch * settings.pixelsPerLine * sign
            event = CGEvent(
                scrollWheelEvent2Source: source,
                units: .pixel,
                wheelCount: 2,
                wheel1: Int32(pixelsY.rounded()),
                wheel2: Int32(pixelsX.rounded()),
                wheel3: 0
            )
            // Continuous scrolling is what makes it feel like a trackpad instead of a ratchet.
            event?.setIntegerValueField(.scrollWheelEventIsContinuous, value: 1)
        case .line:
            let linesY = notchesY * settings.linesPerNotch * sign
            let linesX = notchesX * settings.linesPerNotch * sign
            event = CGEvent(
                scrollWheelEvent2Source: source,
                units: .line,
                wheelCount: 2,
                wheel1: Int32(linesY.rounded()),
                wheel2: Int32(linesX.rounded()),
                wheel3: 0
            )
        }
        guard let event else { return }
        event.flags = currentFlags
        event.post(tap: .cghidEventTap)
    }

    // MARK: - Keyboard

    func key(usage: UInt16, down: Bool, repeatPress: Bool) {
        if let physical = Keymap.physicalModifier(for: usage) {
            modifierKey(usage: usage, physical: physical, down: down)
            return
        }
        guard let virtualKey = Keymap.virtualKey(for: usage) else {
            Log.debug(String(format: "no macOS key for HID usage 0x%02X", usage))
            return
        }
        lock.lock()
        if down {
            pressedUsages.insert(usage)
        } else if pressedUsages.remove(usage) == nil, !repeatPress {
            // Unmatched release (typically after a reconnect): ignore rather than emit a
            // keystroke the user never made.
            lock.unlock()
            return
        }
        let currentFlags = flags
        lock.unlock()

        guard let event = CGEvent(keyboardEventSource: source, virtualKey: virtualKey, keyDown: down)
        else { return }
        event.flags = currentFlags
        event.setIntegerValueField(.keyboardEventAutorepeat, value: repeatPress ? 1 : 0)
        event.post(tap: .cghidEventTap)
    }

    private func modifierKey(usage: UInt16, physical: Keymap.PhysicalModifier, down: Bool) {
        lock.lock()
        if down {
            pressedUsages.insert(usage)
        } else {
            pressedUsages.remove(usage)
        }
        guard let resolved = resolveModifierLocked(physical) else {
            lock.unlock()
            return
        }
        if down {
            flags.insert(resolved.flag)
        } else if !anyOtherUsageHoldsLocked(resolved.flag, excluding: usage) {
            flags.remove(resolved.flag)
        }
        let currentFlags = flags
        lock.unlock()

        // Posting the modifier's own key event is what makes macOS emit flagsChanged; setting
        // `flags` on it keeps the reported state consistent with what we think is held.
        guard let event = CGEvent(keyboardEventSource: source, virtualKey: resolved.key, keyDown: down)
        else { return }
        event.flags = currentFlags
        event.post(tap: .cghidEventTap)
    }

    private struct ResolvedModifier {
        var key: CGKeyCode
        var flag: CGEventFlags
    }

    private func resolveModifierLocked(_ physical: Keymap.PhysicalModifier) -> ResolvedModifier? {
        let role: ModifierRole
        let right: Bool
        switch physical {
        case let .control(isRight): role = modifierMap.control; right = isRight
        case let .alt(isRight): role = modifierMap.alt; right = isRight
        case let .gui(isRight): role = modifierMap.gui; right = isRight
        case let .shift(isRight):
            return ResolvedModifier(key: isRight ? 60 : 56, flag: .maskShift)
        }
        switch role {
        case .control: return ResolvedModifier(key: right ? 62 : 59, flag: .maskControl)
        case .option: return ResolvedModifier(key: right ? 61 : 58, flag: .maskAlternate)
        case .command: return ResolvedModifier(key: right ? 54 : 55, flag: .maskCommand)
        case .none: return nil
        }
    }

    /// With Ctrl and Win both mapped to Command, releasing one must not clear the flag while the
    /// other is still held.
    private func anyOtherUsageHoldsLocked(_ flag: CGEventFlags, excluding usage: UInt16) -> Bool {
        for held in pressedUsages where held != usage {
            if let physical = Keymap.physicalModifier(for: held),
               let resolved = resolveModifierLocked(physical),
               resolved.flag == flag {
                return true
            }
        }
        return false
    }

    // MARK: - Recovery

    /// Releases everything we believe is held. Idempotent, and safe to call from any thread.
    func releaseAll(reason: String) {
        lock.lock()
        let usages = pressedUsages
        let buttons = pressedButtons
        pressedUsages.removeAll()
        pressedButtons.removeAll()
        flags = []
        let position = virtualPosition
        lock.unlock()

        if usages.isEmpty && buttons.isEmpty { return }
        Log.info("releasing \(usages.count) key(s) and \(buttons.count) button(s): \(reason)")

        for usage in usages {
            if let physical = Keymap.physicalModifier(for: usage) {
                lock.lock()
                let resolved = resolveModifierLocked(physical)
                lock.unlock()
                if let resolved,
                   let event = CGEvent(keyboardEventSource: source, virtualKey: resolved.key, keyDown: false) {
                    event.flags = []
                    event.post(tap: .cghidEventTap)
                }
            } else if let virtualKey = Keymap.virtualKey(for: usage),
                      let event = CGEvent(keyboardEventSource: source, virtualKey: virtualKey, keyDown: false) {
                event.flags = []
                event.post(tap: .cghidEventTap)
            }
        }
        for raw in buttons {
            guard let button = Proto.Button(rawValue: raw) else { continue }
            let (type, cgButton): (CGEventType, CGMouseButton)
            switch button {
            case .left: (type, cgButton) = (.leftMouseUp, .left)
            case .right: (type, cgButton) = (.rightMouseUp, .right)
            case .middle: (type, cgButton) = (.otherMouseUp, .center)
            case .back: (type, cgButton) = (.otherMouseUp, CGMouseButton(rawValue: 3)!)
            case .forward: (type, cgButton) = (.otherMouseUp, CGMouseButton(rawValue: 4)!)
            }
            if let event = CGEvent(
                mouseEventSource: source,
                mouseType: type,
                mouseCursorPosition: position,
                mouseButton: cgButton
            ) {
                event.flags = []
                event.post(tap: .cghidEventTap)
            }
        }
    }

    /// Forces the modifier state to match what the sender says is physically held.
    func syncModifiers(_ mask: Proto.ModifierMask) {
        let wanted: [(UInt16, Bool)] = [
            (0xE0, mask.contains(.leftControl)),
            (0xE1, mask.contains(.leftShift)),
            (0xE2, mask.contains(.leftAlt)),
            (0xE3, mask.contains(.leftGUI)),
            (0xE4, mask.contains(.rightControl)),
            (0xE5, mask.contains(.rightShift)),
            (0xE6, mask.contains(.rightAlt)),
            (0xE7, mask.contains(.rightGUI)),
        ]
        for (usage, shouldBeDown) in wanted {
            lock.lock()
            let isDown = pressedUsages.contains(usage)
            lock.unlock()
            if isDown != shouldBeDown, let physical = Keymap.physicalModifier(for: usage) {
                modifierKey(usage: usage, physical: physical, down: shouldBeDown)
            }
        }
    }
}

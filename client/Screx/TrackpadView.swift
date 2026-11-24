import SwiftUI
import UIKit

/// On-screen trackpad overlay: a semi-transparent, movable, resizable surface that drives the
/// host's mouse with the same `MOUSE` messages a physical mouse uses. Because it sits above the
/// video view, touches inside it never reach `DisplayContainerView`, so no touch events are sent
/// for that area.
struct TrackpadOverlay: View {
    let containerSize: CGSize
    /// Keyboard bounds in this container's coordinates, or `.null` when hidden.
    let keyboardFrame: CGRect
    let onMove: (CGFloat, CGFloat) -> Void
    let onButton: (UInt8, Bool) -> Void
    let onScroll: (CGFloat, CGFloat) -> Void

    /// Trackpad frame, as fractions of the container so it survives rotation and size changes.
    @State private var normalizedFrame: CGRect = Self.loadFrame()
    @State private var dragOffset: CGSize = .zero
    @State private var resizeDelta: CGSize = .zero
    @State private var temporaryKeyboardFrame: CGRect?
    @State private var isSurfaceActive = false
    @GestureState private var isAdjusting = false
    @State private var isIdle = false
    @State private var idleTask: Task<Void, Never>?

    private static let minSize = CGSize(width: 180, height: 130)
    private static let handleSize: CGFloat = 44
    private static let gripHeight: CGFloat = 32

    var body: some View {
        let frame = currentFrame()
        let visualOpacity = isIdle ? 0.35 : 1.0
        ZStack(alignment: .top) {
            TrackpadSurface(
                onMove: onMove, onButton: onButton, onScroll: onScroll,
                onInteractionChanged: { isSurfaceActive = $0 }
            )
                .padding(.top, Self.gripHeight)

            // Grip: drag to move the trackpad.
            Capsule()
                .fill(Color.white.opacity(0.55 * visualOpacity))
                .frame(width: 44, height: 5)
                .frame(maxWidth: .infinity)
                .frame(height: Self.gripHeight)
                .contentShape(Rectangle())
                .gesture(
                    DragGesture(coordinateSpace: .global)
                        .updating($isAdjusting) { _, active, _ in active = true }
                        .onChanged { dragOffset = $0.translation }
                        .onEnded { value in
                            let base = baseFrame()
                            commit(origin: CGPoint(
                                x: base.minX + value.translation.width,
                                y: base.minY + value.translation.height
                            ), size: base.size)
                            dragOffset = .zero
                        }
                )
        }
        .frame(width: frame.width, height: frame.height)
        .background(Color.black.opacity(0.28 * visualOpacity), in: RoundedRectangle(cornerRadius: 16))
        .overlay(
            RoundedRectangle(cornerRadius: 16)
                .stroke(Color.white.opacity(0.35 * visualOpacity), lineWidth: 1)
                .allowsHitTesting(false)
        )
        .overlay(alignment: .bottomTrailing) {
            // Corner handle: drag to resize.
            Image(systemName: "arrow.up.left.and.arrow.down.right")
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(Color.white.opacity(0.7 * visualOpacity))
                .frame(width: Self.handleSize, height: Self.handleSize)
                .contentShape(Rectangle())
                .gesture(
                    DragGesture(coordinateSpace: .global)
                        .updating($isAdjusting) { _, active, _ in active = true }
                        .onChanged { resizeDelta = $0.translation }
                        .onEnded { value in
                            let base = baseFrame()
                            commit(origin: base.origin, size: CGSize(
                                width: base.width + value.translation.width,
                                height: base.height + value.translation.height
                            ))
                            resizeDelta = .zero
                        }
                )
        }
        .clipped()
        .allowsHitTesting(frame.height >= 80)
        // Fade only the decoration, never the interactive surface. SwiftUI can exclude
        // low-opacity views from hit testing, which would leave the faded pad unusable.
        .opacity(frame.height >= 80 ? 1 : 0)
        .animation(.easeInOut(duration: 0.25), value: isIdle)
        .position(x: frame.midX, y: frame.midY)
        .onAppear { updateActivity() }
        .onDisappear { idleTask?.cancel() }
        .onChange(of: isSurfaceActive) { _ in updateActivity() }
        .onChange(of: isAdjusting) { _ in updateActivity() }
        .onChange(of: keyboardFrame) { keyboard in
            if keyboard.intersection(CGRect(origin: .zero, size: containerSize)).isEmpty {
                temporaryKeyboardFrame = nil
            }
        }
    }

    private func updateActivity() {
        idleTask?.cancel()
        isIdle = false
        guard !isSurfaceActive && !isAdjusting else { return }
        idleTask = Task { @MainActor in
            do {
                try await Task.sleep(nanoseconds: 1_500_000_000)
                try Task.checkCancellation()
                isIdle = true
            } catch {
                // A new touch or removal of the overlay cancels the pending fade.
            }
        }
    }

    /// The on-screen frame, including any in-progress move or resize, clamped to the container.
    private func currentFrame() -> CGRect {
        let base = baseFrame()
        let moved = base.offsetBy(dx: dragOffset.width, dy: dragOffset.height)
        let resized = CGRect(
            origin: moved.origin,
            size: CGSize(width: moved.width + resizeDelta.width, height: moved.height + resizeDelta.height)
        )
        return TrackpadLayout.avoidingKeyboard(clamped(resized), in: containerSize, keyboard: keyboardFrame)
    }

    private func baseFrame() -> CGRect {
        TrackpadLayout.avoidingKeyboard(clamped(temporaryKeyboardFrame ?? CGRect(
            x: normalizedFrame.minX * containerSize.width,
            y: normalizedFrame.minY * containerSize.height,
            width: normalizedFrame.width * containerSize.width,
            height: normalizedFrame.height * containerSize.height
        )), in: containerSize, keyboard: keyboardFrame)
    }

    private func clamped(_ rect: CGRect) -> CGRect {
        guard containerSize.width > 0, containerSize.height > 0 else { return rect }
        return TrackpadLayout.clamp(rect, in: containerSize, minimum: Self.minSize)
    }

    private func commit(origin: CGPoint, size: CGSize) {
        guard containerSize.width > 0, containerSize.height > 0 else { return }
        let rect = clamped(CGRect(origin: origin, size: size))
        // Allow moving/resizing while typing, but don't overwrite the saved keyboard-free
        // layout with a temporary adjustment. Restore it after dismissal.
        if !keyboardFrame.intersection(CGRect(origin: .zero, size: containerSize)).isEmpty {
            temporaryKeyboardFrame = TrackpadLayout.avoidingKeyboard(rect, in: containerSize, keyboard: keyboardFrame)
            return
        }
        normalizedFrame = CGRect(
            x: rect.minX / containerSize.width,
            y: rect.minY / containerSize.height,
            width: rect.width / containerSize.width,
            height: rect.height / containerSize.height
        )
        Self.saveFrame(normalizedFrame)
    }

    // MARK: - Persistence

    private static let frameKey = "screx_trackpad_frame"

    private static func loadFrame() -> CGRect {
        if let values = UserDefaults.standard.array(forKey: frameKey) as? [Double],
           values.count == 4, values.allSatisfy({ $0.isFinite }), values[2] > 0, values[3] > 0 {
            return CGRect(x: values[0], y: values[1], width: values[2], height: values[3])
        }
        // Default: bottom-left corner, about a quarter of the screen wide.
        return CGRect(x: 0.03, y: 0.60, width: 0.28, height: 0.32)
    }

    private static func saveFrame(_ rect: CGRect) {
        UserDefaults.standard.set(
            [Double(rect.minX), Double(rect.minY), Double(rect.width), Double(rect.height)],
            forKey: frameKey
        )
    }
}

/// UIKit surface that turns touches into mouse actions.
struct TrackpadSurface: UIViewRepresentable {
    let onMove: (CGFloat, CGFloat) -> Void
    let onButton: (UInt8, Bool) -> Void
    let onScroll: (CGFloat, CGFloat) -> Void
    let onInteractionChanged: (Bool) -> Void

    func makeUIView(context: Context) -> TrackpadSurfaceView {
        let view = TrackpadSurfaceView()
        update(view)
        return view
    }

    func updateUIView(_ view: TrackpadSurfaceView, context: Context) {
        update(view)
    }

    static func dismantleUIView(_ view: TrackpadSurfaceView, coordinator: ()) {
        view.cancelInteraction()
    }

    private func update(_ view: TrackpadSurfaceView) {
        view.onMove = onMove
        view.onButton = onButton
        view.onScroll = onScroll
        view.onInteractionChanged = onInteractionChanged
    }
}

/// Gestures:
/// - one finger drag: move the pointer (with mild acceleration)
/// - tap: left click; two-finger tap: right click
/// - two-finger drag: scroll
/// - tap, then touch again and drag: hold the left button while dragging
final class TrackpadSurfaceView: UIView {
    var onMove: ((CGFloat, CGFloat) -> Void)?
    var onButton: ((UInt8, Bool) -> Void)?
    var onScroll: ((CGFloat, CGFloat) -> Void)?
    var onInteractionChanged: ((Bool) -> Void)?

    static let buttonLeft: UInt8 = 0x00
    static let buttonRight: UInt8 = 0x01

    private static let tapMaxDuration: TimeInterval = 0.25
    private static let tapMaxMovement: CGFloat = 8
    private static let dragArmWindow: TimeInterval = 0.3
    private static let baseGain: CGFloat = 1.4

    private var activeTouches: [UITouch] = []
    private var gestureStart: TimeInterval = 0
    private var maxFingers = 0
    private var movement: CGFloat = 0
    private var lastSingleLocation: CGPoint?
    private var lastTwoFingerLocation: CGPoint?
    private var lastTapEnd: TimeInterval = 0
    private var dragArmed = false
    private var dragging = false
    private var wasCancelled = false

    override init(frame: CGRect) {
        super.init(frame: frame)
        isMultipleTouchEnabled = true
        backgroundColor = .clear
    }

    required init?(coder: NSCoder) {
        super.init(coder: coder)
        isMultipleTouchEnabled = true
        backgroundColor = .clear
    }

    /// A physical mouse or trackpad emits `.indirectPointer` touches wherever its cursor is.
    /// Those are the desktop's own pointer, already forwarded through GCMouse, so the on-screen
    /// trackpad must treat them as neither movement nor clicks.
    private func isFingerTouch(_ touch: UITouch) -> Bool {
        touch.type == .direct || touch.type == .pencil
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        let touches = touches.filter(isFingerTouch)
        guard !touches.isEmpty else { return }
        let now = CACurrentMediaTime()
        if activeTouches.isEmpty {
            onInteractionChanged?(true)
            gestureStart = now
            maxFingers = 0
            movement = 0
            wasCancelled = false
            dragArmed = now - lastTapEnd < Self.dragArmWindow
        }
        activeTouches.append(contentsOf: touches)
        maxFingers = max(maxFingers, activeTouches.count)
        if activeTouches.count >= 2 && dragging {
            dragging = false
            onButton?(Self.buttonLeft, false)
        }
        resetTrackingPoints()
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        guard touches.contains(where: { activeTouches.contains($0) }) else { return }
        if activeTouches.count >= 2 {
            let location = centroid()
            if let last = lastTwoFingerLocation {
                let dx = location.x - last.x
                let dy = location.y - last.y
                movement += hypot(dx, dy)
                if movement >= Self.tapMaxMovement { onScroll?(dx, dy) }
            }
            lastTwoFingerLocation = location
            return
        }

        // Lifting one finger after scrolling must not unexpectedly move the pointer.
        guard maxFingers == 1 else { return }
        guard let touch = activeTouches.first else { return }
        let location = touch.location(in: self)
        defer { lastSingleLocation = location }
        guard let last = lastSingleLocation else { return }
        let dx = location.x - last.x
        let dy = location.y - last.y
        movement += hypot(dx, dy)

        if dragArmed && !dragging && movement > Self.tapMaxMovement {
            dragging = true
            onButton?(Self.buttonLeft, true)
        }

        let gain = Self.baseGain * (1 + min(hypot(dx, dy) / 12, 2))
        onMove?(dx * gain, dy * gain)
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        finish(trackedOnly(touches), cancelled: false)
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        finish(trackedOnly(touches), cancelled: true)
    }

    /// Mouse button-up/cancel events must not complete a finger gesture.
    private func trackedOnly(_ touches: Set<UITouch>) -> Set<UITouch> {
        touches.filter { activeTouches.contains($0) }
    }

    private func finish(_ touches: Set<UITouch>, cancelled: Bool) {
        // Nothing we track ended, so there is no gesture to finish or click to synthesize.
        guard !touches.isEmpty else { return }
        wasCancelled = wasCancelled || cancelled
        activeTouches.removeAll { touches.contains($0) }
        resetTrackingPoints()
        guard activeTouches.isEmpty else { return }
        onInteractionChanged?(false)

        let now = CACurrentMediaTime()
        let wasTap = !wasCancelled
            && now - gestureStart < Self.tapMaxDuration
            && movement < Self.tapMaxMovement

        if dragging {
            dragging = false
            onButton?(Self.buttonLeft, false)
        } else if wasTap && maxFingers == 2 {
            click(Self.buttonRight)
        } else if wasTap && maxFingers == 1 {
            click(Self.buttonLeft)
            lastTapEnd = now
            dragArmed = false
            return
        }
        lastTapEnd = 0
        dragArmed = false
    }

    private func click(_ button: UInt8) {
        onButton?(button, true)
        onButton?(button, false)
    }

    /// Re-base move/scroll tracking whenever the set of fingers changes, so adding or lifting
    /// a finger doesn't produce a jump.
    private func resetTrackingPoints() {
        lastSingleLocation = activeTouches.count == 1 ? activeTouches[0].location(in: self) : nil
        lastTwoFingerLocation = activeTouches.count >= 2 ? centroid() : nil
    }

    private func centroid() -> CGPoint {
        let points = activeTouches.map { $0.location(in: self) }
        let n = CGFloat(max(points.count, 1))
        return CGPoint(x: points.reduce(0) { $0 + $1.x } / n, y: points.reduce(0) { $0 + $1.y } / n)
    }

    func cancelInteraction() {
        let wasActive = !activeTouches.isEmpty
        if dragging { onButton?(Self.buttonLeft, false) }
        dragging = false
        dragArmed = false
        lastTapEnd = 0
        activeTouches.removeAll()
        resetTrackingPoints()
        if wasActive { onInteractionChanged?(false) }
    }
}

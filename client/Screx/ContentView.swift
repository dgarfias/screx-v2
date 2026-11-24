import SwiftUI
import UIKit

enum ToolbarOrientation: String {
    case horizontal, vertical
}

struct ContentView: View {
    @EnvironmentObject private var model: StreamViewModel
    @State private var showOverlay = true

    @State private var barPosition: CGPoint = Self.loadBarPosition()
    @State private var barOrientation: ToolbarOrientation = Self.loadBarOrientation()
    @State private var isToolbarExpanded: Bool = Self.loadToolbarExpanded()
    @State private var dragOffset: CGSize = .zero
    @State private var isDragging = false
    @State private var isKeyboardActive = false
    @State private var keyboardHeight: CGFloat = 0
    @State private var keyboardFrame: CGRect = .null
    @State private var preKeyboardY: CGFloat? = nil
    @State private var viewSize: CGSize = .zero
    @State private var pillSize: CGSize = CGSize(width: 80, height: 44)
    @State private var toolbarMessage: String? = nil
    @State private var showDisplaySettings = false

    private static let btnSize: CGFloat = 32
    private static let btnSpacing: CGFloat = 6
    private static let edgeThreshold: CGFloat = 40

    var body: some View {
        GeometryReader { geo in
            ZStack {
                Color.black.ignoresSafeArea()

                if model.displayRouting.internalVideoVisible, let layer = model.displayLayer {
                    VideoDisplayView(
                        layer: layer,
                        videoWidth: Int(model.internalVideoSize.width),
                        videoHeight: Int(model.internalVideoSize.height),
                        onTouch: { data in model.sendTouch(data) },
                        onScroll: { dx, dy in model.forwardPhysicalMouseScroll(deltaX: dx, deltaY: dy) },
                        hidePointer: model.physicalMouseConnected,
                        lockPointer: model.physicalMouseConnected,
                        acceptsTouch: model.canForwardInternalTouch,
                        onWindowAttachmentChanged: { model.setDisplaySurfaceAttached(.internalDisplay, attached: $0) }
                    )
                    .ignoresSafeArea()
                } else if model.isConnected {
                    Group {
                        if model.isExternalControlPanelVisible {
                            ExternalControlPanel(
                                model: model,
                                onShowDisplays: { showDisplaySettings = true },
                                onMessage: { showToolbarMessage($0) }
                            )
                            .ignoresSafeArea(.container, edges: .bottom)
                            .onAppear { isKeyboardActive = false }
                        } else {
                            VStack(spacing: 16) {
                                Image(systemName: "display")
                                    .font(.system(size: 44))
                                    .foregroundStyle(.secondary)
                                    .accessibilityHidden(true)
                                Text("You are running only in the external display")
                                    .font(.headline)
                                Button {
                                    model.setLaptopMode(true)
                                } label: {
                                    Label("Switch to laptop mode", systemImage: "laptopcomputer")
                                }
                                .buttonStyle(.borderedProminent)
                            }
                            .foregroundStyle(.white)
                            .multilineTextAlignment(.center)
                            .padding(24)
                            .frame(maxWidth: .infinity, maxHeight: .infinity)
                        }
                    }
                    // No video view in External only, which normally owns pointer lock. Keep the
                    // iPad cursor hidden and locked so a physical mouse moves the remote pointer
                    // relatively instead of stopping at the iPad's screen edges.
                    .background(PointerLockRegion(locked: model.physicalMouseConnected).ignoresSafeArea())
                }

                if !model.isConnected {
                    ConnectionScreen(model: model)
                        .transition(.opacity)
                }

                // MARK: Info overlay (toggleable when connected)
                // Laptop mode shows connection info inline in its top bar.
                if model.isConnected && showOverlay && !model.isExternalControlPanelVisible {
                    StreamInfoView(model: model)
                    .padding(.horizontal, 16)
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
                    .padding(.top, model.isExternalControlPanelVisible ? 64 : 8)
                    .transition(.move(edge: .top).combined(with: .opacity))
                }

                if let toolbarMessage {
                    Text(toolbarMessage)
                        .font(.caption.weight(.medium))
                        .padding(.horizontal, 10)
                        .padding(.vertical, 6)
                        .screxGlassBackground(in: Capsule())
                        .foregroundStyle(.white)
                        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
                        .padding(.top, showOverlay && model.isConnected ? 92 : 16)
                        .transition(.move(edge: .top).combined(with: .opacity))
                }

                KeyboardInputView(
                    isActive: $isKeyboardActive,
                    physicalKeyboardActive: model.physicalKeyboardConnected,
                    onText: { model.sendTextInsert($0) },
                    onDelete: { model.sendSpecialKey(0x01) },
                    onSpecial: { model.sendSpecialKey($0) },
                    onCombo: { mods, text in model.sendComboText(mods: mods, text: text) },
                    onModSpecial: { mods, code in model.sendComboSpecial(mods: mods, code: code) }
                )
                .frame(width: 0, height: 0)

                if model.isConnected && !model.isExternalControlPanelVisible {
                    if model.isTrackpadEnabled {
                        TrackpadOverlay(
                            containerSize: geo.size,
                            keyboardFrame: keyboardFrame.offsetBy(
                                dx: -geo.frame(in: .global).minX,
                                dy: -geo.frame(in: .global).minY
                            ),
                            onMove: { dx, dy in model.moveTrackpad(dx: dx, dy: dy) },
                            onButton: { code, pressed in model.trackpadButton(code, pressed: pressed) },
                            onScroll: { dx, dy in model.scrollTrackpad(deltaX: dx, deltaY: dy) }
                        )
                        .id(model.inputLayoutRevision)
                        .frame(width: geo.size.width, height: geo.size.height)
                    }
                    floatingBar(in: geo)
                }
            }
            .onAppear {
                viewSize = geo.size
                clampBarPosition(in: geo.size, persist: barPosition != .zero)
            }
            .onChange(of: geo.size) { newSize in
                viewSize = newSize
                clampBarPosition(in: newSize)
            }
            .onChange(of: pillSize) { _ in
                clampBarPosition(in: geo.size)
            }
            .onChange(of: model.externalDisplayAvailable) { available in
                if available && model.isConnected {
                    showToolbarMessage("External display connected. Open Displays to choose an output.")
                }
            }
            .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillChangeFrameNotification)) { notif in
                guard let frame = notif.userInfo?[UIResponder.keyboardFrameEndUserInfoKey] as? CGRect else { return }
                let overlap = frame.intersection(geo.frame(in: .global))
                let kbH = overlap.isEmpty ? 0 : max(0, geo.frame(in: .global).maxY - overlap.minY)
                let duration = notif.userInfo?[UIResponder.keyboardAnimationDurationUserInfoKey] as? Double ?? 0.25
                if kbH > 0 && preKeyboardY == nil { preKeyboardY = barPosition.y }
                withAnimation(.easeOut(duration: duration)) {
                    keyboardFrame = frame
                    keyboardHeight = kbH
                    if kbH == 0, let savedY = preKeyboardY {
                        barPosition = clampedBarPosition(CGPoint(x: barPosition.x, y: savedY), in: geo.size, keyboardHeight: 0)
                        preKeyboardY = nil
                    } else {
                        clampBarPosition(in: geo.size, persist: false)
                    }
                }
            }
            .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillHideNotification)) { _ in
                withAnimation(.easeOut(duration: 0.25)) {
                    keyboardFrame = .null
                    keyboardHeight = 0
                    let y = preKeyboardY ?? barPosition.y
                    barPosition = clampedBarPosition(CGPoint(x: barPosition.x, y: y), in: geo.size, keyboardHeight: 0)
                    preKeyboardY = nil
                }
            }
        }
        // Our floating controls handle keyboard occlusion themselves. Keeping the container
        // full-height avoids SwiftUI shrinking/shifting it a second time underneath them.
        .ignoresSafeArea(.keyboard)
        .sheet(isPresented: $showDisplaySettings) {
            DisplaySettingsView(model: model)
        }
        .alert("Couldn’t switch displays", isPresented: Binding(
            get: { model.displayConfigurationError != nil && !showDisplaySettings },
            set: { if !$0 { model.clearDisplayConfigurationError() } }
        )) {
            Button("OK") { model.clearDisplayConfigurationError() }
        } message: {
            Text(model.displayConfigurationError ?? "The previous display configuration was kept.")
        }
        .sheet(isPresented: $model.showPinEntry) {
            VStack(spacing: 20) {
                Text("Pairing Required")
                    .font(.title2.bold())

                Text(model.pairingStatus)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)

                TextField("0000-0000", text: $model.pinInput)
                    .font(.system(size: 32, weight: .bold, design: .monospaced))
                    .multilineTextAlignment(.center)
                    .keyboardType(.numberPad)
                    .submitLabel(.go)
                    .onSubmit { model.submitPin() }
                    .frame(maxWidth: 240)
                    .textFieldStyle(.roundedBorder)

                HStack(spacing: 16) {
                    Button("Cancel") { model.cancelPin() }
                        .buttonStyle(.bordered)

                    Button("Pair") { model.submitPin() }
                        .buttonStyle(.borderedProminent)
                        .disabled(model.normalizedPairingPin.utf8.count != 8 ||
                                  !model.normalizedPairingPin.utf8.allSatisfy { (48...57).contains($0) })
                }
            }
            .padding(32)
            .interactiveDismissDisabled()
        }
        .statusBarHidden(true)
        .persistentSystemOverlays(.hidden)
        .onChange(of: model.physicalKeyboardConnected) { connected in
            if connected {
                isKeyboardActive = false
            }
        }
    }

    // MARK: - Floating button bar

    @ViewBuilder
    private func floatingBar(in geo: GeometryProxy) -> some View {
        let layout = barOrientation == .vertical
            ? AnyLayout(VStackLayout(spacing: Self.btnSpacing))
            : AnyLayout(HStackLayout(spacing: Self.btnSpacing))

        let pos = clampedBarPosition(
            CGPoint(x: barPosition.x + dragOffset.width, y: barPosition.y + dragOffset.height),
            in: geo.size
        )

        layout {
            if model.isConnected {
                toolbarButton(
                    icon: barOrientation == .vertical
                        ? (isToolbarExpanded ? "chevron.down" : "chevron.up")
                        : (isToolbarExpanded ? "chevron.right" : "chevron.left"),
                    color: .white
                ) {
                    let nextExpanded = !isToolbarExpanded
                    withAnimation(.easeInOut(duration: 0.2)) {
                        isToolbarExpanded = nextExpanded
                    }
                    Self.saveToolbarExpanded(nextExpanded)
                    DispatchQueue.main.async {
                        clampBarPosition(in: geo.size)
                    }
                }

                if isToolbarExpanded {
                    toolbarButton(
                        icon: "rectangle.and.hand.point.up.left",
                        active: model.isTrackpadEnabled,
                        color: model.isTrackpadEnabled ? .green : .white
                    ) {
                        model.setTrackpadEnabled(!model.isTrackpadEnabled)
                    }
                    .accessibilityLabel(model.isTrackpadEnabled ? "Hide trackpad" : "Show trackpad")
                    .accessibilityHint("Move the pointer, tap to click, or use two fingers to scroll")

                    toolbarButton(
                        icon: model.isControllerPassthroughEnabled ? "gamecontroller.fill" : "gamecontroller",
                        active: model.isControllerPassthroughEnabled,
                        color: !model.isControllerAvailable ? .gray : (model.isControllerPassthroughEnabled ? .green : .white)
                    ) {
                        if model.isControllerAvailable {
                            model.toggleControllerPassthrough()
                        } else {
                            showToolbarMessage("Gamepad passthrough not supported by this daemon")
                        }
                    }

                    toolbarButton(
                        icon: model.isCameraActive
                            ? (model.isCameraFront ? "arrow.triangle.2.circlepath.camera.fill" : "video.fill")
                            : "video",
                        active: model.isCameraActive,
                        color: !model.isCameraAvailable ? .gray : (model.isCameraActive ? .green : .white)
                    ) {
                        if model.isCameraAvailable {
                            model.toggleCamera()
                        } else {
                            showToolbarMessage("Camera not supported by this daemon")
                        }
                    }
                        .onLongPressGesture(minimumDuration: 0.5) { model.flipCamera() }

                    toolbarButton(
                        icon: model.isMicActive ? "mic.fill" : "mic",
                        active: model.isMicActive,
                        color: !model.isMicAvailable ? .gray : (model.isMicActive ? .green : .white)
                    ) {
                        if model.isMicAvailable {
                            model.toggleMic()
                        } else {
                            showToolbarMessage("Microphone not supported by this daemon")
                        }
                    }

                    toolbarButton(
                        icon: model.isSpeakerActive ? "speaker.wave.2.fill" : "speaker.slash.fill",
                        active: model.isSpeakerActive,
                        color: !model.isSpeakerAvailable ? .gray : (model.isSpeakerActive ? .green : .white)
                    ) {
                        if model.isSpeakerAvailable {
                            model.toggleSpeaker()
                        } else {
                            showToolbarMessage("Speaker not supported by this daemon")
                        }
                    }
                }

                if model.externalDisplayAvailable || model.displayRouting.committedMask & 2 != 0 {
                    Button {
                        showDisplaySettings = true
                    } label: {
                        Image(systemName: model.isChangingDisplays ? "arrow.triangle.2.circlepath" : "display.2")
                            .font(.footnote)
                            .foregroundStyle(model.isChangingDisplays ? .orange : .white)
                            .frame(width: Self.btnSize, height: Self.btnSize)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel("Displays")
                    .accessibilityValue(model.isChangingDisplays ? "Switching displays" : model.selectedDisplayMode.label)
                    .accessibilityHint("Choose internal, external, or both displays")
                }

                toolbarButton(
                    icon: isKeyboardActive ? "keyboard.fill" : "keyboard",
                    active: isKeyboardActive && !model.physicalKeyboardConnected,
                    color: model.physicalKeyboardConnected ? .gray : (isKeyboardActive ? .green : .white)
                ) {
                    if model.physicalKeyboardConnected {
                        showToolbarMessage("External keyboard detected")
                    } else {
                        isKeyboardActive.toggle()
                    }
                }

                toolbarButton(
                    icon: showOverlay ? "info.circle.fill" : "info.circle",
                    active: showOverlay,
                    color: showOverlay ? .green : .white
                ) {
                    withAnimation(.easeInOut(duration: 0.2)) { showOverlay.toggle() }
                }
            }
        }
        .padding(4)
        .screxGlassBackground(in: Capsule())
        .contentShape(Capsule())
        .opacity(isDragging ? 0.8 : 1)
        .background(
            GeometryReader { pillGeo in
                Color.clear.onAppear { pillSize = pillGeo.size }
                    .onChange(of: pillGeo.size) { newSize in
                        DispatchQueue.main.async {
                            pillSize = newSize
                            clampBarPosition(in: geo.size)
                        }
                    }
                    .onChange(of: model.isConnected) { _ in
                        DispatchQueue.main.async {
                            pillSize = pillGeo.size
                            clampBarPosition(in: geo.size)
                        }
                    }
                    .onChange(of: isToolbarExpanded) { _ in
                        DispatchQueue.main.async {
                            pillSize = pillGeo.size
                            withAnimation(.easeInOut(duration: 0.2)) {
                                clampBarPosition(in: geo.size)
                            }
                        }
                    }
            }
        )
        .position(pos)
        .gesture(
            DragGesture(minimumDistance: 5, coordinateSpace: .global)
                .onChanged { value in
                    isDragging = true
                    dragOffset = value.translation
                }
                .onEnded { value in
                    isDragging = false
                    var newPos = CGPoint(
                        x: barPosition.x + value.translation.width,
                        y: barPosition.y + value.translation.height
                    )

                    var newOrientation = barOrientation
                    if newPos.x < Self.edgeThreshold {
                        newOrientation = .vertical
                    } else if newPos.y < Self.edgeThreshold {
                        newOrientation = .horizontal
                    } else if newPos.y > geo.size.height - Self.edgeThreshold {
                        newOrientation = .horizontal
                    }

                    newPos = clampedBarPosition(newPos, in: geo.size)
                    preKeyboardY = nil

                    barPosition = newPos
                    dragOffset = .zero

                    if newOrientation != barOrientation {
                        withAnimation(.easeOut(duration: 0.2)) {
                            barOrientation = newOrientation
                        }
                    }

                    Self.saveBarPosition(newPos)
                    Self.saveBarOrientation(newOrientation)
                }
        )
    }

    @ViewBuilder
    private func toolbarButton(icon: String, active: Bool = false, color: Color, action: @escaping () -> Void) -> some View {
        Image(systemName: icon)
            .font(.footnote)
            .foregroundStyle(color)
            .frame(width: Self.btnSize, height: Self.btnSize)
            .contentShape(Circle())
            .onTapGesture(perform: action)
    }

    private func showToolbarMessage(_ message: String) {
        withAnimation(.easeInOut(duration: 0.2)) {
            toolbarMessage = message
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.8) {
            guard toolbarMessage == message else { return }
            withAnimation(.easeInOut(duration: 0.2)) {
                toolbarMessage = nil
            }
        }
    }

    private func defaultBarPosition(in size: CGSize) -> CGPoint {
        let halfW = pillSize.width / 2 + 4
        let halfH = pillSize.height / 2 + 4
        return CGPoint(x: size.width - halfW, y: size.height - halfH)
    }

    private func clampedBarPosition(_ position: CGPoint, in size: CGSize, keyboardHeight: CGFloat? = nil) -> CGPoint {
        guard size.width > 0, size.height > 0 else { return position }

        let halfW = pillSize.width / 2 + 4
        let halfH = pillSize.height / 2 + 4
        let activeKeyboardHeight = keyboardHeight ?? self.keyboardHeight

        let minX = halfW
        let maxX = max(halfW, size.width - halfW)
        let minY = halfH
        let maxVisibleY = size.height - activeKeyboardHeight - halfH
        let maxY = max(halfH, maxVisibleY)

        return CGPoint(
            x: min(max(position.x, minX), maxX),
            y: min(max(position.y, minY), maxY)
        )
    }

    private func clampBarPosition(in size: CGSize, persist: Bool = true) {
        let target = barPosition == .zero ? defaultBarPosition(in: size) : barPosition
        let clamped = clampedBarPosition(target, in: size)
        guard clamped != barPosition else { return }
        barPosition = clamped
        if persist {
            Self.saveBarPosition(clamped)
        }
    }

    // MARK: - Persistence

    private static let posXKey = "screx_bar_x"
    private static let posYKey = "screx_bar_y"
    private static let orientKey = "screx_bar_orient"
    private static let expandedKey = "screx_bar_expanded"

    private static func loadBarPosition() -> CGPoint {
        let defaults = UserDefaults.standard
        guard defaults.object(forKey: posXKey) != nil else { return .zero }
        return CGPoint(x: defaults.double(forKey: posXKey), y: defaults.double(forKey: posYKey))
    }

    private static func saveBarPosition(_ pos: CGPoint) {
        let defaults = UserDefaults.standard
        defaults.set(pos.x, forKey: posXKey)
        defaults.set(pos.y, forKey: posYKey)
    }

    private static func loadBarOrientation() -> ToolbarOrientation {
        let raw = UserDefaults.standard.string(forKey: orientKey) ?? "horizontal"
        return ToolbarOrientation(rawValue: raw) ?? .horizontal
    }

    private static func saveBarOrientation(_ orient: ToolbarOrientation) {
        UserDefaults.standard.set(orient.rawValue, forKey: orientKey)
    }

    private static func loadToolbarExpanded() -> Bool {
        UserDefaults.standard.object(forKey: expandedKey) != nil
            ? UserDefaults.standard.bool(forKey: expandedKey)
            : false
    }

    private static func saveToolbarExpanded(_ expanded: Bool) {
        UserDefaults.standard.set(expanded, forKey: expandedKey)
    }
}

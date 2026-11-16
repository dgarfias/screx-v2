import SwiftUI
import UIKit

/// Requests pointer lock and hides the iPad cursor over the panel, mirroring what
/// `DisplayContainerController` does for the video view in the other modes.
struct PointerLockRegion: UIViewControllerRepresentable {
    let locked: Bool

    func makeUIViewController(context: Context) -> PointerLockController {
        let controller = PointerLockController()
        controller.locked = locked
        return controller
    }

    func updateUIViewController(_ controller: PointerLockController, context: Context) {
        controller.locked = locked
    }
}

final class PointerLockController: UIViewController, UIPointerInteractionDelegate {
    var locked = false {
        didSet {
            guard oldValue != locked else { return }
            setNeedsUpdateOfPrefersPointerLocked()
        }
    }

    override func loadView() {
        let view = UIView()
        view.backgroundColor = .clear
        view.addInteraction(UIPointerInteraction(delegate: self))
        self.view = view
    }

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        setNeedsUpdateOfPrefersPointerLocked()
    }

    override var prefersPointerLocked: Bool { locked }

    func pointerInteraction(_ interaction: UIPointerInteraction, styleFor region: UIPointerRegion) -> UIPointerStyle? {
        locked ? .hidden() : nil
    }
}

/// **Laptop mode** for External only: top bar (options left, connection info right), a drawn
/// laptop keyboard, and the trackpad at the bottom. Replaces the pill and the iOS keyboard.
struct ExternalControlPanel: View {
    @ObservedObject var model: StreamViewModel
    let onShowDisplays: () -> Void
    let onMessage: (String) -> Void

    @State private var isTrackpadActive = false

    private static let buttonSize: CGFloat = 44
    private static let topBarHeight: CGFloat = 52

    var body: some View {
        GeometryReader { geo in
            // Edge to edge like a laptop deck: on an 11" iPad in landscape each key is ~78 pt wide
            // and ~66 pt tall, close to a full-size key. The trackpad keeps at least 180 pt.
            let keyboardWidth = geo.size.width - 12
            let unit = keyboardWidth / CGFloat(LaptopKeyboardLayout.rowUnits)
            let spacing: CGFloat = 12 * 2 + 16 + 6
            let rowHeight = max(36, min(unit * 0.85, 72,
                                        (geo.size.height - Self.topBarHeight - spacing - 180) / 6))
            let keyboardHeight = rowHeight * 6
            let trackpadHeight = min(max(geo.size.height - Self.topBarHeight - keyboardHeight - spacing, 0), 360)
            VStack(spacing: 12) {
                topBar.padding(.top, 6)
                LaptopKeyboardView(
                    onText: { model.sendTextInsert($0) },
                    onSpecial: { model.sendSpecialKey($0) },
                    onComboText: { mods, text in model.sendComboText(mods: mods, text: text) },
                    onComboSpecial: { mods, code in model.sendComboSpecial(mods: mods, code: code) }
                )
                .frame(width: keyboardWidth, height: keyboardHeight)
                Spacer(minLength: 0)
                trackpad
                    .frame(width: min(geo.size.width * 0.45, 560), height: trackpadHeight)
                    .opacity(trackpadHeight >= 100 ? 1 : 0)
                    .padding(.bottom, 16)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    private var trackpad: some View {
        ZStack {
            RoundedRectangle(cornerRadius: 16, style: .continuous)
                .fill(Color.white.opacity(isTrackpadActive ? 0.10 : 0.06))
            RoundedRectangle(cornerRadius: 16, style: .continuous)
                .strokeBorder(Color.white.opacity(isTrackpadActive ? 0.35 : 0.18), lineWidth: 1)
            TrackpadSurface(
                onMove: { dx, dy in model.moveTrackpad(dx: dx, dy: dy) },
                onButton: { code, pressed in model.trackpadButton(code, pressed: pressed) },
                onScroll: { model.scrollTrackpad(deltaY: $0) },
                onInteractionChanged: { active in
                    withAnimation(.easeOut(duration: 0.15)) { isTrackpadActive = active }
                }
            )
        }
    }

    private var topBar: some View {
        HStack(spacing: 2) {
            Button {
                model.setLaptopMode(false)
            } label: {
                Label("Switch to normal mode", systemImage: "rectangle.on.rectangle")
                    .font(.footnote.weight(.medium))
                    .foregroundStyle(.white)
                    .padding(.horizontal, 12)
                    .frame(height: 36)
                    .background(Color.white.opacity(0.14), in: Capsule())
                    .frame(height: Self.buttonSize)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .padding(.trailing, 6)
            control("display.2", label: "Displays", action: onShowDisplays)
            control(model.isSpeakerActive ? "speaker.wave.2.fill" : "speaker.slash.fill",
                    label: "Speaker",
                    tint: !model.isSpeakerAvailable ? .gray : (model.isSpeakerActive ? .green : .white)) {
                if model.isSpeakerAvailable { model.toggleSpeaker() } else { onMessage("Speaker not supported by this daemon") }
            }
            control(model.isMicActive ? "mic.fill" : "mic",
                    label: "Microphone",
                    tint: !model.isMicAvailable ? .gray : (model.isMicActive ? .green : .white)) {
                if model.isMicAvailable { model.toggleMic() } else { onMessage("Microphone not supported by this daemon") }
            }
            control(model.isCameraActive ? "video.fill" : "video",
                    label: "Camera",
                    tint: !model.isCameraAvailable ? .gray : (model.isCameraActive ? .green : .white)) {
                if model.isCameraAvailable { model.toggleCamera() } else { onMessage("Camera not supported by this daemon") }
            }
            control(model.isControllerPassthroughEnabled ? "gamecontroller.fill" : "gamecontroller",
                    label: "Controller",
                    tint: !model.isControllerAvailable ? .gray : (model.isControllerPassthroughEnabled ? .green : .white)) {
                if model.isControllerAvailable { model.toggleControllerPassthrough() } else { onMessage("Gamepad passthrough not supported by this daemon") }
            }
            Spacer(minLength: 12)
            infoText
        }
        .frame(height: Self.topBarHeight)
        .padding(.horizontal, 12)
    }

    /// Connection info at the right end of the bar, followed by a red disconnect button.
    private var infoText: some View {
        HStack(spacing: 10) {
            HStack(spacing: 10) {
                Text(model.sessionDisplayName.isEmpty ? "Screx" : model.sessionDisplayName)
                    .fontWeight(.semibold)
                    .lineLimit(1)
                Text("↓ \(model.receiveRateText)  ↑ \(model.sendRateText)")
                Text("RTT \(model.latencyText)  Jitter \(model.jitterText)")
                    .foregroundStyle(.secondary)
            }
            .font(.caption.monospacedDigit())
            .foregroundStyle(.white)
            .accessibilityElement(children: .combine)

            Button {
                model.disconnect()
            } label: {
                Image(systemName: "xmark.circle.fill")
                    .font(.title3)
                    .symbolRenderingMode(.palette)
                    .foregroundStyle(.white, .red)
                    .frame(width: Self.buttonSize, height: Self.buttonSize)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel("Disconnect")
        }
    }
    private func control(_ icon: String, label: String, tint: Color = .white,
                         action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Image(systemName: icon)
                .font(.footnote)
                .foregroundStyle(tint)
                .frame(width: Self.buttonSize, height: Self.buttonSize)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(label)
    }
}

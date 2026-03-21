import SwiftUI
import AVFoundation
import UIKit

// MARK: - Keyboard input proxy (captures iOS native keyboard and forwards to daemon)

struct KeyboardInputView: UIViewRepresentable {
    @Binding var isActive: Bool
    let onText: (String) -> Void
    let onDelete: () -> Void
    let onSpecial: (UInt8) -> Void
    let onCombo: (UInt8, String) -> Void
    let onModSpecial: (UInt8, UInt8) -> Void

    func makeUIView(context: Context) -> KeyInputProxyView {
        let view = KeyInputProxyView()
        view.onText = onText
        view.onDelete = onDelete
        view.onSpecial = onSpecial
        view.onCombo = onCombo
        view.onModSpecial = onModSpecial
        view.onResign = { context.coordinator.deactivate() }
        view.isUserInteractionEnabled = false
        return view
    }

    func updateUIView(_ uiView: KeyInputProxyView, context: Context) {
        uiView.onText = onText
        uiView.onDelete = onDelete
        uiView.onSpecial = onSpecial
        uiView.onCombo = onCombo
        uiView.onModSpecial = onModSpecial
        uiView.onResign = { context.coordinator.deactivate() }
        uiView.allowFirstResponder = isActive
        uiView.isUserInteractionEnabled = isActive
        if isActive && !uiView.isFirstResponder {
            DispatchQueue.main.async { uiView.becomeFirstResponder() }
        } else if !isActive && uiView.isFirstResponder {
            DispatchQueue.main.async { uiView.resignFirstResponder() }
        }
    }

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    final class Coordinator {
        var parent: KeyboardInputView
        init(_ parent: KeyboardInputView) { self.parent = parent }
        func deactivate() {
            DispatchQueue.main.async { self.parent.isActive = false }
        }
    }
}

final class KeyInputProxyView: UIView, UIKeyInput {
    var onText: ((String) -> Void)?
    var onDelete: (() -> Void)?
    var onSpecial: ((UInt8) -> Void)?
    var onCombo: ((UInt8, String) -> Void)?
    var onModSpecial: ((UInt8, UInt8) -> Void)?
    var onResign: (() -> Void)?
    var allowFirstResponder = false

    private var activeModifiers: UInt8 = 0
    private var modifierButtons: [UInt8: UIButton] = [:]

    override var canBecomeFirstResponder: Bool { allowFirstResponder }
    var hasText: Bool { true }

    var autocorrectionType: UITextAutocorrectionType { .no }

    override var inputAccessoryView: UIView? { accessoryBar }

    func insertText(_ text: String) {
        if activeModifiers != 0 {
            onCombo?(activeModifiers, text)
            clearModifiers()
        } else {
            onText?(text)
        }
    }

    func deleteBackward() {
        if activeModifiers != 0 {
            onModSpecial?(activeModifiers, 0x01)
            clearModifiers()
        } else {
            onDelete?()
        }
    }

    @discardableResult
    override func resignFirstResponder() -> Bool {
        let result = super.resignFirstResponder()
        if result {
            allowFirstResponder = false
            clearModifiers()
            onResign?()
        }
        return result
    }

    // MARK: - Modifier state

    private static let modSpecialCodes: [UInt8: UInt8] = [
        0x01: 0x0C, // Ctrl
        0x02: 0x0D, // Alt
        0x04: 0x0E, // Super
    ]

    private func toggleModifier(_ mask: UInt8) {
        if (activeModifiers & mask) != 0 {
            // Already active → send lone keypress and deactivate
            activeModifiers &= ~mask
            updateModifierAppearance()
            if let code = Self.modSpecialCodes[mask] {
                onSpecial?(code)
            }
        } else {
            activeModifiers |= mask
            updateModifierAppearance()
        }
    }

    private func clearModifiers() {
        activeModifiers = 0
        updateModifierAppearance()
    }

    private func updateModifierAppearance() {
        for (mask, btn) in modifierButtons {
            let isOn = (activeModifiers & mask) != 0
            btn.backgroundColor = isOn
                ? UIColor.systemBlue
                : UIColor(white: 0.4, alpha: 1)
        }
    }

    private func handleSpecialKey(_ code: UInt8) {
        if activeModifiers != 0 {
            onModSpecial?(activeModifiers, code)
            clearModifiers()
        } else {
            onSpecial?(code)
        }
    }

    // MARK: - Accessory bar

    private lazy var accessoryBar: UIInputView = {
        let bar = UIInputView(frame: CGRect(x: 0, y: 0, width: 0, height: 44),
                              inputViewStyle: .keyboard)

        let stack = UIStackView()
        stack.axis = .horizontal
        stack.spacing = 5
        stack.alignment = .center
        stack.translatesAutoresizingMaskIntoConstraints = false

        let spacer = { () -> UIView in
            let v = UIView()
            v.setContentHuggingPriority(.defaultLow, for: .horizontal)
            return v
        }

        let items: [(String, AccessoryAction)] = [
            ("Esc",   .special(0x04)),
            ("Tab",   .special(0x03)),
            ("Ctrl",  .modifier(0x01)),
            ("Alt",   .modifier(0x02)),
            ("Super", .modifier(0x04)),
            ("Home",  .special(0x0A)),
            ("End",   .special(0x0B)),
            ("Ins",   .special(0x0F)),
            ("Del",   .special(0x09)),
            ("←",     .special(0x05)),
            ("↑",     .special(0x07)),
            ("↓",     .special(0x08)),
            ("→",     .special(0x06)),
        ]

        for (label, action) in items {
            let btn = makeKeyButton(label)
            switch action {
            case .special(let code):
                btn.addAction(UIAction { [weak self] _ in
                    self?.handleSpecialKey(code)
                }, for: .touchUpInside)
            case .modifier(let mask):
                modifierButtons[mask] = btn
                btn.addAction(UIAction { [weak self] _ in
                    self?.toggleModifier(mask)
                }, for: .touchUpInside)
            }
            stack.addArrangedSubview(btn)
        }

        stack.addArrangedSubview(spacer())

        bar.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: bar.leadingAnchor, constant: 6),
            stack.trailingAnchor.constraint(equalTo: bar.trailingAnchor, constant: -6),
            stack.centerYAnchor.constraint(equalTo: bar.centerYAnchor),
        ])

        return bar
    }()

    private func makeKeyButton(_ title: String) -> UIButton {
        let btn = UIButton(type: .system)
        btn.setTitle(title, for: .normal)
        btn.titleLabel?.font = .systemFont(ofSize: 14, weight: .medium)
        btn.setTitleColor(.white, for: .normal)
        btn.backgroundColor = UIColor(white: 0.4, alpha: 1)
        btn.layer.cornerRadius = 6
        btn.clipsToBounds = true
        btn.contentEdgeInsets = UIEdgeInsets(top: 6, left: 10, bottom: 6, right: 10)
        return btn
    }

    private enum AccessoryAction {
        case special(UInt8)
        case modifier(UInt8)
    }
}

struct VideoDisplayView: UIViewRepresentable {
    let layer: AVSampleBufferDisplayLayer
    let videoWidth: Int
    let videoHeight: Int
    var onTouch: ((Data) -> Void)?

    func makeUIView(context: Context) -> DisplayContainerView {
        let view = DisplayContainerView()
        view.videoWidth = videoWidth
        view.videoHeight = videoHeight
        view.onTouch = onTouch
        view.attach(layer: layer)
        return view
    }

    func updateUIView(_ uiView: DisplayContainerView, context: Context) {
        uiView.videoWidth = videoWidth
        uiView.videoHeight = videoHeight
        uiView.onTouch = onTouch
        uiView.attach(layer: layer)
    }
}

final class DisplayContainerView: UIView {
    private weak var attachedLayer: AVSampleBufferDisplayLayer?

    var videoWidth: Int = 1920
    var videoHeight: Int = 1080
    var onTouch: ((Data) -> Void)?

    private var touchSlots: [ObjectIdentifier: UInt8] = [:]
    private var nextSlot: UInt8 = 0

    override init(frame: CGRect) {
        super.init(frame: frame)
        isMultipleTouchEnabled = true
    }

    required init?(coder: NSCoder) {
        super.init(coder: coder)
        isMultipleTouchEnabled = true
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        attachedLayer?.frame = bounds
    }

    func attach(layer: AVSampleBufferDisplayLayer) {
        if attachedLayer !== layer {
            attachedLayer?.removeFromSuperlayer()
            layer.removeFromSuperlayer()
            self.layer.addSublayer(layer)
            attachedLayer = layer
        }
        layer.frame = bounds
    }

    // MARK: - Coordinate mapping

    private func videoRect() -> CGRect {
        let layerSize = bounds.size
        guard layerSize.width > 0, layerSize.height > 0 else { return .zero }

        let videoAspect = CGFloat(videoWidth) / CGFloat(videoHeight)
        let viewAspect = layerSize.width / layerSize.height

        if videoAspect > viewAspect {
            let h = layerSize.width / videoAspect
            return CGRect(x: 0, y: (layerSize.height - h) / 2, width: layerSize.width, height: h)
        } else {
            let w = layerSize.height * videoAspect
            return CGRect(x: (layerSize.width - w) / 2, y: 0, width: w, height: layerSize.height)
        }
    }

    private func mapToDisplay(_ point: CGPoint) -> (UInt16, UInt16)? {
        let vr = videoRect()
        guard vr.width > 0, vr.height > 0 else { return nil }

        let nx = (point.x - vr.minX) / vr.width
        let ny = (point.y - vr.minY) / vr.height

        guard nx >= 0, nx <= 1, ny >= 0, ny <= 1 else { return nil }

        let px = UInt16(min(nx * CGFloat(videoWidth), CGFloat(videoWidth - 1)).rounded())
        let py = UInt16(min(ny * CGFloat(videoHeight), CGFloat(videoHeight - 1)).rounded())
        return (px, py)
    }

    // MARK: - Slot management

    private func slotFor(_ touch: UITouch) -> UInt8 {
        let id = ObjectIdentifier(touch)
        if let slot = touchSlots[id] {
            return slot
        }
        let slot = nextSlot
        nextSlot = (nextSlot + 1) % 10
        touchSlots[id] = slot
        return slot
    }

    private func releaseSlot(_ touch: UITouch) {
        let id = ObjectIdentifier(touch)
        touchSlots.removeValue(forKey: id)
    }

    // MARK: - Touch events

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        sendTouchEvents(touches, eventType: 0)
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        sendTouchEvents(touches, eventType: 1)
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        sendTouchEvents(touches, eventType: 2)
        for touch in touches { releaseSlot(touch) }
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        sendTouchEvents(touches, eventType: 2)
        for touch in touches { releaseSlot(touch) }
    }

    private func sendTouchEvents(_ touches: Set<UITouch>, eventType: UInt8) {
        guard let onTouch else { return }

        var contacts = Data()
        var count: UInt8 = 0

        for touch in touches {
            let point = touch.location(in: self)
            guard let (px, py) = mapToDisplay(point) else { continue }
            let slot = slotFor(touch)

            // 8 bytes per contact: slot(1) + type(1) + x(2 BE) + y(2 BE) + padding(2)
            contacts.append(slot)
            contacts.append(eventType)
            withUnsafeBytes(of: px.bigEndian) { contacts.append(contentsOf: $0) }
            withUnsafeBytes(of: py.bigEndian) { contacts.append(contentsOf: $0) }
            contacts.append(contentsOf: [0, 0])
            count += 1
        }

        guard count > 0 else { return }

        var packet = Data()
        packet.append(count)
        packet.append(contacts)
        onTouch(packet)
    }
}

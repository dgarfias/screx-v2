import SwiftUI
import AVFoundation
import UIKit

// MARK: - Modifier flags (bitmask matching daemon protocol)

struct ModFlags {
    static let ctrl:  UInt8 = 0x01
    static let alt:   UInt8 = 0x02
    static let supér: UInt8 = 0x04
    static let shift: UInt8 = 0x08
}

// MARK: - Keyboard input proxy (captures iOS native keyboard and forwards to daemon)

struct KeyboardInputView: UIViewRepresentable {
    @Binding var isActive: Bool
    /// (text, modifierFlags) — flags are consumed and cleared after each keystroke
    let onText: (String, UInt8) -> Void
    /// (specialCode, modifierFlags)
    let onSpecial: (UInt8, UInt8) -> Void

    func makeUIView(context: Context) -> KeyInputProxyView {
        let view = KeyInputProxyView()
        view.onText = onText
        view.onSpecial = onSpecial
        view.onResign = { context.coordinator.deactivate() }
        view.isUserInteractionEnabled = false
        return view
    }

    func updateUIView(_ uiView: KeyInputProxyView, context: Context) {
        uiView.onText = onText
        uiView.onSpecial = onSpecial
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
    var onText: ((String, UInt8) -> Void)?
    var onSpecial: ((UInt8, UInt8) -> Void)?
    var onResign: (() -> Void)?
    var allowFirstResponder = false

    override var canBecomeFirstResponder: Bool { allowFirstResponder }
    var hasText: Bool { true }

    var autocorrectionType: UITextAutocorrectionType { .no }

    // MARK: - Modifier state (managed by the toolbar)

    private var modCtrl  = false
    private var modAlt   = false
    private var modSuper = false
    private var modShift = false
    private var modCtrlLocked  = false
    private var modAltLocked   = false
    private var modSuperLocked = false
    private var modShiftLocked = false

    private var ctrlButton:  UIButton?
    private var altButton:   UIButton?
    private var superButton: UIButton?
    private var shiftButton: UIButton?

    /// Builds the current modifier flags bitmask and resets one-shot modifiers.
    private func consumeModifiers() -> UInt8 {
        var flags: UInt8 = 0
        if modCtrl  { flags |= ModFlags.ctrl }
        if modAlt   { flags |= ModFlags.alt }
        if modSuper { flags |= ModFlags.supér }
        if modShift { flags |= ModFlags.shift }
        if !modCtrlLocked  { modCtrl = false }
        if !modAltLocked   { modAlt = false }
        if !modSuperLocked { modSuper = false }
        if !modShiftLocked { modShift = false }
        updateButtonStyles()
        return flags
    }

    // MARK: - UIKeyInput

    func insertText(_ text: String) {
        let mods = consumeModifiers()
        onText?(text, mods)
    }

    func deleteBackward() {
        let mods = consumeModifiers()
        onSpecial?(0x01, mods)
    }

    @discardableResult
    override func resignFirstResponder() -> Bool {
        let result = super.resignFirstResponder()
        if result {
            allowFirstResponder = false
            resetModifiers()
            onResign?()
        }
        return result
    }

    private func resetModifiers() {
        modCtrl = false; modAlt = false; modSuper = false; modShift = false
        modCtrlLocked = false; modAltLocked = false; modSuperLocked = false; modShiftLocked = false
        updateButtonStyles()
    }

    // MARK: - Input accessory view (pure UIKit toolbar)

    private lazy var toolbar: UIView = {
        let bar = UIInputView(frame: CGRect(x: 0, y: 0, width: UIScreen.main.bounds.width, height: 40),
                              inputViewStyle: .keyboard)
        bar.autoresizingMask = [.flexibleWidth]
        bar.allowsSelfSizing = true

        let scroll = UIScrollView()
        scroll.showsHorizontalScrollIndicator = false
        scroll.translatesAutoresizingMaskIntoConstraints = false
        bar.addSubview(scroll)
        NSLayoutConstraint.activate([
            scroll.leadingAnchor.constraint(equalTo: bar.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: bar.trailingAnchor),
            scroll.topAnchor.constraint(equalTo: bar.topAnchor),
            scroll.bottomAnchor.constraint(equalTo: bar.bottomAnchor),
            scroll.heightAnchor.constraint(equalToConstant: 40),
        ])

        let stack = UIStackView()
        stack.axis = .horizontal
        stack.spacing = 4
        stack.alignment = .center
        stack.translatesAutoresizingMaskIntoConstraints = false
        scroll.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: scroll.contentLayoutGuide.leadingAnchor, constant: 6),
            stack.trailingAnchor.constraint(equalTo: scroll.contentLayoutGuide.trailingAnchor, constant: -6),
            stack.topAnchor.constraint(equalTo: scroll.contentLayoutGuide.topAnchor),
            stack.bottomAnchor.constraint(equalTo: scroll.contentLayoutGuide.bottomAnchor),
            stack.heightAnchor.constraint(equalTo: scroll.frameLayoutGuide.heightAnchor),
        ])

        let esc  = makeKeyButton("Esc",   action: #selector(tapEsc))
        let tab  = makeKeyButton("Tab",   action: #selector(tapTab))
        let ctrl = makeModButton("Ctrl",  action: #selector(tapCtrl))
        let alt  = makeModButton("Alt",   action: #selector(tapAlt))
        let sup  = makeModButton("Super", action: #selector(tapSuper))
        let shft = makeModButton("Shift", action: #selector(tapShift))

        ctrlButton = ctrl; altButton = alt; superButton = sup; shiftButton = shft

        for btn in [esc, tab, ctrl, alt, sup, shft] { stack.addArrangedSubview(btn) }
        stack.addArrangedSubview(makeSeparator())
        for n in 1...12 {
            let btn = makeKeyButton("F\(n)", action: #selector(tapFKey(_:)))
            btn.tag = n
            stack.addArrangedSubview(btn)
        }
        stack.addArrangedSubview(makeSeparator())
        let pgup = makeKeyButton("PgUp", action: #selector(tapPgUp))
        let pgdn = makeKeyButton("PgDn", action: #selector(tapPgDn))
        let home = makeKeyButton("Home", action: #selector(tapHome))
        let end  = makeKeyButton("End",  action: #selector(tapEnd))
        for btn in [pgup, pgdn, home, end] { stack.addArrangedSubview(btn) }

        return bar
    }()

    override var inputAccessoryView: UIView? { toolbar }

    // MARK: - Button factory

    private func makeKeyButton(_ title: String, action: Selector) -> UIButton {
        let btn = UIButton(type: .system)
        btn.setTitle(title, for: .normal)
        btn.titleLabel?.font = .monospacedSystemFont(ofSize: 13, weight: .medium)
        btn.setTitleColor(.label, for: .normal)
        btn.contentEdgeInsets = UIEdgeInsets(top: 5, left: 8, bottom: 5, right: 8)
        btn.addTarget(self, action: action, for: .touchUpInside)
        return btn
    }

    private func makeModButton(_ title: String, action: Selector) -> UIButton {
        let btn = UIButton(type: .system)
        btn.setTitle(title, for: .normal)
        btn.titleLabel?.font = .monospacedSystemFont(ofSize: 13, weight: .semibold)
        btn.setTitleColor(.label, for: .normal)
        btn.contentEdgeInsets = UIEdgeInsets(top: 5, left: 8, bottom: 5, right: 8)
        btn.layer.cornerRadius = 5
        btn.addTarget(self, action: action, for: .touchUpInside)
        return btn
    }

    private func makeSeparator() -> UIView {
        let v = UIView()
        v.backgroundColor = .separator
        v.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            v.widthAnchor.constraint(equalToConstant: 1),
            v.heightAnchor.constraint(equalToConstant: 22),
        ])
        return v
    }

    // MARK: - Modifier toggle logic (tap = one-shot, double-tap = lock)

    private func toggleMod(active: inout Bool, locked: inout Bool) {
        if locked {
            active = false; locked = false
        } else if active {
            locked = true
        } else {
            active = true
        }
        updateButtonStyles()
    }

    private func updateButtonStyles() {
        styleModButton(ctrlButton,  active: modCtrl,  locked: modCtrlLocked)
        styleModButton(altButton,   active: modAlt,   locked: modAltLocked)
        styleModButton(superButton, active: modSuper, locked: modSuperLocked)
        styleModButton(shiftButton, active: modShift, locked: modShiftLocked)
    }

    private func styleModButton(_ btn: UIButton?, active: Bool, locked: Bool) {
        guard let btn else { return }
        if locked {
            btn.backgroundColor = UIColor.systemYellow.withAlphaComponent(0.35)
            btn.setTitleColor(.systemYellow, for: .normal)
        } else if active {
            btn.backgroundColor = UIColor.systemBlue.withAlphaComponent(0.25)
            btn.setTitleColor(.systemBlue, for: .normal)
        } else {
            btn.backgroundColor = .clear
            btn.setTitleColor(.label, for: .normal)
        }
    }

    // MARK: - Button actions

    @objc private func tapCtrl()  { toggleMod(active: &modCtrl,  locked: &modCtrlLocked) }
    @objc private func tapAlt()   { toggleMod(active: &modAlt,   locked: &modAltLocked) }
    @objc private func tapSuper() { toggleMod(active: &modSuper, locked: &modSuperLocked) }
    @objc private func tapShift() { toggleMod(active: &modShift, locked: &modShiftLocked) }

    @objc private func tapEsc() {
        let mods = consumeModifiers()
        onSpecial?(0x04, mods)
    }
    @objc private func tapTab() {
        let mods = consumeModifiers()
        onSpecial?(0x03, mods)
    }
    @objc private func tapFKey(_ sender: UIButton) {
        let mods = consumeModifiers()
        onSpecial?(UInt8(0x0F + sender.tag), mods)
    }
    @objc private func tapPgUp() { let m = consumeModifiers(); onSpecial?(0x1C, m) }
    @objc private func tapPgDn() { let m = consumeModifiers(); onSpecial?(0x1D, m) }
    @objc private func tapHome() { let m = consumeModifiers(); onSpecial?(0x0A, m) }
    @objc private func tapEnd()  { let m = consumeModifiers(); onSpecial?(0x0B, m) }
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

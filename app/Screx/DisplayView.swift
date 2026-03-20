import SwiftUI
import AVFoundation
import UIKit

// MARK: - Keyboard input proxy (captures iOS native keyboard and forwards to daemon)

struct KeyboardInputView: UIViewRepresentable {
    let isActive: Bool
    let onText: (String) -> Void
    let onDelete: () -> Void

    func makeUIView(context: Context) -> KeyInputProxyView {
        let view = KeyInputProxyView()
        view.onText = onText
        view.onDelete = onDelete
        view.isUserInteractionEnabled = false
        return view
    }

    func updateUIView(_ uiView: KeyInputProxyView, context: Context) {
        uiView.onText = onText
        uiView.onDelete = onDelete
        uiView.allowFirstResponder = isActive
        uiView.isUserInteractionEnabled = isActive
        if isActive && !uiView.isFirstResponder {
            DispatchQueue.main.async { uiView.becomeFirstResponder() }
        } else if !isActive && uiView.isFirstResponder {
            DispatchQueue.main.async { uiView.resignFirstResponder() }
        }
    }
}

final class KeyInputProxyView: UIView, UIKeyInput {
    var onText: ((String) -> Void)?
    var onDelete: (() -> Void)?
    var allowFirstResponder = false

    override var canBecomeFirstResponder: Bool { allowFirstResponder }
    var hasText: Bool { true }

    var autocorrectionType: UITextAutocorrectionType { .no }

    func insertText(_ text: String) {
        onText?(text)
    }

    func deleteBackward() {
        onDelete?()
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

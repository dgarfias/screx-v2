import SwiftUI
import AVFoundation

struct VideoDisplayView: UIViewRepresentable {
    let layer: AVSampleBufferDisplayLayer
    let onTouch: ((TouchEvent) -> Void)?

    func makeUIView(context: Context) -> DisplayContainerView {
        let view = DisplayContainerView()
        view.attach(layer: layer)
        view.onTouch = onTouch
        return view
    }

    func updateUIView(_ uiView: DisplayContainerView, context: Context) {
        uiView.attach(layer: layer)
        uiView.onTouch = onTouch
    }
}

enum TouchEventType: UInt8 {
    case tap = 0
    case down = 1
    case move = 2
    case up = 3
    case scroll = 4
    case rightClick = 5
}

struct TouchEvent {
    let type_: TouchEventType
    let x: UInt16
    let y: UInt16
    let dx: Int16
    let dy: Int16

    func serialize() -> Data {
        var data = Data("TOUCH".utf8)
        data.append(type_.rawValue)
        withUnsafeBytes(of: x.bigEndian) { data.append(contentsOf: $0) }
        withUnsafeBytes(of: y.bigEndian) { data.append(contentsOf: $0) }
        withUnsafeBytes(of: dx.bigEndian) { data.append(contentsOf: $0) }
        withUnsafeBytes(of: dy.bigEndian) { data.append(contentsOf: $0) }
        return data
    }
}

final class DisplayContainerView: UIView {
    private weak var attachedLayer: AVSampleBufferDisplayLayer?
    var onTouch: ((TouchEvent) -> Void)?
    private var isDragging = false

    override init(frame: CGRect) {
        super.init(frame: frame)
        isMultipleTouchEnabled = true

        let tap = UITapGestureRecognizer(target: self, action: #selector(handleTap(_:)))
        addGestureRecognizer(tap)

        let longPress = UILongPressGestureRecognizer(target: self, action: #selector(handleLongPress(_:)))
        longPress.minimumPressDuration = 0.5
        addGestureRecognizer(longPress)

        let pan = UIPanGestureRecognizer(target: self, action: #selector(handlePan(_:)))
        pan.minimumNumberOfTouches = 1
        pan.maximumNumberOfTouches = 1
        addGestureRecognizer(pan)

        let scroll = UIPanGestureRecognizer(target: self, action: #selector(handleScroll(_:)))
        scroll.minimumNumberOfTouches = 2
        scroll.maximumNumberOfTouches = 2
        addGestureRecognizer(scroll)

        tap.require(toFail: longPress)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) not supported")
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

    // Map a view-space point to the virtual display pixel coordinate.
    // The display layer uses .resizeAspect so we need to account for letterboxing.
    private func mapToDisplay(_ point: CGPoint) -> (UInt16, UInt16)? {
        guard let layer = attachedLayer else { return nil }

        let layerSize = layer.bounds.size
        guard layerSize.width > 0, layerSize.height > 0 else { return nil }

        // Get the video dimensions from the last format description if available
        // Fall back to the layer bounds ratio
        let videoSize = layer.bounds.size

        // With .resizeAspect, the video is centered and scaled to fit
        // Since our EVDI resolution matches the aspect ratio we configured,
        // the video fills the layer. Map proportionally.
        let nx = point.x / layerSize.width
        let ny = point.y / layerSize.height

        guard nx >= 0, nx <= 1, ny >= 0, ny <= 1 else { return nil }

        // We don't know the daemon resolution here, so send normalized 0..65535
        let x = UInt16(min(nx * 65535.0, 65535.0))
        let y = UInt16(min(ny * 65535.0, 65535.0))
        return (x, y)
    }

    @objc private func handleTap(_ gesture: UITapGestureRecognizer) {
        let loc = gesture.location(in: self)
        guard let (x, y) = mapToDisplay(loc) else { return }
        onTouch?(TouchEvent(type_: .tap, x: x, y: y, dx: 0, dy: 0))
    }

    @objc private func handleLongPress(_ gesture: UILongPressGestureRecognizer) {
        guard gesture.state == .began else { return }
        let loc = gesture.location(in: self)
        guard let (x, y) = mapToDisplay(loc) else { return }
        onTouch?(TouchEvent(type_: .rightClick, x: x, y: y, dx: 0, dy: 0))
    }

    @objc private func handlePan(_ gesture: UIPanGestureRecognizer) {
        let loc = gesture.location(in: self)
        guard let (x, y) = mapToDisplay(loc) else { return }

        switch gesture.state {
        case .began:
            isDragging = true
            onTouch?(TouchEvent(type_: .down, x: x, y: y, dx: 0, dy: 0))
        case .changed:
            onTouch?(TouchEvent(type_: .move, x: x, y: y, dx: 0, dy: 0))
        case .ended, .cancelled:
            isDragging = false
            onTouch?(TouchEvent(type_: .up, x: x, y: y, dx: 0, dy: 0))
        default:
            break
        }
    }

    @objc private func handleScroll(_ gesture: UIPanGestureRecognizer) {
        guard gesture.state == .changed else { return }
        let loc = gesture.location(in: self)
        guard let (x, y) = mapToDisplay(loc) else { return }

        let velocity = gesture.translation(in: self)
        gesture.setTranslation(.zero, in: self)

        // Convert to scroll ticks (negative Y = scroll up in Linux)
        let dy = Int16(max(-127, min(127, Int(-velocity.y / 8.0))))
        let dx = Int16(max(-127, min(127, Int(velocity.x / 8.0))))

        if dy != 0 || dx != 0 {
            onTouch?(TouchEvent(type_: .scroll, x: x, y: y, dx: dx, dy: dy))
        }
    }
}

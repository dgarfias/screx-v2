import SwiftUI
import AVFoundation

struct VideoDisplayView: UIViewRepresentable {
    let layer: AVSampleBufferDisplayLayer

    func makeUIView(context: Context) -> DisplayContainerView {
        let view = DisplayContainerView()
        view.attach(layer: layer)
        return view
    }

    func updateUIView(_ uiView: DisplayContainerView, context: Context) {
        uiView.attach(layer: layer)
    }
}

final class DisplayContainerView: UIView {
    private weak var attachedLayer: AVSampleBufferDisplayLayer?

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
}

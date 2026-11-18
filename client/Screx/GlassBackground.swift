import SwiftUI

extension View {
    /// Native Liquid Glass on supported systems, retaining the original material on older iPads.
    /// Apply after padding so the existing overlay's shape and dimensions stay unchanged.
    @ViewBuilder
    func screxGlassBackground<S: Shape>(in shape: S) -> some View {
        if #available(iOS 26.0, *) {
            glassEffect(.regular, in: shape)
        } else {
            background(.ultraThinMaterial, in: shape)
        }
    }
}

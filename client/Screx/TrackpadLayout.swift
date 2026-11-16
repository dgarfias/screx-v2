import Foundation
import CoreGraphics

/// Pure geometry, independent of SwiftUI's keyboard safe-area resizing. The saved frame is
/// always calculated from the full container; keyboard adjustments only affect its display.
enum TrackpadLayout {
    static func clamp(_ rect: CGRect, in size: CGSize, minimum: CGSize) -> CGRect {
        let w = min(max(rect.width, minimum.width), max(0, size.width * 0.9))
        let h = min(max(rect.height, minimum.height), max(0, size.height * 0.9))
        return CGRect(
            x: min(max(rect.minX, 0), max(0, size.width - w)),
            y: min(max(rect.minY, 0), max(0, size.height - h)),
            width: w, height: h
        )
    }

    static func avoidingKeyboard(_ rect: CGRect, in size: CGSize, keyboard: CGRect) -> CGRect {
        let obstruction = keyboard.intersection(CGRect(origin: .zero, size: size))
        guard !obstruction.isEmpty, rect.intersects(obstruction) else { return rect }
        let availableHeight = max(0, obstruction.minY - 8)
        let height = min(rect.height, availableHeight)
        return CGRect(x: rect.minX, y: min(rect.minY, availableHeight - height), width: rect.width, height: height)
    }
}

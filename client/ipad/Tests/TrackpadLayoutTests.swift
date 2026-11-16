import Foundation
import CoreGraphics

// Standalone tests: no simulator or code signing required.
// From client/ipad: swiftc Screx/TrackpadLayout.swift Tests/TrackpadLayoutTests.swift -o /tmp/screx-layout-tests && /tmp/screx-layout-tests
@main
struct TrackpadLayoutTests {
    static func main() {
        let size = CGSize(width: 1024, height: 768)
        let keyboard = CGRect(x: 0, y: 468, width: 1024, height: 300)
        let saved = CGRect(x: 30, y: 500, width: 300, height: 220)
        let adjusted = TrackpadLayout.avoidingKeyboard(saved, in: size, keyboard: keyboard)
        precondition(adjusted.size == saved.size, "Must move rather than shrink when space permits")
        precondition(adjusted.maxY == 460 && adjusted.minY >= 0, "Must remain visible above the keyboard")
        precondition(adjusted.minX == saved.minX, "Keyboard must not change horizontal position")

        let top = CGRect(x: 30, y: 20, width: 300, height: 220)
        precondition(TrackpadLayout.avoidingKeyboard(top, in: size, keyboard: keyboard) == top,
                     "Don't move a trackpad the keyboard doesn't block")

        let tall = CGRect(x: 30, y: 100, width: 300, height: 600)
        let shrunk = TrackpadLayout.avoidingKeyboard(tall, in: size, keyboard: keyboard)
        precondition(shrunk.height == 460 && shrunk.minY == 0, "Shrink only as much as necessary")
        precondition(shrunk.width == tall.width, "Keep width when height must shrink")
        precondition(TrackpadLayout.avoidingKeyboard(tall, in: size, keyboard: .null) == tall,
                     "Restore original frame when keyboard closes")

        let floating = CGRect(x: 700, y: 400, width: 300, height: 300)
        precondition(TrackpadLayout.avoidingKeyboard(saved, in: size, keyboard: floating) == saved,
                     "A floating keyboard elsewhere must not move the trackpad")
        let hidden = CGRect(x: 0, y: 768, width: 1024, height: 300)
        precondition(TrackpadLayout.avoidingKeyboard(saved, in: size, keyboard: hidden) == saved,
                     "An offscreen keyboard must not affect layout")

        let covered = TrackpadLayout.avoidingKeyboard(saved, in: size, keyboard: CGRect(origin: .zero, size: size))
        precondition(covered.height == 0 && covered.minY == 0, "No space must not place controls offscreen")
        print("Trackpad layout tests passed")
    }
}

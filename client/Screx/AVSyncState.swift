import Foundation
import QuartzCore

/// Where video frames land on the local clock relative to the daemon's timestamps. Audio and
/// video timestamps share the daemon's clock, so `AudioPlayer` uses this to keep audio from
/// playing too far behind the picture.
nonisolated final class AVSyncState: @unchecked Sendable {
    private let lock = NSLock()
    private var minOffsetMs: Double?
    private var lastUpdateMs: Double = 0

    /// No video for this long means there's nothing to stay in sync with.
    private static let staleAfterMs: Double = 1000
    /// A jump this large is a new daemon clock (restart), not network delay.
    private static let resetBeyondMs: Double = 5000
    /// How far the minimum creeps toward each newer offset, so it follows clock drift and
    /// route changes instead of holding one lucky sample forever.
    private static let creep: Double = 0.002

    init() {}

    func updateVideo(timestamp: UInt32) {
        let now = CACurrentMediaTime() * 1000
        let offset = now - Double(timestamp)
        lock.lock()
        if let current = minOffsetMs, abs(offset - current) < Self.resetBeyondMs {
            minOffsetMs = offset < current ? offset : current + (offset - current) * Self.creep
        } else {
            minOffsetMs = offset
        }
        lastUpdateMs = now
        lock.unlock()
    }

    /// The smallest recent (local arrival time - daemon timestamp) of video frames, in ms; nil
    /// when no video arrived in the last second.
    func videoOffsetMs() -> Double? {
        let now = CACurrentMediaTime() * 1000
        lock.lock()
        defer { lock.unlock() }
        guard now - lastUpdateMs < Self.staleAfterMs else { return nil }
        return minOffsetMs
    }
}

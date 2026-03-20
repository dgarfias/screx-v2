import Foundation
import QuartzCore

final class AVSyncState: @unchecked Sendable {
    private let lock = NSLock()
    private var videoTs: UInt32 = 0
    private var videoWallTime: Double = 0

    nonisolated init() {}

    nonisolated func updateVideo(timestamp: UInt32) {
        lock.lock()
        videoTs = timestamp
        videoWallTime = CACurrentMediaTime()
        lock.unlock()
    }

    nonisolated func expectedDaemonTimeNow() -> UInt32 {
        lock.lock()
        let vt = videoTs
        let vw = videoWallTime
        lock.unlock()
        guard vw > 0 else { return 0 }
        let elapsed = (CACurrentMediaTime() - vw) * 1000
        return vt &+ UInt32(elapsed)
    }

    nonisolated var isValid: Bool {
        lock.lock()
        let vw = videoWallTime
        lock.unlock()
        guard vw > 0 else { return false }
        return (CACurrentMediaTime() - vw) < 1.0
    }
}

import Foundation
import QuartzCore

final class AVSyncState: @unchecked Sendable {
    private let lock = NSLock()
    private var videoTs: UInt32 = 0
    private var videoWallTime: Double = 0
    private var _didResumeAfterGap = false

    /// How long without a video frame before we consider it a gap
    private static let gapThreshold: Double = 2.0

    nonisolated init() {}

    nonisolated func updateVideo(timestamp: UInt32) {
        lock.lock()
        let now = CACurrentMediaTime()
        let gap = videoWallTime > 0 && (now - videoWallTime) >= Self.gapThreshold
        videoTs = timestamp
        videoWallTime = now
        if gap {
            _didResumeAfterGap = true
        }
        lock.unlock()
    }

    /// Returns true once after video resumes from a gap. Consuming — resets after read.
    nonisolated func consumeGapResume() -> Bool {
        lock.lock()
        let val = _didResumeAfterGap
        _didResumeAfterGap = false
        lock.unlock()
        return val
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

import AVFoundation
import QuartzCore

final class AudioPlayer {
    private let engine = AVAudioEngine()
    private var sourceNode: AVAudioSourceNode?
    private let avSync: AVSyncState

    private let lock = NSLock()
    private var ringBuffer = Data()
    private static let maxBufferSize = 48000 * 2 * 2 // ~500ms at 48kHz stereo i16

    // 48kHz stereo s16le = 192000 bytes/sec = 192 bytes/ms
    private static let bytesPerMs = 192
    // Target buffer: ~30ms of audio for jitter absorption
    private static let targetBufferMs = 30
    private static let targetBufferBytes = targetBufferMs * bytesPerMs
    // Drift thresholds (ms) before corrective action
    private static let driftDropThresholdMs: Int32 = -40
    private static let driftTrimThresholdMs: Int32 = 60

    init(avSync: AVSyncState) {
        self.avSync = avSync

        let format = AVAudioFormat(
            commonFormat: .pcmFormatInt16,
            sampleRate: 48000,
            channels: 2,
            interleaved: true
        )!

        let node = AVAudioSourceNode(format: format) { [weak self] _, _, frameCount, bufferList -> OSStatus in
            guard let self else { return noErr }

            let ablPointer = UnsafeMutableAudioBufferListPointer(bufferList)
            let bytesNeeded = Int(frameCount) * 2 * 2

            self.lock.lock()
            let available = self.ringBuffer.count

            if available >= bytesNeeded {
                let chunk = self.ringBuffer.prefix(bytesNeeded)
                self.ringBuffer.removeFirst(bytesNeeded)
                self.lock.unlock()

                for i in 0..<ablPointer.count {
                    guard let dest = ablPointer[i].mData else { continue }
                    chunk.copyBytes(to: dest.assumingMemoryBound(to: UInt8.self), count: bytesNeeded)
                    ablPointer[i].mDataByteSize = UInt32(bytesNeeded)
                }
            } else {
                self.lock.unlock()
                for i in 0..<ablPointer.count {
                    guard let dest = ablPointer[i].mData else { continue }
                    memset(dest, 0, bytesNeeded)
                    ablPointer[i].mDataByteSize = UInt32(bytesNeeded)
                }
            }

            return noErr
        }

        self.sourceNode = node
        engine.attach(node)
        engine.connect(node, to: engine.mainMixerNode, format: format)
    }

    func start() {
        do {
            try engine.start()
            print("[audio] playback engine started")
        } catch {
            print("[audio] engine start failed: \(error)")
        }
    }

    func stop() {
        engine.stop()
        lock.lock()
        ringBuffer.removeAll()
        lock.unlock()
        print("[audio] playback engine stopped")
    }

    func enqueueAudio(_ data: Data, timestampMs: UInt32 = 0) {
        lock.lock()
        ringBuffer.append(data)

        if avSync.isValid {
            let expectedTs = avSync.expectedDaemonTimeNow()
            let drift = Int32(bitPattern: timestampMs) &- Int32(bitPattern: expectedTs)

            if drift < Self.driftDropThresholdMs {
                // Audio is behind video — drop oldest audio to catch up
                let dropBytes = min(ringBuffer.count, Int(-drift) * Self.bytesPerMs)
                // Keep aligned to frame boundary (4 bytes = one stereo s16 sample)
                let aligned = (dropBytes / 4) * 4
                if aligned > 0 {
                    ringBuffer.removeFirst(aligned)
                }
            } else if drift > Self.driftTrimThresholdMs {
                // Audio is ahead of video — trim if buffer grew too large
                let excess = ringBuffer.count - Self.targetBufferBytes
                if excess > 0 {
                    let aligned = (excess / 4) * 4
                    ringBuffer.removeFirst(aligned)
                }
            }
        }

        if ringBuffer.count > Self.maxBufferSize {
            let excess = ringBuffer.count - Self.maxBufferSize
            ringBuffer.removeFirst(excess)
        }
        lock.unlock()
    }
}

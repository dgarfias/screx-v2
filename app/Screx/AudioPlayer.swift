import AVFoundation
import QuartzCore

final class AudioPlayer {
    private let engine = AVAudioEngine()
    private var sourceNode: AVAudioSourceNode?
    private let avSync: AVSyncState
    private(set) var isOutputEnabled = true

    private let lock = NSLock()
    private static let maxBufferSize = 48000 * 2 * 2 // ~500ms at 48kHz stereo i16

    // Circular buffer: O(1) read and discard instead of O(n) Data.removeFirst
    private var ringStorage = UnsafeMutablePointer<UInt8>.allocate(capacity: AudioPlayer.maxBufferSize)
    private var ringReadPos = 0
    private var ringWritePos = 0
    private var ringCount = 0
    private let ringCapacity = AudioPlayer.maxBufferSize

    // 48kHz stereo s16le = 192000 bytes/sec = 192 bytes/ms
    private static let bytesPerMs = 192
    // Target buffer: ~30ms of audio for jitter absorption
    private static let targetBufferMs = 30
    private static let targetBufferBytes = targetBufferMs * bytesPerMs
    // Drift thresholds (ms) before corrective action
    private static let driftDropThresholdMs: Int32 = -40
    private static let driftTrimThresholdMs: Int32 = 60

    deinit {
        ringStorage.deallocate()
    }

    private func ringWrite(_ src: UnsafePointer<UInt8>, count: Int) {
        let toWrite = min(count, ringCapacity - ringCount)
        guard toWrite > 0 else { return }
        let firstChunk = min(toWrite, ringCapacity - ringWritePos)
        ringStorage.advanced(by: ringWritePos).update(from: src, count: firstChunk)
        if firstChunk < toWrite {
            ringStorage.update(from: src.advanced(by: firstChunk), count: toWrite - firstChunk)
        }
        ringWritePos = (ringWritePos + toWrite) % ringCapacity
        ringCount += toWrite
    }

    private func ringRead(into dest: UnsafeMutablePointer<UInt8>, count: Int) {
        let toRead = min(count, ringCount)
        let firstChunk = min(toRead, ringCapacity - ringReadPos)
        dest.update(from: ringStorage.advanced(by: ringReadPos), count: firstChunk)
        if firstChunk < toRead {
            dest.advanced(by: firstChunk).update(from: ringStorage, count: toRead - firstChunk)
        }
        ringReadPos = (ringReadPos + toRead) % ringCapacity
        ringCount -= toRead
    }

    private func ringDiscard(_ n: Int) {
        let toDiscard = min(n, ringCount)
        ringReadPos = (ringReadPos + toDiscard) % ringCapacity
        ringCount -= toDiscard
    }

    private func ringClear() {
        ringReadPos = 0
        ringWritePos = 0
        ringCount = 0
    }

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

            if self.ringCount >= bytesNeeded {
                var firstDest: UnsafeMutablePointer<UInt8>?
                for i in 0..<ablPointer.count {
                    guard let dest = ablPointer[i].mData else { continue }
                    let ptr = dest.assumingMemoryBound(to: UInt8.self)
                    if firstDest == nil {
                        self.ringRead(into: ptr, count: bytesNeeded)
                        firstDest = ptr
                    } else {
                        memcpy(ptr, firstDest!, bytesNeeded)
                    }
                    ablPointer[i].mDataByteSize = UInt32(bytesNeeded)
                }
                self.lock.unlock()
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
        guard isOutputEnabled else { return }
        guard !engine.isRunning else { return }
        do {
            try engine.start()
            print("[audio] playback engine started")
        } catch {
            print("[audio] engine start failed: \(error)")
        }
    }

    func stop() {
        guard engine.isRunning || ringCount > 0 else { return }
        engine.stop()
        lock.lock()
        ringClear()
        lock.unlock()
        print("[audio] playback engine stopped")
    }

    func setOutputEnabled(_ enabled: Bool) {
        guard enabled != isOutputEnabled else { return }
        isOutputEnabled = enabled
        if enabled {
            start()
        } else {
            stop()
        }
    }

    func enqueueAudio(_ data: Data, timestampMs: UInt32 = 0) {
        guard isOutputEnabled else { return }
        if avSync.consumeGapResume() {
            lock.lock()
            ringClear()
            lock.unlock()
        }

        lock.lock()
        data.withUnsafeBytes { raw in
            guard let ptr = raw.baseAddress?.assumingMemoryBound(to: UInt8.self) else { return }
            ringWrite(ptr, count: raw.count)
        }

        if avSync.isValid {
            let expectedTs = avSync.expectedDaemonTimeNow()
            let drift = Int32(bitPattern: timestampMs) &- Int32(bitPattern: expectedTs)

            if drift < Self.driftDropThresholdMs {
                let dropBytes = min(ringCount, Int(-drift) * Self.bytesPerMs)
                let aligned = (dropBytes / 4) * 4
                if aligned > 0 {
                    ringDiscard(aligned)
                }
            } else if drift > Self.driftTrimThresholdMs {
                let excess = ringCount - Self.targetBufferBytes
                if excess > 0 {
                    let aligned = (excess / 4) * 4
                    ringDiscard(aligned)
                }
            }
        }

        if ringCount > Self.maxBufferSize {
            let excess = ringCount - Self.maxBufferSize
            ringDiscard(excess)
        }
        lock.unlock()
    }
}

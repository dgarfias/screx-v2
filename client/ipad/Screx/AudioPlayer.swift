import AVFoundation
import QuartzCore
import Opus
import os

final class AudioPlayer {
    private let engine = AVAudioEngine()
    private var sourceNode: AVAudioSourceNode?
    private let avSync: AVSyncState
    private(set) var isOutputEnabled = true

    // NOTE: The ideal implementation here is a lock-free SPSC ring using
    // atomics for head (render thread) and tail (network thread). Adding
    // swift-atomics to the Xcode project was not practical in this environment,
    // so we use OSAllocatedUnfairLock as a fast, modern fallback. The render
    // callback never allocates inside the lock and emits silence when data is
    // unavailable, keeping latency predictable.
    private let lock = OSAllocatedUnfairLock()
    private static let maxBufferSize = 48000 * 2 * 2 // ~500ms at 48kHz stereo i16

    // Circular buffer: O(1) read and discard instead of O(n) Data.removeFirst
    private var ringStorage = UnsafeMutablePointer<UInt8>.allocate(capacity: AudioPlayer.maxBufferSize)
    private var ringReadPos = 0
    private var ringWritePos = 0
    private var ringCount = 0
    private var droppedPacketCount = 0
    private let ringCapacity = AudioPlayer.maxBufferSize

    // 48kHz stereo s16le = 192000 bytes/sec = 192 bytes/ms
    private static let bytesPerMs = 192
    // Target buffer: ~50ms of audio for jitter absorption. iOS render is 10ms,
    // so this is a 5-buffer budget — deep enough to ride over a video burst
    // (an IDR can momentarily queue a few ms of audio behind it on the shared
    // socket) without adding perceptible latency. Moonlight/Parsec target
    // roughly this range rather than a minimal 1-2 buffer.
    private static let targetBufferMs = 50
    private static let targetBufferBytes = targetBufferMs * bytesPerMs
    // Drift thresholds (ms) before corrective action
    private static let driftDropThresholdMs: Int32 = -40
    private static let driftTrimThresholdMs: Int32 = 90

    // Standing-latency trim: underruns ratchet the ring depth up (silence is
    // played, then the late audio arrives and queues behind), and timestamp
    // drift stays ~0 so the drift logic above never corrects it. Track the
    // minimum occupancy over a rolling window; if even the low point exceeds
    // the threshold, that excess is standing latency — trim back to target.
    private static let latencyTrimWindowFrames = 48000 * 3 // 3s at 48kHz
    private static let latencyTrimThresholdBytes = (targetBufferMs + 30) * bytesPerMs
    private var windowMinOccupancy = Int.max
    private var windowFrames = 0

    deinit {
        ringStorage.deallocate()
    }

    @discardableResult
    private func ringWrite(_ src: UnsafePointer<UInt8>, count: Int) -> Bool {
        // Drop the whole packet if it doesn't fit. Do not advance the consumer
        // index; the render thread owns readPos/ringCount.
        guard count <= ringCapacity - ringCount else { return false }
        let firstChunk = min(count, ringCapacity - ringWritePos)
        ringStorage.advanced(by: ringWritePos).update(from: src, count: firstChunk)
        if firstChunk < count {
            ringStorage.update(from: src.advanced(by: firstChunk), count: count - firstChunk)
        }
        ringWritePos = (ringWritePos + count) % ringCapacity
        ringCount += count
        return true
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
        windowMinOccupancy = Int.max
        windowFrames = 0
    }

    init(avSync: AVSyncState) {
        self.avSync = avSync

        let format = Self.pcmFormat

        let node = AVAudioSourceNode(format: format) { [weak self] _, _, frameCount, bufferList -> OSStatus in
            guard let self else { return noErr }

            let ablPointer = UnsafeMutableAudioBufferListPointer(bufferList)
            let bytesNeeded = Int(frameCount) * 2 * 2

            self.lock.withLock {
                // Occupancy right before a render pull is the standing latency.
                if self.ringCount < self.windowMinOccupancy {
                    self.windowMinOccupancy = self.ringCount
                }
                self.windowFrames += Int(frameCount)
                if self.windowFrames >= Self.latencyTrimWindowFrames {
                    if self.windowMinOccupancy > Self.latencyTrimThresholdBytes {
                        let excess = self.windowMinOccupancy - Self.targetBufferBytes
                        self.ringDiscard((excess / 4) * 4)
                    }
                    self.windowFrames = 0
                    self.windowMinOccupancy = Int.max
                }

                var firstDest: UnsafeMutablePointer<UInt8>?
                for i in 0..<ablPointer.count {
                    guard let dest = ablPointer[i].mData else { continue }
                    let ptr = dest.assumingMemoryBound(to: UInt8.self)
                    if firstDest == nil {
                        let bytesToRead = min(bytesNeeded, self.ringCount)
                        self.ringRead(into: ptr, count: bytesToRead)
                        if bytesToRead < bytesNeeded {
                            memset(ptr.advanced(by: bytesToRead), 0, bytesNeeded - bytesToRead)
                        }
                        firstDest = ptr
                    } else {
                        memcpy(ptr, firstDest!, bytesNeeded)
                    }
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
        lock.withLock {
            ringClear()
        }
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

    // Speaker audio arrives as Opus (48 kHz stereo, 10 ms packets) when the daemon
    // honors `StreamSettings.featureOpusAudio`, and as raw s16le PCM otherwise (older
    // daemons, USB). The decoder is created lazily; if it fails to initialize, Opus
    // packets are dropped rather than crashing. The decode buffer is reused for every
    // packet; 5760 frames (120 ms) is far more than the 480-frame packets expected.
    private static let pcmFormat = AVAudioFormat(
        commonFormat: .pcmFormatInt16,
        sampleRate: 48000,
        channels: 2,
        interleaved: true
    )!
    private static let opusDecodeCapacity: AVAudioFrameCount = 5760

    private lazy var opusDecoder: Opus.Decoder? = {
        do {
            return try Opus.Decoder(format: Self.pcmFormat, application: .audio)
        } catch {
            print("[audio] Opus decoder init failed, Opus audio will be dropped: \(error)")
            return nil
        }
    }()
    private lazy var opusDecodeBuffer: AVAudioPCMBuffer? =
        AVAudioPCMBuffer(pcmFormat: Self.pcmFormat, frameCapacity: Self.opusDecodeCapacity)

    /// Decodes one Opus packet and feeds the PCM into the jitter buffer.
    func enqueueOpus(_ packet: Data, timestampMs: UInt32 = 0) {
        guard isOutputEnabled, !packet.isEmpty else { return }
        guard let opusDecoder, let opusDecodeBuffer else { return }

        do {
            try packet.withUnsafeBytes { raw in
                try opusDecoder.decode(raw.bindMemory(to: UInt8.self), to: opusDecodeBuffer)
            }
        } catch {
            print("[audio] Opus decode failed, dropping packet: \(error)")
            return
        }

        guard opusDecodeBuffer.frameLength > 0,
              let channelData = opusDecodeBuffer.int16ChannelData else { return }
        let byteCount = Int(opusDecodeBuffer.frameLength) * 2 * 2 // frames * channels * bytes
        channelData[0].withMemoryRebound(to: UInt8.self, capacity: byteCount) { ptr in
            enqueuePCM(ptr, count: byteCount, timestampMs: timestampMs)
        }
    }

    /// Feeds raw interleaved s16le stereo PCM into the jitter buffer.
    func enqueueAudio(_ data: Data, timestampMs: UInt32 = 0) {
        guard isOutputEnabled else { return }
        data.withUnsafeBytes { raw in
            guard let ptr = raw.baseAddress?.assumingMemoryBound(to: UInt8.self) else { return }
            enqueuePCM(ptr, count: raw.count, timestampMs: timestampMs)
        }
    }

    private func enqueuePCM(_ ptr: UnsafePointer<UInt8>, count: Int, timestampMs: UInt32) {
        if avSync.consumeGapResume() {
            lock.withLock {
                ringClear()
            }
        }

        lock.withLock {
            if !ringWrite(ptr, count: count) {
                droppedPacketCount += 1
            }

            if avSync.isValid {
                let expectedTs = avSync.expectedDaemonTimeNow()
                let drift = Int32(bitPattern: timestampMs) &- Int32(bitPattern: expectedTs)

                // Negative drift = this audio segment's nominal time is behind
                // the video clock. That is normal after any arrival burst or
                // pause (PacketAudio/tsched, IDR encodes) and is NOT a reason
                // to throw audio away — the jitter buffer absorbs it. Only
                // clip at extreme lateness (>100ms, ~10 full packets) and even
                // then never drop more than half the ring, so a transient can
                // never punch a hole in playback.
                if drift < Self.driftDropThresholdMs {
                    let lateMs = Int(-drift)
                    if lateMs > 100 && ringCount > 0 {
                        let dropBytes = min(ringCount / 2, lateMs * Self.bytesPerMs)
                        let aligned = (dropBytes / 4) * 4
                        if aligned > 0 {
                            ringDiscard(aligned)
                        }
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
        }
    }
}

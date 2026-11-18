import AVFoundation
import Copus
import QuartzCore
import os

/// Plays the daemon's speaker stream. Packets go into `AudioJitterBuffer` as they arrive and
/// are decoded on the render thread when they are due, so a lost or late packet becomes Opus
/// packet-loss concealment instead of silence or added delay. See "Speaker audio" in
/// docs/ARCHITECTURE.md.
final class AudioPlayer {
    private let engine = AVAudioEngine()
    private var sourceNode: AVAudioSourceNode?
    private let avSync: AVSyncState
    private(set) var isOutputEnabled = true
    private var wantsPlayback = false
    private var configurationObserver: NSObjectProtocol?
    private var routeObserver: NSObjectProtocol?

    /// Guards `jitter`, `generation` and `decoderResetPending`. The render thread holds it only
    /// to copy out the next packet; decoding happens outside it.
    private let lock = OSAllocatedUnfairLock()
    private let jitter = AudioJitterBuffer()
    private var generation: UInt32 = 0
    private var decoderResetPending = false

    /// Audio may play at most this far behind the matching video.
    private static let maxVideoLagMs = 100.0
    private static let sampleRate = 48000
    private static let framesPerMs = Double(sampleRate) / 1000
    private static let packetFrames = 480
    /// Largest decode, in frames: room for a 20 ms packet though the daemon sends 10 ms.
    private static let maxDecodeFrames = 960

    // Render thread only.
    private let decoder: OpaquePointer?
    private let pcm = UnsafeMutablePointer<Int16>.allocate(capacity: AudioPlayer.maxDecodeFrames * 2)
    private let mergePcm = UnsafeMutablePointer<Int16>.allocate(capacity: AudioPlayer.maxDecodeFrames * 2)
    private let packetA = UnsafeMutablePointer<UInt8>.allocate(capacity: AudioJitterBuffer.maxPacketBytes)
    private let packetB = UnsafeMutablePointer<UInt8>.allocate(capacity: AudioJitterBuffer.maxPacketBytes)
    private var pcmFrames = 0
    private var pcmPosition = 0

    private static let pcmFormat = AVAudioFormat(
        commonFormat: .pcmFormatInt16,
        sampleRate: 48000,
        channels: 2,
        interleaved: true
    )!

    deinit {
        if let configurationObserver { NotificationCenter.default.removeObserver(configurationObserver) }
        if let routeObserver { NotificationCenter.default.removeObserver(routeObserver) }
        engine.stop()
        if let decoder { opus_decoder_destroy(decoder) }
        pcm.deallocate()
        mergePcm.deallocate()
        packetA.deallocate()
        packetB.deallocate()
    }

    init(avSync: AVSyncState) {
        self.avSync = avSync
        var error: Int32 = 0
        let created = opus_decoder_create(Int32(Self.sampleRate), 2, &error)
        if error == 0, let created {
            decoder = created
        } else {
            print("[audio] Opus decoder init failed (\(error)); speaker audio will be silent")
            decoder = nil
        }

        let node = AVAudioSourceNode(format: Self.pcmFormat) { [weak self] _, _, frameCount, bufferList -> OSStatus in
            self?.render(frameCount: Int(frameCount), bufferList: bufferList)
            return noErr
        }

        self.sourceNode = node
        engine.attach(node)
        engine.connect(node, to: engine.mainMixerNode, format: Self.pcmFormat)
        configurationObserver = NotificationCenter.default.addObserver(forName: .AVAudioEngineConfigurationChange,
                                                                        object: engine, queue: .main) { [weak self] _ in
            guard let self, self.wantsPlayback, self.isOutputEnabled, !self.engine.isRunning else { return }
            // The engine stops when the hardware route's format changes (e.g. HDMI hotplug).
            self.engine.stop()
            self.resetPlayback(newStream: false)
            self.start()
        }
        routeObserver = NotificationCenter.default.addObserver(forName: AVAudioSession.routeChangeNotification,
                                                               object: AVAudioSession.sharedInstance(), queue: .main) { _ in
            let outputs = AVAudioSession.sharedInstance().currentRoute.outputs
            print("[audio] route changed: \(outputs.map { "\($0.portType.rawValue):\($0.portName)" }.joined(separator: ", "))")
        }
    }

    func start() {
        guard isOutputEnabled else { return }
        wantsPlayback = true
        guard !engine.isRunning else { return }
        do {
            try engine.start()
            print("[audio] playback engine started")
        } catch {
            print("[audio] engine start failed: \(error)")
        }
    }

    func stop() {
        wantsPlayback = false
        let wasRunning = engine.isRunning
        engine.stop()
        resetPlayback(newStream: true)
        if wasRunning { print("[audio] playback engine stopped") }
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

    /// Runs `body` holding `lock`.
    private func locked<R>(_ body: () -> R) -> R {
        lock.lock()
        defer { lock.unlock() }
        return body()
    }

    /// Empties the jitter buffer; the render thread resets the decoder before its next packet.
    /// `newStream` also forgets the learned target, for a new session that may be on another
    /// network; an output route change keeps it.
    private func resetPlayback(newStream: Bool) {
        locked {
            jitter.reset(newStream: newStream)
            decoderResetPending = true
        }
    }

    /// Queues the packets of one speaker datagram: the one named by `frameId` and the redundant
    /// copies of earlier ones. Called on the stream queue. `receiveTimeNs` is when the network
    /// stack received the datagram (`CLOCK_MONOTONIC_RAW`), 0 if unknown.
    func enqueue(payload: Data, entries: [SpeakerAudioEntry], frameId: UInt32, timestampMs: UInt32,
                 generation: UInt32, receiveTimeNs: UInt64) {
        guard isOutputEnabled else { return }
        let nowMs = CACurrentMediaTime() * 1000
        var processingMs: Double?
        if receiveTimeNs > 0 {
            let nowNs = clock_gettime_nsec_np(CLOCK_MONOTONIC_RAW)
            if nowNs >= receiveTimeNs {
                processingMs = Double(nowNs - receiveTimeNs) / 1_000_000
            }
        }
        let videoOffsetMs = avSync.videoOffsetMs()
        payload.withUnsafeBytes { raw in
            locked {
                if generation != self.generation {
                    // A new session (or a reconnect on another network): start from scratch.
                    self.generation = generation
                    jitter.reset(newStream: true)
                    decoderResetPending = true
                }
                for entry in entries {
                    let seq = Int64(frameId) - Int64(entry.distance)
                    guard seq >= 0 else { continue }
                    let start = entry.opus.lowerBound - payload.startIndex
                    let bytes = UnsafeRawBufferPointer(rebasing: raw[start..<(start + entry.opus.count)])
                    jitter.insert(seq: seq,
                                  timestampMs: timestampMs &- UInt32(entry.distance) &* 10,
                                  isPrimary: entry.distance == 0,
                                  bytes: bytes,
                                  nowMs: nowMs,
                                  processingMs: processingMs)
                }
                // Audio and video timestamps share the daemon's clock. Audio plays about
                // (its arrival offset - video's) + depth + one render quantum behind the
                // picture, so cap the depth to keep that within `maxVideoLagMs`; past it, late
                // packets are concealed instead of delaying everything.
                if let videoOffsetMs, let audioOffsetMs = jitter.minOffsetMs {
                    jitter.maxTargetMs = Self.maxVideoLagMs - (audioOffsetMs - videoOffsetMs) - jitter.renderQuantumMs
                } else {
                    jitter.maxTargetMs = .infinity
                }
            }
        }
    }

    /// The playout report for the daemon, or nil while nothing is playing.
    func takeReport() -> AudioPlayoutReport? {
        guard isOutputEnabled else { return nil }
        return locked {
            generation == 0 ? nil : jitter.takeReport(generation: generation)
        }
    }

    // MARK: - Render thread

    private func render(frameCount: Int, bufferList: UnsafeMutablePointer<AudioBufferList>) {
        let buffers = UnsafeMutableAudioBufferListPointer(bufferList)
        guard buffers.count > 0, let first = buffers[0].mData else { return }
        let out = first.assumingMemoryBound(to: Int16.self)
        var written = 0
        while written < frameCount {
            if pcmPosition >= pcmFrames {
                refill(renderFrames: frameCount)
            }
            let n = min(frameCount - written, pcmFrames - pcmPosition)
            (out + written * 2).update(from: pcm + pcmPosition * 2, count: n * 2)
            written += n
            pcmPosition += n
        }
        let byteCount = frameCount * 4
        buffers[0].mDataByteSize = UInt32(byteCount)
        for i in 1..<buffers.count {
            guard let dest = buffers[i].mData else { continue }
            memcpy(dest, first, byteCount)
            buffers[i].mDataByteSize = UInt32(byteCount)
        }
    }

    /// Decodes the next packet (or conceals, merges, or plays silence) into `pcm`.
    private func refill(renderFrames: Int) {
        let (action, resetDecoder) = locked { () -> (AudioJitterBuffer.Action, Bool) in
            let reset = decoderResetPending
            decoderResetPending = false
            jitter.renderQuantumMs = Double(renderFrames) / Self.framesPerMs
            return (jitter.pull(queuedMs: 0, a: packetA, b: packetB), reset)
        }
        if resetDecoder, let decoder {
            opus_decoder_init(decoder, Int32(Self.sampleRate), 2)
        }

        var frames: Int
        switch action {
        case .decode(let length):
            frames = decode(packetA, length: length, into: pcm)
        case .conceal:
            frames = decode(nil, length: 0, into: pcm)
        case .merge(let first, let second):
            let n = decode(first == nil ? nil : packetA, length: first ?? 0, into: pcm)
            let m = decode(second == nil ? nil : packetB, length: second ?? 0, into: mergePcm)
            frames = min(n, m)
            crossfade(into: pcm, from: mergePcm, frames: frames)
        case .silence:
            frames = 0
        }
        if frames <= 0 {
            frames = Self.packetFrames
            pcm.update(repeating: 0, count: frames * 2)
        }
        pcmFrames = frames
        pcmPosition = 0
    }

    /// Decodes one packet, or conceals one 10 ms packet when `packet` is nil. Returns frames.
    private func decode(_ packet: UnsafeMutablePointer<UInt8>?, length: Int, into out: UnsafeMutablePointer<Int16>) -> Int {
        guard let decoder else { return 0 }
        let result: Int32
        if let packet {
            result = opus_decode(decoder, packet, Int32(length), out, Int32(Self.maxDecodeFrames), 0)
        } else {
            result = opus_decode(decoder, nil, 0, out, Int32(Self.packetFrames), 0)
        }
        return max(0, Int(result))
    }

    /// Crossfades `pcm` (fading out) into `other` (fading in), interleaved stereo.
    private func crossfade(into pcm: UnsafeMutablePointer<Int16>, from other: UnsafeMutablePointer<Int16>, frames: Int) {
        guard frames > 0 else { return }
        for frame in 0..<frames {
            let w = Float(frame) / Float(frames)
            for channel in 0..<2 {
                let i = frame * 2 + channel
                pcm[i] = Int16(Float(pcm[i]) * (1 - w) + Float(other[i]) * w)
            }
        }
    }
}

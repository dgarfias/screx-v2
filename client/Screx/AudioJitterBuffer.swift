import Foundation

// Speaker audio playout logic, free of AVFoundation so it can be tested on its own. See
// "Speaker audio" in docs/ARCHITECTURE.md for the wire format and the behavior.

/// One entry of a speaker datagram: the packet `distance` frame IDs before the header's, with
/// its Opus bytes at `opus` (indices into the payload).
struct SpeakerAudioEntry: Equatable {
    let distance: UInt8
    let opus: Range<Int>
}

enum SpeakerAudioPayload {
    static let maxDistance: UInt8 = 8
    static let maxEntries = 4

    /// `count(u8)` then entries of `distance(u8) + len(u16) + opus`. The first entry has
    /// distance 0; distances strictly increase. Nil if malformed.
    static func parse(_ payload: Data) -> [SpeakerAudioEntry]? {
        guard let count = payload.first, count >= 1, Int(count) <= maxEntries else { return nil }
        var entries: [SpeakerAudioEntry] = []
        entries.reserveCapacity(Int(count))
        var offset = payload.startIndex + 1
        var lastDistance = -1
        for index in 0..<Int(count) {
            guard payload.endIndex - offset >= 3 else { return nil }
            let distance = payload[offset]
            let length = Int(payload[offset + 1]) << 8 | Int(payload[offset + 2])
            offset += 3
            guard length > 0, payload.endIndex - offset >= length,
                  (index == 0) == (distance == 0),
                  Int(distance) > lastDistance, distance <= maxDistance else { return nil }
            lastDistance = Int(distance)
            entries.append(SpeakerAudioEntry(distance: distance, opus: offset..<(offset + length)))
            offset += length
        }
        guard offset == payload.endIndex else { return nil }
        return entries
    }
}

/// UDP `AUDIO_REPORT` body.
struct AudioPlayoutReport: Equatable {
    var generation: UInt32
    var depthUs: UInt32
    var targetUs: UInt32
    var due: UInt16
    var primaryLost: UInt16
    var recovered: UInt16
    var concealed: UInt16
    var late: UInt16
    var maxBurst: UInt8
    /// Jitter estimate before the render quantum and the video cap are applied.
    var jitterUs: UInt32
    /// The video cap on the target; 0 when there is none.
    var capUs: UInt32
    /// Longest time a speaker datagram waited on the iPad between the network stack receiving it
    /// and the jitter buffer storing it; 0 when the receive time is unavailable.
    var maxProcessingUs: UInt32

    static let byteCount = 35

    func encoded() -> Data {
        var data = Data(capacity: Self.byteCount)
        for value in [generation, depthUs, targetUs] {
            withUnsafeBytes(of: value.bigEndian) { data.append(contentsOf: $0) }
        }
        for value in [due, primaryLost, recovered, concealed, late] {
            withUnsafeBytes(of: value.bigEndian) { data.append(contentsOf: $0) }
        }
        data.append(maxBurst)
        for value in [jitterUs, capUs, maxProcessingUs] {
            withUnsafeBytes(of: value.bigEndian) { data.append(contentsOf: $0) }
        }
        return data
    }
}

/// Estimates how deep the jitter buffer must be from packet arrival times. Each packet's
/// offset (local arrival time minus its daemon timestamp) is compared with the smallest offset
/// in the last 4 s; the 99th percentile of that extra delay, plus one packet, is the estimate.
/// It rises as soon as the measurement does and falls by at most 2 ms per second. Mirrors
/// `JitterEstimator` in daemon/src/audio.rs.
struct AudioJitterEstimator {
    static let windowMs = 4000.0
    static let updateMs = 500.0
    static let percentile = 0.99
    static let fallMsPerSecond = 2.0

    private let packetMs: Double
    private let ceilingMs: Double
    private let initialMs: Double
    private var offsets: [Double]
    private var head = 0
    private var count = 0
    private var sinceUpdate = 0
    private let updateEvery: Int
    private var scratch: [Double] = []
    private var lastUpdateMs: Double?
    private(set) var targetMs: Double
    /// Smallest offset in the window, for comparing with video.
    private(set) var minOffsetMs: Double?

    init(packetMs: Double, ceilingMs: Double, initialMs: Double) {
        self.packetMs = packetMs
        self.ceilingMs = ceilingMs
        self.initialMs = initialMs
        let capacity = Int(Self.windowMs / packetMs)
        offsets = [Double](repeating: 0, count: capacity)
        updateEvery = Int(Self.updateMs / packetMs)
        targetMs = initialMs
        scratch.reserveCapacity(capacity)
    }

    /// Forgets the measurements. A resync within a stream keeps the learned target; a new
    /// stream (another session or network) starts from the initial one.
    mutating func reset(keepTarget: Bool) {
        if !keepTarget { targetMs = initialMs }
        head = 0
        count = 0
        sinceUpdate = 0
        lastUpdateMs = nil
        minOffsetMs = nil
    }

    mutating func onArrival(offsetMs: Double, nowMs: Double) {
        offsets[head] = offsetMs
        head = (head + 1) % offsets.count
        count = min(count + 1, offsets.count)
        sinceUpdate += 1
        if sinceUpdate >= updateEvery {
            sinceUpdate = 0
            recompute(nowMs: nowMs)
        }
    }

    private mutating func recompute(nowMs: Double) {
        guard count > 0 else { return }
        scratch.removeAll(keepingCapacity: true)
        var minimum = Double.infinity
        for i in 0..<count {
            minimum = min(minimum, offsets[i])
        }
        for i in 0..<count {
            scratch.append(offsets[i] - minimum)
        }
        scratch.sort()
        let index = Int((Double(scratch.count - 1) * Self.percentile).rounded())
        let wanted = min(scratch[index] + packetMs, ceilingMs)
        if wanted >= targetMs {
            targetMs = wanted
        } else {
            let elapsed = lastUpdateMs.map { max(0, nowMs - $0) / 1000 } ?? 0
            targetMs = max(wanted, targetMs - Self.fallMsPerSecond * elapsed)
        }
        lastUpdateMs = nowMs
        minOffsetMs = minimum
    }
}

/// The speaker jitter buffer: Opus packets by packet ID, handed out in order as the render
/// thread needs them. Not thread-safe; `AudioPlayer` guards it with its lock. Packet bytes live
/// in preallocated storage so the render thread never allocates.
final class AudioJitterBuffer {
    enum Action: Equatable {
        /// Decode the packet whose bytes were copied into `a` (byte count).
        case decode(Int)
        /// Fill one packet with Opus packet-loss concealment.
        case conceal
        /// Decode two consecutive packets (bytes in `a` and `b`; nil = conceal that one) and
        /// crossfade them into one packet's length, draining 10 ms without a click.
        case merge(Int?, Int?)
        /// Nothing to play yet: one packet of silence.
        case silence
    }

    static let packetMs = 10.0
    static let slotCount = 64
    static let maxPacketBytes = 1500
    static let floorMs = 15.0
    static let ceilingMs = 200.0
    /// Concealment allowed with nothing buffered before going silent (30 ms).
    static let maxConcealedRun = 3
    /// A jump this large (500 ms) is a restarted stream, not loss.
    static let resyncGap: Int64 = 50
    /// Merge or insert once the smoothed depth is this far from the target: 15 ms, or 20% of
    /// the target if larger. Smaller errors are clock drift, which the daemon's pace follows.
    static let adjustBandMs = 15.0
    static let adjustBandFraction = 0.2
    /// At most one merge or insert per this many packets (200 ms), so the smoothed depth can
    /// catch up between adjustments.
    static let adjustCooldownPackets = 20

    private var seqs = [Int64](repeating: -1, count: AudioJitterBuffer.slotCount)
    private var lengths = [Int](repeating: 0, count: AudioJitterBuffer.slotCount)
    private var arrivedAsPrimary = [Bool](repeating: false, count: AudioJitterBuffer.slotCount)
    private let storage = UnsafeMutablePointer<UInt8>.allocate(
        capacity: AudioJitterBuffer.slotCount * AudioJitterBuffer.maxPacketBytes)

    /// Packet ID to play next; nil while (re)filling.
    private(set) var next: Int64?
    private(set) var highest: Int64?
    private var estimator = AudioJitterEstimator(packetMs: AudioJitterBuffer.packetMs,
                                                 ceilingMs: AudioJitterBuffer.ceilingMs,
                                                 initialMs: 20)
    private(set) var smoothedDepthMs = 0.0
    private var concealedRun = 0
    private var packetsSinceAdjust = 0

    /// Audio the output pulls at once; the target covers it.
    var renderQuantumMs = 5.0
    /// Upper bound from keeping audio near the video (see `AudioPlayer`).
    var maxTargetMs = Double.infinity

    // Report counters since the last `takeReport`.
    private var due = 0
    private var primaryLost = 0
    private var recovered = 0
    private var concealed = 0
    private var late = 0
    private var maxBurst = 0
    private var burst = 0
    private var maxProcessingMs = 0.0

    deinit {
        storage.deallocate()
    }

    var isPlaying: Bool { next != nil }

    var minOffsetMs: Double? { estimator.minOffsetMs }

    var targetMs: Double {
        let upper = max(Self.floorMs, min(Self.ceilingMs, maxTargetMs))
        return min(max(estimator.targetMs + renderQuantumMs, Self.floorMs), upper)
    }

    /// Empties the buffer. `newStream` (a new session) also forgets the learned target.
    func reset(newStream: Bool = false) {
        for i in 0..<Self.slotCount { seqs[i] = -1 }
        next = nil
        highest = nil
        estimator.reset(keepTarget: !newStream)
        smoothedDepthMs = 0
        concealedRun = 0
        burst = 0
    }

    private func slot(_ seq: Int64) -> Int {
        Int(seq % Int64(Self.slotCount))
    }

    /// Stores one packet. `isPrimary` is true for the datagram's own packet, false for a
    /// redundant copy; only primaries count for jitter and lateness. `processingMs` is how long
    /// the datagram waited on the iPad before reaching here, if known.
    func insert(seq: Int64, timestampMs: UInt32, isPrimary: Bool, bytes: UnsafeRawBufferPointer,
                nowMs: Double, processingMs: Double? = nil) {
        guard seq >= 0 else { return }
        if isPrimary, let processingMs {
            maxProcessingMs = max(maxProcessingMs, processingMs)
        }
        if let highest, abs(seq - highest) > Self.resyncGap {
            guard isPrimary else { return }
            reset()
        }
        if let next, seq < next {
            if isPrimary { late += 1 }
            return
        }
        if isPrimary {
            estimator.onArrival(offsetMs: nowMs - Double(timestampMs), nowMs: nowMs)
        }
        let index = slot(seq)
        if seqs[index] == seq {
            if isPrimary { arrivedAsPrimary[index] = true }
            return
        }
        guard bytes.count > 0, bytes.count <= Self.maxPacketBytes, let base = bytes.baseAddress else { return }
        (storage + index * Self.maxPacketBytes).update(from: base.assumingMemoryBound(to: UInt8.self),
                                                        count: bytes.count)
        seqs[index] = seq
        lengths[index] = bytes.count
        arrivedAsPrimary[index] = isPrimary
        if highest.map({ seq > $0 }) ?? true {
            highest = seq
        }
    }

    /// Packets from `next` through the newest received, holes included.
    private var depthPackets: Int64 {
        guard let next, let highest else { return 0 }
        return max(0, highest - next + 1)
    }

    /// Called whenever the output needs another packet's worth of audio. `queuedMs` is decoded
    /// audio not yet played. Copies packet bytes into `a` / `b`.
    func pull(queuedMs: Double, a: UnsafeMutablePointer<UInt8>, b: UnsafeMutablePointer<UInt8>) -> Action {
        let target = targetMs
        if next == nil && !start(targetMs: target) {
            return .silence
        }
        guard let current = next else { return .silence }
        let depth = depthPackets
        let depthMs = Double(depth) * Self.packetMs + queuedMs
        smoothedDepthMs += 0.02 * (depthMs - smoothedDepthMs)

        if depth == 0 {
            concealedRun += 1
            if concealedRun > Self.maxConcealedRun {
                next = nil
                return .silence
            }
            countMissing()
            next = current + 1
            return .conceal
        }
        concealedRun = 0
        packetsSinceAdjust += 1

        let band = max(Self.adjustBandMs, Self.adjustBandFraction * target)
        if packetsSinceAdjust >= Self.adjustCooldownPackets {
            if smoothedDepthMs > target + band && depth >= 2 {
                packetsSinceAdjust = 0
                let first = take(current, into: a)
                let second = take(current + 1, into: b)
                next = current + 2
                smoothedDepthMs -= Self.packetMs
                return .merge(first, second)
            }
            if smoothedDepthMs < target - band {
                packetsSinceAdjust = 0
                smoothedDepthMs += Self.packetMs
                return .conceal
            }
        }

        next = current + 1
        if let length = take(current, into: a) {
            return .decode(length)
        }
        return .conceal
    }

    /// Starts playback once the buffered span reaches the target, skipping anything older than
    /// the target behind the newest packet.
    private func start(targetMs: Double) -> Bool {
        guard let highest else { return false }
        var lowest = highest
        for i in 0..<Self.slotCount {
            let seq = seqs[i]
            if seq >= 0 && seq < lowest && highest - seq < Int64(Self.slotCount) {
                lowest = seq
            }
        }
        let targetPackets = max(1, Int64((targetMs / Self.packetMs).rounded(.up)))
        guard highest - lowest + 1 >= targetPackets else { return false }
        let first = max(lowest, highest - targetPackets + 1)
        for i in 0..<Self.slotCount where seqs[i] >= 0 && seqs[i] < first {
            seqs[i] = -1
        }
        next = first
        smoothedDepthMs = Double(highest - first + 1) * Self.packetMs
        concealedRun = 0
        packetsSinceAdjust = 0
        return true
    }

    private func take(_ seq: Int64, into dest: UnsafeMutablePointer<UInt8>) -> Int? {
        let index = slot(seq)
        guard seqs[index] == seq else {
            countMissing()
            return nil
        }
        due += 1
        let length = lengths[index]
        dest.update(from: storage + index * Self.maxPacketBytes, count: length)
        seqs[index] = -1
        if arrivedAsPrimary[index] {
            burst = 0
        } else {
            primaryLost += 1
            recovered += 1
            burst += 1
            maxBurst = max(maxBurst, burst)
        }
        return length
    }

    private func countMissing() {
        due += 1
        primaryLost += 1
        concealed += 1
        burst += 1
        maxBurst = max(maxBurst, burst)
    }

    /// The report since the last call, or nil while not playing. Resets the counters.
    func takeReport(generation: UInt32) -> AudioPlayoutReport? {
        defer {
            due = 0
            primaryLost = 0
            recovered = 0
            concealed = 0
            late = 0
            maxBurst = 0
            maxProcessingMs = 0
        }
        guard isPlaying else { return nil }
        func micros(_ ms: Double) -> UInt32 {
            UInt32(min(max(ms, 0), 4_000_000) * 1000)
        }
        return AudioPlayoutReport(generation: generation,
                                  depthUs: micros(smoothedDepthMs),
                                  targetUs: micros(targetMs),
                                  due: UInt16(clamping: due),
                                  primaryLost: UInt16(clamping: primaryLost),
                                  recovered: UInt16(clamping: recovered),
                                  concealed: UInt16(clamping: concealed),
                                  late: UInt16(clamping: late),
                                  maxBurst: UInt8(clamping: maxBurst),
                                  jitterUs: micros(estimator.targetMs),
                                  capUs: maxTargetMs.isFinite ? micros(maxTargetMs) : 0,
                                  maxProcessingUs: micros(maxProcessingMs))
    }
}

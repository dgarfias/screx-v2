import Foundation
import QuartzCore

final class StreamClient {
    private let session: QUICSession
    private let lifecycleLock = NSLock()
    private var isRunning = false
    private var running: Bool {
        get {
            lifecycleLock.lock()
            defer { lifecycleLock.unlock() }
            return isRunning
        }
        set {
            lifecycleLock.lock()
            isRunning = newValue
            lifecycleLock.unlock()
        }
    }
    /// Dispatches authenticated QUIC datagrams and handles audio directly. Video
    /// reassembly, FEC recovery and decoding happen on `videoQueue`, so audio never waits behind
    /// them.
    private let queue = DispatchQueue(label: "screx.stream", qos: .userInteractive)
    private let pendingDatagrams = DispatchSemaphore(value: 512)
    /// Owns `videoStreams` and everything about video frames.
    private let videoQueue = DispatchQueue(label: "screx.stream.video", qos: .userInteractive)
    private let pendingVideo = DispatchSemaphore(value: 512)
    private static let maxPendingFrames = 8
    private let debugId = String(UUID().uuidString.prefix(6))
    private let videoStreams: [StreamDisplayID: VideoReceiveState]
    let audioPlayer: AudioPlayer
    let avSync: AVSyncState

    var onFirstFrame: (() -> Void)?
    var onTraffic: ((Int, Int) -> Void)?
    var sendPliRequest: ((StreamDisplayID, UInt32) -> Void)?
    var onVideoFrameReady: ((StreamDisplayID, UInt32) -> Void)?

    private static let headerLen = MediaPacketHeader.byteCount
    static let chunkPayload = QUICSession.chunkPayload
    /// How long an incomplete frame may sit before we give up on it. Only a
    /// loss-recovery deadline (complete frames deliver immediately), so it is
    /// not a steady-state latency cost. 100 ms is six frames at 60 fps;
    /// 50 ms (~3 frames) recovers sooner on loss while staying far above
    /// Wi-Fi reorder jitter. Moonlight uses a ~10 ms reorder window.
    private static let frameTimeout: TimeInterval = 0.050

    private var audioGeneration: UInt32 = 0
    private static let audioReportInterval: TimeInterval = 0.2
    private var audioReportTimer: DispatchSourceTimer?
    private var hasReceivedFirstFrame = false
    private var videoRecoveryTimer: DispatchSourceTimer?
    private static let pliMinInterval: TimeInterval = 1.0
    private var receiveCount = 0

    private func log(_ message: String) {
        print("[stream \(debugId)] \(message)")
    }

    private func shouldLogDebug(_ count: Int) -> Bool {
        count <= 3 || (count > 0 && (count & (count - 1)) == 0)
    }

    init(session: QUICSession, decoder: VideoDecoder, externalDecoder: VideoDecoder, audioPlayer: AudioPlayer, avSync: AVSyncState) {
        self.session = session
        self.videoStreams = [
            .internalDisplay: VideoReceiveState(decoder: decoder),
            .externalDisplay: VideoReceiveState(decoder: externalDecoder)
        ]
        self.audioPlayer = audioPlayer
        self.avSync = avSync
    }

    func connect() {
        log("connect()")
        running = true
        session.setDatagramHandler { [weak self] packet in
            guard let self else { return }
            let pending = self.pendingDatagrams
            guard pending.wait(timeout: .now()) == .success else { return }
            self.queue.async { [weak self] in
                defer { pending.signal() }
                guard let self, self.running else { return }
                self.onTraffic?(packet.count, 0)
                self.receiveCount += 1
                self.handlePacket(packet)
            }
        }
        startVideoRecovery()
        startAudioReports()
        for (display, stream) in videoStreams {
            stream.decoder.onFrameReady = { [weak self] generation in
                self?.videoQueue.async { [weak self] in
                    guard let self, self.videoStreams[display]?.timeline.generation == generation else { return }
                    self.onVideoFrameReady?(display, generation)
                }
            }
            stream.decoder.sendPliRequest = { [weak self] in
                self?.videoQueue.async { [weak self] in self?.sendPli(display: display) }
            }
        }
    }

    func disconnect() {
        running = false
        log("disconnect()")
        videoRecoveryTimer?.cancel()
        videoRecoveryTimer = nil
        audioReportTimer?.cancel()
        audioReportTimer = nil
        session.setDatagramHandler(nil)
    }

    private func sendMedia(_ packet: Data) {
        session.sendDatagram(packet)
        onTraffic?(0, packet.count)
    }

    /// Sends the speaker playout report the daemon uses to follow the iPad's clock and pick
    /// redundancy. Nothing is sent while speaker audio isn't playing.
    private func startAudioReports() {
        audioReportTimer?.cancel()
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + Self.audioReportInterval, repeating: Self.audioReportInterval)
        timer.setEventHandler { [weak self] in
            guard let self, let report = self.audioPlayer.takeReport() else { return }
            var packet = Data([ScrexMessage.Datagram.audioReport])
            packet.append(report.encoded())
            self.sendMedia(packet)
        }
        timer.resume()
        audioReportTimer = timer
    }

    private func startVideoRecovery() {
        videoRecoveryTimer?.cancel()
        let timer = DispatchSource.makeTimerSource(queue: videoQueue)
        timer.schedule(deadline: .now() + Self.frameTimeout, repeating: Self.frameTimeout)
        timer.setEventHandler { [weak self] in
            guard let self else { return }
            for display in StreamDisplayID.allCases {
                self.pruneOldFrames(display: display)
            }
        }
        timer.resume()
        videoRecoveryTimer = timer
    }

    private func handlePacket(_ data: Data) {
        guard let packet = MediaPacketHeader(data) else { return }
        let frameId = packet.frameID
        let chunkIdx = packet.chunkIndex
        let totalData = packet.totalData
        let totalParity = packet.totalParity
        let flags = packet.flags
        let codecId = packet.codecID
        let payloadLen = packet.payloadLength
        let timestampMs = packet.timestampMs

        if shouldLogDebug(receiveCount) {
            log("handlePacket frameId=\(frameId) chunkIdx=\(chunkIdx) totalData=\(totalData) totalParity=\(totalParity) flags=0x\(String(flags, radix: 16)) codecId=\(codecId) payloadLen=\(payloadLen)")
        }

        // QUIC has already authenticated and decrypted the complete datagram.
        let payload = Data(data.dropFirst(Self.headerLen))

        // Only the audio, IDR and external-display flags exist.
        guard packet.generation > 0, flags & ~0x13 == 0,
              Int(payloadLen) == payload.count else { return }

        if packet.isAudio {
            // One datagram per packet: its own Opus packet plus redundant copies of earlier ones.
            guard flags == 0x02, totalParity == 0, totalData == 1, chunkIdx == 0 else { return }
            guard packet.generation >= audioGeneration else { return }
            audioGeneration = packet.generation
            guard let entries = SpeakerAudioPayload.parse(payload) else {
                log("ignoring malformed speaker audio payload for frameId=\(frameId)")
                return
            }
            audioPlayer.enqueue(payload: payload, entries: entries, frameId: frameId,
                                timestampMs: timestampMs, generation: packet.generation)
            return
        }

        let pendingVideo = self.pendingVideo
        guard pendingVideo.wait(timeout: .now()) == .success else { return }
        videoQueue.async { [weak self] in
            defer { pendingVideo.signal() }
            guard let self, self.running else { return }
            self.handleVideoPacket(packet, payload: payload)
        }
    }

    /// Runs on `videoQueue`.
    private func handleVideoPacket(_ packet: MediaPacketHeader, payload: Data) {
        let frameId = packet.frameID
        let chunkIdx = packet.chunkIndex
        let totalData = packet.totalData
        let totalParity = packet.totalParity
        let codecId = packet.codecID
        let payloadLen = packet.payloadLength
        let timestampMs = packet.timestampMs
        guard codecId <= 1, let stream = videoStreams[packet.displayID] else { return }
        let shardCount = Int(totalData) + Int(totalParity)
        guard totalData > 0,
              shardCount <= 4096,
              Int(chunkIdx) < shardCount,
              payloadLen > 0,
              Int(payloadLen) <= Self.chunkPayload,
              Int(payloadLen) <= payload.count else {
            log("ignoring invalid video shard metadata for frameId=\(frameId)")
            return
        }

        let previousGeneration = stream.timeline.generation
        guard stream.timeline.accept(generation: packet.generation) else { return }
        let wantCodec: VideoCodecType = codecId == 0x01 ? .h265 : .h264
        if previousGeneration != packet.generation {
            stream.reassembly.removeAll(keepingCapacity: true)
            stream.lastPliTime = nil
            stream.waitingForIDR = true
            stream.decoder.beginStream(codec: wantCodec, generation: packet.generation)
        } else if stream.decoder.codec != wantCodec {
            // Codec changes require a new generation, not a decoder reset on a late shard.
            return
        }
        guard stream.timeline.isNewFrame(frameId) else { return }
        if stream.waitingForIDR && !packet.isIDR {
            sendPli(display: packet.displayID)
            return
        }

        let assembly: FrameAssembly
        if let existing = stream.reassembly[frameId] {
            guard existing.totalData == Int(totalData),
                  existing.totalParity == Int(totalParity),
                  existing.isIdr == packet.isIDR,
                  existing.timestampMs == timestampMs else {
                log("ignoring inconsistent shard metadata for frameId=\(frameId)")
                return
            }
            assembly = existing
        } else {
            if stream.reassembly.count >= Self.maxPendingFrames {
                guard let oldest = stream.reassembly.keys.min(), frameId > oldest else { return }
                stream.reassembly.removeValue(forKey: oldest)
                sendPli(display: packet.displayID)
            }
            guard let created = FrameAssembly(
                totalData: Int(totalData),
                totalParity: Int(totalParity),
                isIdr: packet.isIDR,
                createdAt: CACurrentMediaTime(),
                timestampMs: timestampMs
            ) else {
                log("ignoring invalid FEC layout for frameId=\(frameId)")
                return
            }
            assembly = created
        }
        stream.reassembly[frameId] = assembly

        assembly.addChunk(index: Int(chunkIdx), payload: payload, actualLen: Int(payloadLen))

        if assembly.isComplete {
            deliverFrame(display: packet.displayID, frameId: frameId, assembly: assembly)
        }

    }

    private func deliverFrame(display: StreamDisplayID, frameId: UInt32, assembly: FrameAssembly) {
        guard let stream = videoStreams[display] else { return }
        stream.reassembly.removeValue(forKey: frameId)
        guard stream.timeline.isNewFrame(frameId) else { return }
        guard let accessUnit = assembly.reassemble() else {
            sendPli(display: display)
            return
        }
        if stream.timeline.complete(frameId) {
            sendPli(display: display)
        }
        if assembly.isIdr { stream.waitingForIDR = false }
        stream.lastFrameTime = CACurrentMediaTime()
        // Audio remains a single session stream; prefer the internal video clock when active.
        if display == .internalDisplay || stream.lastFrameTime - (videoStreams[.internalDisplay]?.lastFrameTime ?? 0) > 0.25 {
            avSync.updateVideo(timestamp: assembly.timestampMs)
        }
        stream.decoder.decodeAccessUnit(accessUnit)
        if !hasReceivedFirstFrame {
            hasReceivedFirstFrame = true
            onFirstFrame?()
        }
    }

    private func pruneOldFrames(display: StreamDisplayID) {
        guard let stream = videoStreams[display] else { return }
        let now = CACurrentMediaTime()
        var recoverable: [(UInt32, FrameAssembly)] = []
        var expired: [UInt32] = []
        var superseded: [UInt32] = []
        for (fid, assembly) in stream.reassembly {
            if !stream.timeline.isNewFrame(fid) {
                superseded.append(fid)
                continue
            }
            if now - assembly.createdAt > Self.frameTimeout {
                if assembly.canRecover {
                    recoverable.append((fid, assembly))
                } else {
                    expired.append(fid)
                }
            }
        }
        for fid in superseded { stream.reassembly.removeValue(forKey: fid) }
        // Never mutate `reassembly` while enumerating it. Apart from avoiding
        // a rare collection-mutation crash, ordering recovered frames keeps a
        // late older frame from replacing a newer one at the decoder.
        recoverable.sort { $0.0 < $1.0 }
        for (fid, assembly) in recoverable {
            deliverFrame(display: display, frameId: fid, assembly: assembly)
        }
        if !expired.isEmpty {
            for fid in expired {
                stream.reassembly.removeValue(forKey: fid)
            }
            sendPli(display: display)
        }
    }

    private func sendPli(display: StreamDisplayID) {
        guard let stream = videoStreams[display], let generation = stream.timeline.generation else { return }
        let now = CACurrentMediaTime()
        if let last = stream.lastPliTime, now - last < Self.pliMinInterval { return }
        stream.lastPliTime = now
        sendPliRequest?(display, generation)
    }

    func sendCameraFrame(_ jpeg: Data, frameId: UInt32) {
        guard running else { return }
        let chunkSize = 1000
        let totalChunks = (jpeg.count + chunkSize - 1) / chunkSize

        for i in 0..<totalChunks {
            let start = i * chunkSize
            let end = min(start + chunkSize, jpeg.count)

            var packet = Data([ScrexMessage.Datagram.camera])
            withUnsafeBytes(of: frameId.bigEndian) { packet.append(contentsOf: $0) }
            withUnsafeBytes(of: UInt16(i).bigEndian) { packet.append(contentsOf: $0) }
            withUnsafeBytes(of: UInt16(totalChunks).bigEndian) { packet.append(contentsOf: $0) }
            packet.append(jpeg.subdata(in: start..<end))

            sendMedia(packet)
        }
    }

    /// Sends one mic Opus packet. `seq` counts mic packets only, so the daemon can
    /// detect lost ones and conceal them.
    func sendMicPacket(_ opus: Data, seq: UInt32) {
        guard running else { return }
        var packet = Data([ScrexMessage.Datagram.mic])
        withUnsafeBytes(of: seq.bigEndian) { packet.append(contentsOf: $0) }
        packet.append(opus)
        sendMedia(packet)
    }
}

/// Accessed only on `StreamClient.videoQueue`. No FEC, loss, or decoder state crosses displays.
private final class VideoReceiveState {
    let decoder: VideoDecoder
    var timeline = VideoStreamTimeline()
    var reassembly: [UInt32: FrameAssembly] = [:]
    var lastPliTime: TimeInterval?
    var waitingForIDR = true
    var lastFrameTime: TimeInterval = 0

    init(decoder: VideoDecoder) { self.decoder = decoder }
}

/// Reed-Solomon block layout shared with the daemon (`fec_block_count` /
/// `fec_block_range` in daemon/src/stream_server.rs) — the two must stay identical.
/// A frame that fits in 255 data+parity shards is a single block. Larger frames
/// are split into contiguous blocks: data shards `0..<totalData` and parity shards
/// `totalData..<totalShards` are each divided as evenly as possible, earlier blocks
/// taking the extras, and block `b` protects its own data range with its own parity.
enum FECBlockLayout {
    static let maxShardsPerBlock = 255

    static func blockCount(data: Int, parity: Int) -> Int {
        if parity == 0 || data + parity <= maxShardsPerBlock { return 1 }
        return (data + parity + maxShardsPerBlock - 2) / (maxShardsPerBlock - 1)
    }

    static func range(total: Int, blocks: Int, index: Int) -> Range<Int> {
        let base = total / blocks
        let extra = total % blocks
        let start = index * base + min(index, extra)
        return start..<(start + base + (index < extra ? 1 : 0))
    }
}

private final class FrameAssembly {
    let totalData: Int
    let totalParity: Int
    let totalShards: Int
    let isIdr: Bool
    let createdAt: TimeInterval
    let timestampMs: UInt32

    private let blockCount: Int
    private let dataRanges: [Range<Int>]
    private let parityRanges: [Range<Int>]
    /// FEC block of each shard index.
    private let blockOfShard: [Int]

    private var shards: [Data?]
    private var actualLengths: [Int?]
    private var dataReceived: [Int]
    private var shardsReceived: [Int]
    private var readyBlocks = 0

    /// Nil when the shard counts can't form a valid block layout.
    init?(totalData: Int, totalParity: Int, isIdr: Bool, createdAt: TimeInterval, timestampMs: UInt32 = 0) {
        let blocks = FECBlockLayout.blockCount(data: totalData, parity: totalParity)
        guard totalData >= blocks else { return nil }

        self.totalData = totalData
        self.totalParity = totalParity
        self.totalShards = totalData + totalParity
        self.isIdr = isIdr
        self.createdAt = createdAt
        self.timestampMs = timestampMs
        let dataRanges = (0..<blocks).map { FECBlockLayout.range(total: totalData, blocks: blocks, index: $0) }
        let parityRanges = (0..<blocks).map { FECBlockLayout.range(total: totalParity, blocks: blocks, index: $0) }
        var blockOfShard = [Int](repeating: 0, count: totalData + totalParity)
        for block in 0..<blocks {
            for i in dataRanges[block] { blockOfShard[i] = block }
            for i in parityRanges[block] { blockOfShard[totalData + i] = block }
        }
        self.blockCount = blocks
        self.dataRanges = dataRanges
        self.parityRanges = parityRanges
        self.blockOfShard = blockOfShard
        self.shards = Array(repeating: nil, count: totalData + totalParity)
        self.actualLengths = Array(repeating: nil, count: totalData + totalParity)
        self.dataReceived = Array(repeating: 0, count: blocks)
        self.shardsReceived = Array(repeating: 0, count: blocks)
    }

    /// A block is decodable once it has all its data shards, or (with parity) at least
    /// as many shards as it has data shards.
    private func blockIsReady(_ block: Int) -> Bool {
        let needed = dataRanges[block].count
        return dataReceived[block] == needed || shardsReceived[block] >= needed
    }

    func addChunk(index: Int, payload: Data, actualLen: Int) {
        guard shards[index] == nil else { return }
        shards[index] = payload
        actualLengths[index] = actualLen

        let block = blockOfShard[index]
        let wasReady = blockIsReady(block)
        if index < totalData { dataReceived[block] += 1 }
        shardsReceived[block] += 1
        if !wasReady && blockIsReady(block) { readyBlocks += 1 }
    }

    var isComplete: Bool { readyBlocks == blockCount }

    var canRecover: Bool { isComplete }

    func reassemble() -> Data? {
        guard isComplete else { return nil }

        for block in 0..<blockCount where dataReceived[block] < dataRanges[block].count {
            guard recoverBlock(block) else { return nil }
        }

        var totalSize = 0
        for i in 0..<totalData {
            totalSize += actualLengths[i] ?? StreamClient.chunkPayload
        }
        var result = Data(capacity: totalSize)
        for i in 0..<totalData {
            guard let shard = shards[i] else { return nil }
            let actualLen = min(actualLengths[i] ?? shard.count, shard.count)
            if actualLen == shard.count {
                result.append(shard)
            } else {
                result.append(shard.prefix(actualLen))
            }
        }
        return result
    }

    /// Runs Reed-Solomon recovery for one block and stores the recovered data shards.
    private func recoverBlock(_ block: Int) -> Bool {
        let shardSize = StreamClient.chunkPayload
        let dataRange = dataRanges[block]
        let parityRange = parityRanges[block]
        let indices = Array(dataRange) + parityRange.map { totalData + $0 }

        var blockShards: [Data?] = indices.map { index -> Data? in
            guard let data = shards[index] else { return nil }
            if data.count == shardSize { return data }
            if data.count < shardSize {
                var padded = data
                padded.append(Data(count: shardSize - data.count))
                return padded
            }
            return data.prefix(shardSize)
        }

        guard ReedSolomonDecoder.recover(shards: &blockShards,
                                         dataCount: dataRange.count,
                                         parityCount: parityRange.count) else {
            return false
        }

        for (offset, index) in dataRange.enumerated() where shards[index] == nil {
            guard let recovered = blockShards[offset] else { return false }
            shards[index] = recovered
        }
        return true
    }
}

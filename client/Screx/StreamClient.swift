import Foundation
import Network
import CryptoKit
import QuartzCore

enum StreamClientEvent {
    case readyToRegister
    case waiting(String)
    case connectionFailed(String)
    case receiveError(String)
    case timedOut
    case firstFrame
}

final class StreamClient {
    private let endpoint: NWEndpoint
    private var connection: NWConnection?
    private let queue = DispatchQueue(label: "screx.stream", qos: .userInteractive)
    private let debugId = String(UUID().uuidString.prefix(6))
    private let videoStreams: [StreamDisplayID: VideoReceiveState]
    let audioPlayer: AudioPlayer
    let avSync: AVSyncState

    var onEvent: ((StreamClientEvent) -> Void)?
    var onDisconnect: (() -> Void)?
    var onTraffic: ((Int, Int) -> Void)?
    var sessionKey: SymmetricKey?
    var sendPliRequest: ((StreamDisplayID, UInt32) -> Void)?
    var onVideoFrameReady: ((StreamDisplayID, UInt32) -> Void)?

    private static let headerLen = MediaPacketHeader.byteCount
    static let chunkPayload = 1400
    private static let keepaliveInterval: TimeInterval = 2.0
    /// How long an incomplete frame may sit before we give up on it. Only a
    /// loss-recovery deadline (complete frames deliver immediately), so it is
    /// not a steady-state latency cost. 100 ms is six frames at 60 fps;
    /// 50 ms (~3 frames) recovers sooner on loss while staying far above
    /// Wi-Fi reorder jitter. Moonlight uses a ~10 ms reorder window.
    private static let frameTimeout: TimeInterval = 0.050
    private static let dataTimeout: TimeInterval = 5.0

    private var audioReassembly: [UInt32: AudioAssembly] = [:]
    private var audioGeneration: UInt32 = 0
    private var hasReceivedFirstFrame = false
    private var keepaliveTimer: DispatchSourceTimer?
    private var dataTimeoutTimer: DispatchSourceTimer?
    private var videoRecoveryTimer: DispatchSourceTimer?
    private static let pliMinInterval: TimeInterval = 1.0
    private var sendSeq: UInt32 = 0
    private var registerSendCount = 0
    private var receiveCount = 0
    private var decryptFailCount = 0
    private var lastInboundTime = CACurrentMediaTime()

    private func log(_ message: String) {
        print("[stream \(debugId)] \(message)")
    }

    private func shouldLogDebug(_ count: Int) -> Bool {
        count <= 3 || (count > 0 && (count & (count - 1)) == 0)
    }

    init(endpoint: NWEndpoint, decoder: VideoDecoder, externalDecoder: VideoDecoder, audioPlayer: AudioPlayer, avSync: AVSyncState) {
        self.endpoint = endpoint
        self.videoStreams = [
            .internalDisplay: VideoReceiveState(decoder: decoder),
            .externalDisplay: VideoReceiveState(decoder: externalDecoder)
        ]
        self.audioPlayer = audioPlayer
        self.avSync = avSync
    }

    func connect() {
        log("connect() endpoint=\(endpoint)")
        let params = NWParameters.udp
        params.requiredLocalEndpoint = nil

        let conn = NWConnection(to: endpoint, using: params)
        self.connection = conn

        conn.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            self.log("udp state -> \(state)")
            switch state {
            case .ready:
                self.log("udp ready, sending initial register and starting receive loop")
                self.onEvent?(.readyToRegister)
                self.sendRegister(conn)
                self.startKeepalive(conn)
                self.startDataTimeout()
                self.startVideoRecovery()
                for (display, stream) in self.videoStreams {
                    stream.decoder.onFrameReady = { [weak self] generation in
                        self?.queue.async { [weak self] in
                            guard let self, self.videoStreams[display]?.timeline.generation == generation else { return }
                            self.onVideoFrameReady?(display, generation)
                        }
                    }
                    stream.decoder.sendPliRequest = { [weak self] in
                        // Decoder callbacks may originate on the render queue.
                        self?.queue.async { [weak self] in self?.sendPli(display: display) }
                    }
                }
                self.receiveLoop(conn)
            case .failed(let error):
                self.disconnect()
                self.onEvent?(.connectionFailed(error.localizedDescription))
                self.onDisconnect?()
            case .waiting(let error):
                self.onEvent?(.waiting(error.localizedDescription))
            default:
                break
            }
        }

        conn.start(queue: queue)
    }

    func disconnect() {
        log("disconnect() registerSends=\(registerSendCount) receives=\(receiveCount) decryptFailures=\(decryptFailCount)")
        keepaliveTimer?.cancel()
        keepaliveTimer = nil
        dataTimeoutTimer?.cancel()
        dataTimeoutTimer = nil
        videoRecoveryTimer?.cancel()
        videoRecoveryTimer = nil
        connection?.cancel()
        connection = nil
    }

    private func sendRegister(_ conn: NWConnection) {
        registerSendCount += 1
        log("sendRegister() #\(registerSendCount)")
        encryptAndSend(Data([ScrexMessage.Udp.keepalive]), conn: conn, label: "register")
    }

    /// Encrypt plaintext with session key, prepend 4-byte seq_num, send over UDP.
    private func encryptAndSend(_ plaintext: Data, conn: NWConnection, label: String) {
        guard let key = sessionKey else {
            log("\(label) send aborted: missing session key")
            return
        }

        let seq = sendSeq
        sendSeq = sendSeq &+ 1

        let nonce = ScrexCrypto.nonceClient(seqNum: seq)
        var seqData = Data(count: 4)
        seqData.withUnsafeMutableBytes { buf in
            buf.storeBytes(of: seq.bigEndian, as: UInt32.self)
        }

        guard let encrypted = ScrexCrypto.encrypt(key: key, nonce: nonce, plaintext: plaintext, aad: seqData) else {
            print("[stream] \(label) encrypt failed")
            return
        }

        var packet = seqData
        packet.append(encrypted)
        log("\(label) send queued: seq=\(seq) plaintext=\(plaintext.count) packet=\(packet.count)")

        conn.send(content: packet, completion: .contentProcessed { error in
            if let error {
                print("[stream] \(label) send error: \(error)")
            } else {
                self.onTraffic?(0, packet.count)
                self.log("\(label) send completed")
            }
        })
    }

    private func startKeepalive(_ conn: NWConnection) {
        log("startKeepalive(interval=\(Self.keepaliveInterval)s)")
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + Self.keepaliveInterval, repeating: Self.keepaliveInterval)
        timer.setEventHandler { [weak self] in
            self?.sendRegister(conn)
        }
        timer.resume()
        keepaliveTimer = timer
    }

    private func startDataTimeout() {
        log("startDataTimeout(timeout=\(Self.dataTimeout)s)")
        dataTimeoutTimer?.cancel()
        lastInboundTime = CACurrentMediaTime()
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + 1, repeating: 1)
        timer.setEventHandler { [weak self] in
            guard let self else { return }
            if CACurrentMediaTime() - self.lastInboundTime >= Self.dataTimeout {
                self.handleTimeout()
            }
        }
        timer.resume()
        dataTimeoutTimer = timer
    }

    private func resetDataTimeout() {
        lastInboundTime = CACurrentMediaTime()
    }

    private func startVideoRecovery() {
        videoRecoveryTimer?.cancel()
        let timer = DispatchSource.makeTimerSource(queue: queue)
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

    private func handleTimeout() {
        guard connection != nil else { return }
        log("data timeout fired after \(Self.dataTimeout)s without inbound UDP")
        print("[stream] data timeout — daemon appears gone")
        disconnect()
        onEvent?(.timedOut)
        onDisconnect?()
    }

    private func receiveLoop(_ conn: NWConnection) {
        conn.receiveMessage { [weak self] data, _, isComplete, error in
            guard let self else { return }

            if let error {
                self.log("receiveLoop error: \(error.localizedDescription)")
                self.disconnect()
                self.onEvent?(.receiveError(error.localizedDescription))
                self.onDisconnect?()
                return
            }

            if let data, !data.isEmpty {
                self.onTraffic?(data.count, 0)
                self.receiveCount += 1
                if self.shouldLogDebug(self.receiveCount) {
                    self.log("received UDP datagram #\(self.receiveCount): bytes=\(data.count) isComplete=\(isComplete)")
                }
                if data.count >= Self.headerLen {
                    self.handlePacket(data)
                } else {
                    self.log("ignoring short inbound UDP datagram: \(data.count) bytes")
                }
            }

            self.receiveLoop(conn)
        }
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

        // Decrypt payload using Data slices (COW, no copy until mutation)
        let payload: Data
        let encryptedPayload = data[Self.headerLen...]

        if let key = sessionKey {
            let nonce = ScrexCrypto.nonceServer(frameId: frameId, chunkIdx: chunkIdx, flags: flags, generation: packet.generation)
            let header = data[..<Self.headerLen]
            guard let decrypted = ScrexCrypto.decrypt(key: key, nonce: nonce, ciphertextAndTag: encryptedPayload, aad: header) else {
                decryptFailCount += 1
                log("decrypt failed #\(decryptFailCount) for frameId=\(frameId) chunkIdx=\(chunkIdx) flags=0x\(String(flags, radix: 16)) encryptedLen=\(encryptedPayload.count)")
                return
            }
            payload = decrypted
        } else {
            return
        }

        // No unauthenticated header can reset a decoder, retire an epoch, or keep us alive.
        guard packet.generation > 0, flags & ~0x93 == 0,
              Int(payloadLen) == payload.count else { return }
        resetDataTimeout()
        if packet.isHeartbeat {
            guard flags == 0x80, totalData == 0, totalParity == 0, chunkIdx == 0 else { return }
            return
        }

        if packet.isAudio {
            guard flags == 0x02, totalParity == 0 else { return }
            guard totalData > 0,
                  chunkIdx < totalData,
                  Int(payloadLen) <= payload.count else {
                log("ignoring invalid audio shard metadata for frameId=\(frameId)")
                return
            }
            guard packet.generation >= audioGeneration else { return }
            if packet.generation != audioGeneration {
                audioGeneration = packet.generation
                audioReassembly.removeAll(keepingCapacity: true)
            }
            handleAudioPacket(frameId: frameId, chunkIdx: Int(chunkIdx),
                              totalData: Int(totalData), payload: payload,
                              actualLen: Int(payloadLen), timestampMs: timestampMs)
            return
        }

        // Video path
        guard codecId <= 1, let stream = videoStreams[packet.displayID] else { return }
        let shardCount = Int(totalData) + Int(totalParity)
        guard totalData > 0,
              shardCount <= 4096,
              Int(chunkIdx) < shardCount,
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

        pruneOldFrames(display: packet.displayID)
    }

    private func handleAudioPacket(frameId: UInt32, chunkIdx: Int, totalData: Int,
                                   payload: Data, actualLen: Int, timestampMs: UInt32) {
        let enqueue: (Data) -> Void = { [audioPlayer] data in
            audioPlayer.enqueueOpus(data, timestampMs: timestampMs)
        }

        if totalData == 1 {
            enqueue(payload.prefix(actualLen))
            return
        }

        let assembly = audioReassembly[frameId] ?? AudioAssembly(totalData: totalData)
        audioReassembly[frameId] = assembly
        assembly.addChunk(index: chunkIdx, payload: payload, actualLen: actualLen)

        if assembly.isComplete {
            if let audio = assembly.reassemble() {
                enqueue(audio)
            }
            audioReassembly.removeValue(forKey: frameId)
        }

        // Prune stale audio assemblies (keep last 10)
        if audioReassembly.count > 10 {
            let sorted = audioReassembly.keys.sorted()
            for key in sorted.prefix(audioReassembly.count - 10) {
                audioReassembly.removeValue(forKey: key)
            }
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
            onEvent?(.firstFrame)
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
        guard let conn = connection else { return }
        let chunkSize = 1300
        let totalChunks = (jpeg.count + chunkSize - 1) / chunkSize

        for i in 0..<totalChunks {
            let start = i * chunkSize
            let end = min(start + chunkSize, jpeg.count)

            var packet = Data([ScrexMessage.Udp.camera])
            withUnsafeBytes(of: frameId.bigEndian) { packet.append(contentsOf: $0) }
            withUnsafeBytes(of: UInt16(i).bigEndian) { packet.append(contentsOf: $0) }
            withUnsafeBytes(of: UInt16(totalChunks).bigEndian) { packet.append(contentsOf: $0) }
            packet.append(jpeg.subdata(in: start..<end))

            encryptAndSend(packet, conn: conn, label: "cam")
        }
    }

    /// Sends one mic Opus packet. `seq` counts mic packets only, so the daemon can
    /// detect lost ones and conceal them.
    func sendMicPacket(_ opus: Data, seq: UInt32) {
        guard let conn = connection else { return }
        var packet = Data([ScrexMessage.Udp.mic])
        withUnsafeBytes(of: seq.bigEndian) { packet.append(contentsOf: $0) }
        packet.append(opus)
        encryptAndSend(packet, conn: conn, label: "mic")
    }
}

/// Accessed only on the stream receive queue. No FEC, loss, or decoder state crosses displays.
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

private final class AudioAssembly {
    let totalData: Int
    private var chunks: [Int: Data] = [:]
    private var actualLengths: [Int: Int] = [:]
    private var receivedCount = 0

    init(totalData: Int) {
        self.totalData = totalData
    }

    func addChunk(index: Int, payload: Data, actualLen: Int) {
        guard chunks[index] == nil else { return }
        chunks[index] = payload
        actualLengths[index] = actualLen
        receivedCount += 1
    }

    var isComplete: Bool { receivedCount >= totalData }

    func reassemble() -> Data? {
        var totalSize = 0
        for i in 0..<totalData {
            guard let chunk = chunks[i] else { return nil }
            totalSize += actualLengths[i] ?? chunk.count
        }
        var result = Data(capacity: totalSize)
        for i in 0..<totalData {
            let chunk = chunks[i]!
            let len = actualLengths[i] ?? chunk.count
            if len == chunk.count {
                result.append(chunk)
            } else {
                result.append(chunk[..<len])
            }
        }
        return result
    }
}

import Foundation
import Network
import CryptoKit
import QuartzCore

final class StreamClient {
    private let endpoint: NWEndpoint
    private var connection: NWConnection?
    private let queue = DispatchQueue(label: "screx.stream", qos: .userInteractive)
    private let debugId = String(UUID().uuidString.prefix(6))
    let decoder: VideoDecoder
    let audioPlayer: AudioPlayer
    let avSync: AVSyncState

    var onStatus: ((String) -> Void)?
    var onDisconnect: (() -> Void)?
    var sessionKey: SymmetricKey?

    /// When true, suppresses the data timeout (e.g. USB is active and no network data is expected)
    var suppressTimeout = false

    private static let headerLen = 18
    static let chunkPayload = 1400
    private static let registerMagic = Data("SCREX".utf8)
    private static let keepaliveInterval: TimeInterval = 2.0
    private static let frameTimeout: TimeInterval = 0.050
    private static let dataTimeout: TimeInterval = 5.0

    private static let flagAudio: UInt8 = 0x02

    private var reassembly: [UInt32: FrameAssembly] = [:]
    private var audioReassembly: [UInt32: AudioAssembly] = [:]
    private var lastCompletedFrameId: UInt32 = 0
    private var hasReceivedFirstFrame = false
    private var keepaliveTimer: DispatchSourceTimer?
    private var dataTimeoutTimer: DispatchSourceTimer?
    private var lastPliTime: TimeInterval = 0
    private static let pliMinInterval: TimeInterval = 1.0
    private static let pliMagic = Data("PLI".utf8)
    private var sendSeq: UInt32 = 0
    private var registerSendCount = 0
    private var receiveCount = 0
    private var decryptFailCount = 0

    private func log(_ message: String) {
        print("[stream \(debugId)] \(message)")
    }

    init(endpoint: NWEndpoint, decoder: VideoDecoder, audioPlayer: AudioPlayer, avSync: AVSyncState) {
        self.endpoint = endpoint
        self.decoder = decoder
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
                self.onStatus?("Connected, registering...")
                self.sendRegister(conn)
                self.startKeepalive(conn)
                self.startDataTimeout()
                self.receiveLoop(conn)
            case .failed(let error):
                self.onStatus?("Connection failed: \(error.localizedDescription)")
                self.handleTimeout()
            case .waiting(let error):
                self.onStatus?("Waiting: \(error.localizedDescription)")
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
        connection?.cancel()
        connection = nil
    }

    private func sendRegister(_ conn: NWConnection) {
        registerSendCount += 1
        log("sendRegister() #\(registerSendCount)")
        encryptAndSend(Self.registerMagic, conn: conn, label: "register")
    }

    /// Encrypt plaintext with session key, prepend 4-byte seq_num, send over UDP.
    private func encryptAndSend(_ plaintext: Data, conn: NWConnection, label: String) {
        guard let key = sessionKey else {
            // Fallback: send unencrypted (shouldn't happen in normal flow)
            log("\(label) send without session key, bytes=\(plaintext.count)")
            conn.send(content: plaintext, completion: .contentProcessed { error in
                if let error { print("[stream] \(label) send error: \(error)") }
            })
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
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + Self.dataTimeout)
        timer.setEventHandler { [weak self] in
            self?.handleTimeout()
        }
        timer.resume()
        dataTimeoutTimer = timer
    }

    private func resetDataTimeout() {
        dataTimeoutTimer?.cancel()
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + Self.dataTimeout)
        timer.setEventHandler { [weak self] in
            self?.handleTimeout()
        }
        timer.resume()
        dataTimeoutTimer = timer
    }

    private func handleTimeout() {
        guard connection != nil else { return }
        if suppressTimeout {
            resetDataTimeout()
            return
        }
        log("data timeout fired after \(Self.dataTimeout)s without inbound UDP")
        print("[stream] data timeout — daemon appears gone")
        disconnect()
        onStatus?("Daemon disconnected")
        onDisconnect?()
    }

    private func receiveLoop(_ conn: NWConnection) {
        log("receiveLoop waiting for UDP datagram")
        conn.receiveMessage { [weak self] data, _, isComplete, error in
            guard let self else { return }

            if let error {
                self.log("receiveLoop error: \(error.localizedDescription)")
                self.onStatus?("Receive error: \(error.localizedDescription)")
                self.handleTimeout()
                return
            }

            if let data, !data.isEmpty {
                self.receiveCount += 1
                self.log("received UDP datagram #\(self.receiveCount): bytes=\(data.count) isComplete=\(isComplete)")
                self.resetDataTimeout()
                if data.count >= Self.headerLen {
                    self.handlePacket(data)
                } else {
                    self.log("ignoring short inbound UDP datagram: \(data.count) bytes")
                }
            } else {
                self.log("receiveLoop callback with empty data isComplete=\(isComplete)")
            }

            self.receiveLoop(conn)
        }
    }

    private func handlePacket(_ data: Data) {
        guard data.count >= Self.headerLen else { return }

        let frameId: UInt32 = UInt32(data[0]) << 24 | UInt32(data[1]) << 16
                           | UInt32(data[2]) << 8  | UInt32(data[3])
        let chunkIdx: UInt16 = UInt16(data[4]) << 8 | UInt16(data[5])
        let totalData: UInt16 = UInt16(data[6]) << 8 | UInt16(data[7])
        let totalParity: UInt16 = UInt16(data[8]) << 8 | UInt16(data[9])
        let flags = data[10]
        let codecId = data[11]
        let payloadLen: UInt16 = UInt16(data[12]) << 8 | UInt16(data[13])
        let timestampMs: UInt32 = UInt32(data[14]) << 24 | UInt32(data[15]) << 16
                               | UInt32(data[16]) << 8  | UInt32(data[17])
        let isAudio = (flags & Self.flagAudio) != 0
        let isIdr = (flags & 1) != 0

        if receiveCount <= 12 || receiveCount.isMultiple(of: 25) {
            log("handlePacket frameId=\(frameId) chunkIdx=\(chunkIdx) totalData=\(totalData) totalParity=\(totalParity) flags=0x\(String(flags, radix: 16)) codecId=\(codecId) payloadLen=\(payloadLen)")
        }

        if frameId == 0xFFFF_FFFF && totalData == 0 {
            log("received heartbeat packet")
            return
        }

        if !isAudio {
            let wantCodec: VideoCodecType = codecId == 0x01 ? .h265 : .h264
            if wantCodec != decoder.codec {
                decoder.setCodec(wantCodec)
            }
        }

        // Decrypt payload using Data slices (COW, no copy until mutation)
        let payload: Data
        let encryptedPayload = data[Self.headerLen...]

        if let key = sessionKey {
            let nonce = ScrexCrypto.nonceServer(frameId: frameId, chunkIdx: chunkIdx, flags: flags)
            let header = data[..<Self.headerLen]
            guard let decrypted = ScrexCrypto.decrypt(key: key, nonce: nonce, ciphertextAndTag: encryptedPayload, aad: header) else {
                decryptFailCount += 1
                log("decrypt failed #\(decryptFailCount) for frameId=\(frameId) chunkIdx=\(chunkIdx) flags=0x\(String(flags, radix: 16)) encryptedLen=\(encryptedPayload.count)")
                return
            }
            payload = decrypted
        } else {
            log("no session key set while decoding inbound packet")
            payload = Data(encryptedPayload)
        }

        if isAudio {
            handleAudioPacket(frameId: frameId, chunkIdx: Int(chunkIdx),
                              totalData: Int(totalData), payload: payload,
                              actualLen: Int(payloadLen), timestampMs: timestampMs)
            return
        }

        // Video path
        if hasReceivedFirstFrame && frameId < lastCompletedFrameId && (lastCompletedFrameId - frameId) < 0x80000000 {
            return
        }

        let assembly = reassembly[frameId] ?? FrameAssembly(
            totalData: Int(totalData),
            totalParity: Int(totalParity),
            isIdr: isIdr,
            createdAt: CACurrentMediaTime(),
            timestampMs: timestampMs
        )
        reassembly[frameId] = assembly

        assembly.addChunk(index: Int(chunkIdx), payload: payload, actualLen: Int(payloadLen))

        if assembly.isComplete {
            deliverFrame(frameId: frameId, assembly: assembly, timestampMs: assembly.timestampMs)
        }

        pruneOldFrames()
    }

    private func handleAudioPacket(frameId: UInt32, chunkIdx: Int, totalData: Int,
                                   payload: Data, actualLen: Int, timestampMs: UInt32) {
        if totalData == 1 {
            audioPlayer.enqueueAudio(payload.prefix(actualLen), timestampMs: timestampMs)
            return
        }

        let assembly = audioReassembly[frameId] ?? AudioAssembly(totalData: totalData)
        audioReassembly[frameId] = assembly
        assembly.addChunk(index: chunkIdx, payload: payload, actualLen: actualLen)

        if assembly.isComplete {
            if let pcm = assembly.reassemble() {
                audioPlayer.enqueueAudio(pcm, timestampMs: timestampMs)
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

    private func deliverFrame(frameId: UInt32, assembly: FrameAssembly, timestampMs: UInt32 = 0) {
        guard let accessUnit = assembly.reassemble() else {
            reassembly.removeValue(forKey: frameId)
            return
        }

        reassembly.removeValue(forKey: frameId)

        // Detect frame_id gap → frames were lost in transit
        if hasReceivedFirstFrame && frameId > lastCompletedFrameId + 1 {
            let gap = frameId - lastCompletedFrameId - 1
            if gap > 0 && gap < 0x80000000 {
                sendPli()
            }
        }

        lastCompletedFrameId = frameId
        hasReceivedFirstFrame = true

        avSync.updateVideo(timestamp: timestampMs)
        decoder.decodeAccessUnit(accessUnit)

        if !decoder.hasReportedFirstFrame {
            decoder.hasReportedFirstFrame = true
            log("first video frame delivered to decoder")
            onStatus?("Streaming")
        }
    }

    private func pruneOldFrames() {
        let now = CACurrentMediaTime()
        var expired: [UInt32] = []
        for (fid, assembly) in reassembly {
            if now - assembly.createdAt > Self.frameTimeout {
                if assembly.canRecover {
                    deliverFrame(frameId: fid, assembly: assembly, timestampMs: assembly.timestampMs)
                } else {
                    expired.append(fid)
                }
            }
        }
        if !expired.isEmpty {
            for fid in expired {
                reassembly.removeValue(forKey: fid)
            }
            sendPli()
        }
    }

    private func sendPli() {
        let now = CACurrentMediaTime()
        guard now - lastPliTime >= Self.pliMinInterval else { return }
        lastPliTime = now
        guard let conn = connection else { return }
        encryptAndSend(Self.pliMagic, conn: conn, label: "PLI")
    }

    func sendTouch(_ contactData: Data) {
        guard let conn = connection else { return }
        var packet = Data("TOUCH".utf8)
        packet.append(contactData)
        encryptAndSend(packet, conn: conn, label: "touch")
    }

    func sendCameraFrame(_ jpeg: Data, frameId: UInt32) {
        guard let conn = connection else { return }
        let chunkSize = 1300
        let totalChunks = (jpeg.count + chunkSize - 1) / chunkSize

        for i in 0..<totalChunks {
            let start = i * chunkSize
            let end = min(start + chunkSize, jpeg.count)
            let chunk = jpeg.subdata(in: start..<end)

            var packet = Data("CAM".utf8)
            withUnsafeBytes(of: frameId.bigEndian) { packet.append(contentsOf: $0) }
            withUnsafeBytes(of: UInt16(i).bigEndian) { packet.append(contentsOf: $0) }
            withUnsafeBytes(of: UInt16(totalChunks).bigEndian) { packet.append(contentsOf: $0) }
            packet.append(chunk)

            encryptAndSend(packet, conn: conn, label: "cam")
        }
    }

    func sendKey(_ keyData: Data) {
        guard let conn = connection else { return }
        var packet = Data("KEY".utf8)
        packet.append(keyData)
        encryptAndSend(packet, conn: conn, label: "key")
    }

    func sendMicPacket(_ packet: Data) {
        guard let conn = connection else { return }
        encryptAndSend(packet, conn: conn, label: "mic")
    }

    func sendMouse(_ mouseData: Data) {
        guard let conn = connection else { return }
        var packet = Data("MOUSE".utf8)
        packet.append(mouseData)
        encryptAndSend(packet, conn: conn, label: "mouse")
    }

    func sendRawKey(_ keyData: Data) {
        guard let conn = connection else { return }
        var packet = Data("RAWKEY".utf8)
        packet.append(keyData)
        encryptAndSend(packet, conn: conn, label: "rawkey")
    }

    func sendPeripheral(_ periphData: Data) {
        guard let conn = connection else { return }
        var packet = Data("PERIPH".utf8)
        packet.append(periphData)
        encryptAndSend(packet, conn: conn, label: "periph")
    }
}

private final class FrameAssembly {
    let totalData: Int
    let totalParity: Int
    let totalShards: Int
    let isIdr: Bool
    let createdAt: TimeInterval
    let timestampMs: UInt32

    private var receivedShards: [Int: Data]
    private var actualLengths: [Int: Int]
    private var receivedCount: Int = 0

    init(totalData: Int, totalParity: Int, isIdr: Bool, createdAt: TimeInterval, timestampMs: UInt32 = 0) {
        self.totalData = totalData
        self.totalParity = totalParity
        self.totalShards = totalData + totalParity
        self.isIdr = isIdr
        self.createdAt = createdAt
        self.timestampMs = timestampMs
        self.receivedShards = [:]
        self.actualLengths = [:]
    }

    func addChunk(index: Int, payload: Data, actualLen: Int) {
        guard receivedShards[index] == nil else { return }
        receivedShards[index] = payload
        actualLengths[index] = actualLen
        receivedCount += 1
    }

    var isComplete: Bool {
        if totalParity == 0 {
            return receivedCount >= totalData
        }
        // All data shards present — no FEC needed
        let hasAllData = (0..<totalData).allSatisfy { receivedShards[$0] != nil }
        if hasAllData { return true }
        // Enough shards for FEC recovery
        return receivedCount >= totalData
    }

    var canRecover: Bool {
        return receivedCount >= totalData
    }

    func reassemble() -> Data? {
        let hasAllData = (0..<totalData).allSatisfy { receivedShards[$0] != nil }

        if hasAllData {
            return assembleFromDataShards()
        }

        if totalParity > 0 && receivedCount >= totalData {
            return recoverAndAssemble()
        }

        return nil
    }

    private func assembleFromDataShards() -> Data {
        var totalSize = 0
        for i in 0..<totalData {
            let shard = receivedShards[i]!
            totalSize += actualLengths[i] ?? shard.count
        }
        var result = Data(capacity: totalSize)
        for i in 0..<totalData {
            let shard = receivedShards[i]!
            let actualLen = actualLengths[i] ?? shard.count
            if actualLen == shard.count {
                result.append(shard)
            } else {
                result.append(shard[..<actualLen])
            }
        }
        return result
    }

    private func recoverAndAssemble() -> Data? {
        let shardSize = StreamClient.chunkPayload

        var shards: [Data?] = Array(repeating: nil, count: totalShards)
        for (idx, data) in receivedShards {
            if data.count == shardSize {
                shards[idx] = data
            } else if data.count < shardSize {
                var padded = data
                padded.append(Data(count: shardSize - data.count))
                shards[idx] = padded
            } else {
                shards[idx] = data[..<shardSize]
            }
        }

        guard ReedSolomonDecoder.recover(shards: &shards, dataCount: totalData, parityCount: totalParity) else {
            return nil
        }

        var totalSize = 0
        for i in 0..<totalData {
            guard shards[i] != nil else { return nil }
            totalSize += actualLengths[i] ?? shardSize
        }
        var result = Data(capacity: totalSize)
        for i in 0..<totalData {
            let shard = shards[i]!
            let actualLen = actualLengths[i] ?? shard.count
            if actualLen == shard.count {
                result.append(shard)
            } else {
                result.append(shard[..<actualLen])
            }
        }
        return result
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

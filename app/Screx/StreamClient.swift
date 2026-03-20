import Foundation
import Network
import QuartzCore

final class StreamClient {
    private let endpoint: NWEndpoint
    private var connection: NWConnection?
    private let queue = DispatchQueue(label: "screx.stream", qos: .userInteractive)
    let decoder: H264Decoder
    let audioPlayer: AudioPlayer

    var onStatus: ((String) -> Void)?
    var onDisconnect: (() -> Void)?

    /// When true, suppresses the data timeout (e.g. USB is active and no WiFi data is expected)
    var suppressTimeout = false

    private static let headerLen = 14
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

    init(endpoint: NWEndpoint, decoder: H264Decoder, audioPlayer: AudioPlayer) {
        self.endpoint = endpoint
        self.decoder = decoder
        self.audioPlayer = audioPlayer
    }

    func connect() {
        let params = NWParameters.udp
        params.requiredLocalEndpoint = nil

        let conn = NWConnection(to: endpoint, using: params)
        self.connection = conn

        conn.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            switch state {
            case .ready:
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
        keepaliveTimer?.cancel()
        keepaliveTimer = nil
        dataTimeoutTimer?.cancel()
        dataTimeoutTimer = nil
        connection?.cancel()
        connection = nil
    }

    private func sendRegister(_ conn: NWConnection) {
        conn.send(content: Self.registerMagic, completion: .contentProcessed { error in
            if let error {
                print("[stream] register send error: \(error)")
            }
        })
    }

    private func startKeepalive(_ conn: NWConnection) {
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + Self.keepaliveInterval, repeating: Self.keepaliveInterval)
        timer.setEventHandler { [weak self] in
            self?.sendRegister(conn)
        }
        timer.resume()
        keepaliveTimer = timer
    }

    private func startDataTimeout() {
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
        print("[stream] data timeout — daemon appears gone")
        disconnect()
        onStatus?("Daemon disconnected")
        onDisconnect?()
    }

    private func receiveLoop(_ conn: NWConnection) {
        conn.receiveMessage { [weak self] data, _, isComplete, error in
            guard let self else { return }

            if let error {
                self.onStatus?("Receive error: \(error.localizedDescription)")
                self.handleTimeout()
                return
            }

            if let data, data.count >= Self.headerLen {
                self.resetDataTimeout()
                self.handlePacket(data)
            }

            self.receiveLoop(conn)
        }
    }

    private func handlePacket(_ data: Data) {
        let frameId = data.withUnsafeBytes { buf -> UInt32 in
            buf.load(fromByteOffset: 0, as: UInt32.self).bigEndian
        }
        let chunkIdx = data.withUnsafeBytes { buf -> UInt16 in
            buf.load(fromByteOffset: 4, as: UInt16.self).bigEndian
        }
        let totalData = data.withUnsafeBytes { buf -> UInt16 in
            buf.load(fromByteOffset: 6, as: UInt16.self).bigEndian
        }
        let totalParity = data.withUnsafeBytes { buf -> UInt16 in
            buf.load(fromByteOffset: 8, as: UInt16.self).bigEndian
        }
        let flags = data[10]
        let payloadLen = data.withUnsafeBytes { buf -> UInt16 in
            buf.load(fromByteOffset: 12, as: UInt16.self).bigEndian
        }
        let isAudio = (flags & Self.flagAudio) != 0
        let isIdr = (flags & 1) != 0

        let payload = data.subdata(in: Self.headerLen..<data.count)

        if isAudio {
            handleAudioPacket(frameId: frameId, chunkIdx: Int(chunkIdx),
                              totalData: Int(totalData), payload: payload,
                              actualLen: Int(payloadLen))
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
            createdAt: CACurrentMediaTime()
        )
        reassembly[frameId] = assembly

        assembly.addChunk(index: Int(chunkIdx), payload: payload, actualLen: Int(payloadLen))

        if assembly.isComplete {
            deliverFrame(frameId: frameId, assembly: assembly)
        }

        pruneOldFrames()
    }

    private func handleAudioPacket(frameId: UInt32, chunkIdx: Int, totalData: Int,
                                   payload: Data, actualLen: Int) {
        if totalData == 1 {
            audioPlayer.enqueueAudio(payload.prefix(actualLen))
            return
        }

        let assembly = audioReassembly[frameId] ?? AudioAssembly(totalData: totalData)
        audioReassembly[frameId] = assembly
        assembly.addChunk(index: chunkIdx, payload: payload, actualLen: actualLen)

        if assembly.isComplete {
            if let pcm = assembly.reassemble() {
                audioPlayer.enqueueAudio(pcm)
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

    private func deliverFrame(frameId: UInt32, assembly: FrameAssembly) {
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

        decoder.decodeAccessUnit(accessUnit)

        if !decoder.hasReportedFirstFrame {
            decoder.hasReportedFirstFrame = true
            onStatus?("Streaming")
        }
    }

    private func pruneOldFrames() {
        let now = CACurrentMediaTime()
        var expired: [UInt32] = []
        for (fid, assembly) in reassembly {
            if now - assembly.createdAt > Self.frameTimeout {
                if assembly.canRecover {
                    deliverFrame(frameId: fid, assembly: assembly)
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
        connection?.send(content: Self.pliMagic, completion: .contentProcessed { error in
            if let error {
                print("[stream] PLI send error: \(error)")
            }
        })
    }

    /// Sends touch contacts over UDP. Packet: "TOUCH" + raw contact data.
    func sendTouch(_ contactData: Data) {
        guard let conn = connection else { return }
        var packet = Data("TOUCH".utf8)
        packet.append(contactData)
        conn.send(content: packet, completion: .contentProcessed { error in
            if let error {
                print("[stream] touch send error: \(error)")
            }
        })
    }

    /// Sends a camera JPEG frame over UDP, chunked.
    /// Header per chunk: "CAM" + frame_id(4 BE) + chunk_idx(2 BE) + total(2 BE) + jpeg_data
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

            conn.send(content: packet, completion: .contentProcessed { error in
                if let error {
                    print("[stream] cam send error: \(error)")
                }
            })
        }
    }

    /// Sends a keyboard event over UDP. Packet: "KEY" + type(1) + payload.
    func sendKey(_ keyData: Data) {
        guard let conn = connection else { return }
        var packet = Data("KEY".utf8)
        packet.append(keyData)
        conn.send(content: packet, completion: .contentProcessed { error in
            if let error {
                print("[stream] key send error: \(error)")
            }
        })
    }
}

private final class FrameAssembly {
    let totalData: Int
    let totalParity: Int
    let totalShards: Int
    let isIdr: Bool
    let createdAt: TimeInterval

    private var receivedShards: [Int: Data]
    private var actualLengths: [Int: Int]
    private var receivedCount: Int = 0

    init(totalData: Int, totalParity: Int, isIdr: Bool, createdAt: TimeInterval) {
        self.totalData = totalData
        self.totalParity = totalParity
        self.totalShards = totalData + totalParity
        self.isIdr = isIdr
        self.createdAt = createdAt
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
        var result = Data()
        for i in 0..<totalData {
            let shard = receivedShards[i]!
            let actualLen = actualLengths[i] ?? shard.count
            result.append(shard.prefix(actualLen))
        }
        return result
    }

    private func recoverAndAssemble() -> Data? {
        let shardSize = StreamClient.chunkPayload

        // Build shard array for FEC: nil = missing, Data = present
        var shards: [Data?] = Array(repeating: nil, count: totalShards)
        for (idx, data) in receivedShards {
            // Pad to shardSize for RS
            var padded = data
            if padded.count < shardSize {
                padded.append(Data(count: shardSize - padded.count))
            } else if padded.count > shardSize {
                padded = padded.prefix(shardSize)
            }
            shards[idx] = padded
        }

        guard ReedSolomonDecoder.recover(shards: &shards, dataCount: totalData, parityCount: totalParity) else {
            return nil
        }

        // Assemble from recovered data shards
        var result = Data()
        for i in 0..<totalData {
            guard let shard = shards[i] else { return nil }
            let actualLen = actualLengths[i] ?? shard.count
            result.append(shard.prefix(actualLen))
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
        var result = Data()
        for i in 0..<totalData {
            guard let chunk = chunks[i] else { return nil }
            let len = actualLengths[i] ?? chunk.count
            result.append(chunk.prefix(len))
        }
        return result
    }
}

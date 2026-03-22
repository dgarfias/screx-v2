import Foundation
import Network
import QuartzCore

final class USBListener {
    private var listener: NWListener?
    private var connection: NWConnection?
    private let queue = DispatchQueue(label: "screx.usb", qos: .userInteractive)

    private let decoder: VideoDecoder
    private let audioPlayer: AudioPlayer
    private let avSync: AVSyncState

    var onStatus: ((String) -> Void)?
    var onConnected: (() -> Void)?
    var onDisconnected: (() -> Void)?

    private static let msgVideo: UInt8 = 0x01
    private static let msgAudio: UInt8 = 0x02
    private static let msgControl: UInt8 = 0x03

    private var lastPliTime: TimeInterval = 0
    private static let pliMinInterval: TimeInterval = 1.0

    private var hasReportedFirstFrame = false
    private var recvBuffer = Data()

    init(decoder: VideoDecoder, audioPlayer: AudioPlayer, avSync: AVSyncState) {
        self.decoder = decoder
        self.audioPlayer = audioPlayer
        self.avSync = avSync
    }

    func start() {
        do {
            let params = NWParameters.tcp
            params.requiredLocalEndpoint = NWEndpoint.hostPort(host: .ipv4(.any), port: 9000)

            let l = try NWListener(using: params)
            self.listener = l

            l.stateUpdateHandler = { [weak self] state in
                switch state {
                case .ready:
                    self?.onStatus?("USB: listening on port 9000")
                    print("[usb] listener ready on port 9000")
                case .failed(let error):
                    print("[usb] listener failed: \(error)")
                    self?.onStatus?("USB listener failed: \(error.localizedDescription)")
                default:
                    break
                }
            }

            l.newConnectionHandler = { [weak self] conn in
                self?.handleNewConnection(conn)
            }

            l.start(queue: queue)
            print("[usb] TCP listener starting on port 9000")
        } catch {
            print("[usb] failed to create listener: \(error)")
            onStatus?("USB listener error: \(error.localizedDescription)")
        }
    }

    func stop() {
        connection?.cancel()
        connection = nil
        listener?.cancel()
        listener = nil
        hasReportedFirstFrame = false
        recvBuffer.removeAll()
        print("[usb] listener stopped")
    }

    private func handleNewConnection(_ conn: NWConnection) {
        // Only allow one USB connection at a time
        connection?.cancel()
        connection = conn
        hasReportedFirstFrame = false
        recvBuffer.removeAll()

        conn.stateUpdateHandler = { [weak self] state in
            switch state {
            case .ready:
                print("[usb] TCP connection established from daemon")
                self?.onStatus?("USB: connected")
                self?.onConnected?()
                self?.readLoop(conn)
            case .failed(let error):
                print("[usb] connection failed: \(error)")
                self?.handleDisconnect()
            case .cancelled:
                print("[usb] connection cancelled")
                self?.handleDisconnect()
            default:
                break
            }
        }

        conn.start(queue: queue)
    }

    private func handleDisconnect() {
        connection = nil
        hasReportedFirstFrame = false
        recvBuffer.removeAll()
        onDisconnected?()
    }

    private func readLoop(_ conn: NWConnection) {
        conn.receive(minimumIncompleteLength: 1, maximumLength: 256 * 1024) { [weak self] data, _, isComplete, error in
            guard let self else { return }

            if let data, !data.isEmpty {
                self.recvBuffer.append(data)
                self.processBuffer()
            }

            if isComplete {
                print("[usb] TCP stream ended")
                self.handleDisconnect()
                return
            }

            if let error {
                print("[usb] TCP read error: \(error)")
                self.handleDisconnect()
                return
            }

            self.readLoop(conn)
        }
    }

    private func processBuffer() {
        while recvBuffer.count >= 4 {
            // Read 4-byte length header manually to avoid alignment issues
            let b = recvBuffer.prefix(4)
            let msgLen = Int(b[b.startIndex]) << 24
                       | Int(b[b.startIndex + 1]) << 16
                       | Int(b[b.startIndex + 2]) << 8
                       | Int(b[b.startIndex + 3])

            guard msgLen > 0, msgLen < 10_000_000 else {
                recvBuffer.removeAll()
                print("[usb] invalid frame length \(msgLen), resetting buffer")
                break
            }

            let totalNeeded = 4 + msgLen
            guard recvBuffer.count >= totalNeeded else { break }

            // Copy out the message bytes into a fresh contiguous Data
            let msgData = Data(recvBuffer.prefix(totalNeeded).dropFirst(4))
            recvBuffer = Data(recvBuffer.dropFirst(totalNeeded))

            guard msgData.count >= 1 else { continue }

            let msgType = msgData[0]

            switch msgType {
            case Self.msgVideo:
                // type(1) + is_idr(1) + codec_id(1) + timestamp_ms(4) + annex_b
                guard msgData.count >= 7 else { continue }
                let codecId = msgData[msgData.startIndex + 2]
                let wantCodec: VideoCodecType = codecId == 0x01 ? .h265 : .h264
                if wantCodec != decoder.codec {
                    decoder.setCodec(wantCodec)
                }
                let o = msgData.startIndex + 3
                let tsMs: UInt32 = UInt32(msgData[o]) << 24 | UInt32(msgData[o+1]) << 16
                                 | UInt32(msgData[o+2]) << 8 | UInt32(msgData[o+3])
                let annexB = Data(msgData.dropFirst(7))
                avSync.updateVideo(timestamp: tsMs)
                decoder.decodeAccessUnit(annexB)

                if !hasReportedFirstFrame {
                    hasReportedFirstFrame = true
                    decoder.hasReportedFirstFrame = true
                    onStatus?("USB: streaming")
                }

            case Self.msgAudio:
                // type(1) + timestamp_ms(4) + pcm
                guard msgData.count >= 5 else { continue }
                let o = msgData.startIndex + 1
                let tsMs: UInt32 = UInt32(msgData[o]) << 24 | UInt32(msgData[o+1]) << 16
                                 | UInt32(msgData[o+2]) << 8 | UInt32(msgData[o+3])
                let pcm = Data(msgData.dropFirst(5))
                audioPlayer.enqueueAudio(pcm, timestampMs: tsMs)

            case Self.msgControl:
                break

            default:
                break
            }
        }
    }

    func sendPli() {
        let now = CACurrentMediaTime()
        guard now - lastPliTime >= Self.pliMinInterval else { return }
        lastPliTime = now

        guard let conn = connection else { return }

        // Build framed PLI: length(4) + type(1) + "PLI"(3)
        var frame = Data()
        let payloadLen: UInt32 = 4  // 1 byte type + 3 bytes "PLI"
        withUnsafeBytes(of: payloadLen.bigEndian) { frame.append(contentsOf: $0) }
        frame.append(Self.msgControl)
        frame.append(Data("PLI".utf8))

        conn.send(content: frame, completion: .contentProcessed { error in
            if let error {
                print("[usb] PLI send error: \(error)")
            }
        })
    }

    /// Sends touch contacts over USB TCP. Framed control message: "TOUCH" + raw contact data.
    func sendTouch(_ contactData: Data) {
        guard let conn = connection else { return }

        let touchPayload = Data("TOUCH".utf8) + contactData
        let payloadLen = UInt32(1 + touchPayload.count) // type byte + "TOUCH" + contacts

        var frame = Data()
        withUnsafeBytes(of: payloadLen.bigEndian) { frame.append(contentsOf: $0) }
        frame.append(Self.msgControl)
        frame.append(touchPayload)

        conn.send(content: frame, completion: .contentProcessed { error in
            if let error {
                print("[usb] touch send error: \(error)")
            }
        })
    }

    /// Sends a camera JPEG frame over USB TCP. Single framed control message.
    func sendCameraFrame(_ jpeg: Data) {
        guard let conn = connection else { return }

        let camPayload = Data("CAM".utf8) + jpeg
        let payloadLen = UInt32(1 + camPayload.count)

        var frame = Data()
        withUnsafeBytes(of: payloadLen.bigEndian) { frame.append(contentsOf: $0) }
        frame.append(Self.msgControl)
        frame.append(camPayload)

        conn.send(content: frame, completion: .contentProcessed { error in
            if let error {
                print("[usb] cam send error: \(error)")
            }
        })
    }

    /// Sends a keyboard event over USB TCP. Framed control: "KEY" + type(1) + payload.
    func sendKey(_ keyData: Data) {
        guard let conn = connection else { return }

        let keyPayload = Data("KEY".utf8) + keyData
        let payloadLen = UInt32(1 + keyPayload.count)

        var frame = Data()
        withUnsafeBytes(of: payloadLen.bigEndian) { frame.append(contentsOf: $0) }
        frame.append(Self.msgControl)
        frame.append(keyPayload)

        conn.send(content: frame, completion: .contentProcessed { error in
            if let error {
                print("[usb] key send error: \(error)")
            }
        })
    }

    /// Sends a mic Opus packet over USB TCP. Pre-built: "MIC" + seq(4) + opus_data.
    func sendMicPacket(_ micData: Data) {
        guard let conn = connection else { return }

        let payloadLen = UInt32(1 + micData.count)

        var frame = Data()
        withUnsafeBytes(of: payloadLen.bigEndian) { frame.append(contentsOf: $0) }
        frame.append(Self.msgControl)
        frame.append(micData)

        conn.send(content: frame, completion: .contentProcessed { error in
            if let error {
                print("[usb] mic send error: \(error)")
            }
        })
    }

    func sendMouse(_ mouseData: Data) {
        guard let conn = connection else { return }
        let payload = Data("MOUSE".utf8) + mouseData
        let payloadLen = UInt32(1 + payload.count)
        var frame = Data()
        withUnsafeBytes(of: payloadLen.bigEndian) { frame.append(contentsOf: $0) }
        frame.append(Self.msgControl)
        frame.append(payload)
        conn.send(content: frame, completion: .contentProcessed { error in
            if let error { print("[usb] mouse send error: \(error)") }
        })
    }

    func sendRawKey(_ keyData: Data) {
        guard let conn = connection else { return }
        let payload = Data("RAWKEY".utf8) + keyData
        let payloadLen = UInt32(1 + payload.count)
        var frame = Data()
        withUnsafeBytes(of: payloadLen.bigEndian) { frame.append(contentsOf: $0) }
        frame.append(Self.msgControl)
        frame.append(payload)
        conn.send(content: frame, completion: .contentProcessed { error in
            if let error { print("[usb] rawkey send error: \(error)") }
        })
    }

    func sendPeripheral(_ periphData: Data) {
        guard let conn = connection else { return }
        let payload = Data("PERIPH".utf8) + periphData
        let payloadLen = UInt32(1 + payload.count)
        var frame = Data()
        withUnsafeBytes(of: payloadLen.bigEndian) { frame.append(contentsOf: $0) }
        frame.append(Self.msgControl)
        frame.append(payload)
        conn.send(content: frame, completion: .contentProcessed { error in
            if let error { print("[usb] periph send error: \(error)") }
        })
    }

    var isConnected: Bool {
        connection != nil
    }
}

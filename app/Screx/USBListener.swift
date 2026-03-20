import Foundation
import Network
import QuartzCore

final class USBListener {
    private var listener: NWListener?
    private var connection: NWConnection?
    private let queue = DispatchQueue(label: "screx.usb", qos: .userInteractive)

    private let decoder: H264Decoder
    private let audioPlayer: AudioPlayer

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

    init(decoder: H264Decoder, audioPlayer: AudioPlayer) {
        self.decoder = decoder
        self.audioPlayer = audioPlayer
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
                self?.audioPlayer.start()
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
            let msgLen = recvBuffer.withUnsafeBytes { buf -> UInt32 in
                buf.load(fromByteOffset: 0, as: UInt32.self).bigEndian
            }

            let totalNeeded = 4 + Int(msgLen)
            guard recvBuffer.count >= totalNeeded else { break }

            let msgData = recvBuffer.subdata(in: 4..<totalNeeded)
            recvBuffer.removeFirst(totalNeeded)

            guard !msgData.isEmpty else { continue }

            let msgType = msgData[msgData.startIndex]

            switch msgType {
            case Self.msgVideo:
                guard msgData.count >= 3 else { continue }
                let annexB = msgData.subdata(in: (msgData.startIndex + 2)..<msgData.endIndex)
                decoder.decodeAccessUnit(annexB)

                if !hasReportedFirstFrame {
                    hasReportedFirstFrame = true
                    decoder.hasReportedFirstFrame = true
                    onStatus?("USB: streaming")
                }

            case Self.msgAudio:
                guard msgData.count >= 2 else { continue }
                let pcm = msgData.subdata(in: (msgData.startIndex + 1)..<msgData.endIndex)
                audioPlayer.enqueueAudio(pcm)

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

    var isConnected: Bool {
        connection != nil
    }
}

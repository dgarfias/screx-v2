import Foundation
import Network
import CryptoKit
import QuartzCore

final class NetworkControlClient {
    private let connection: NWConnection
    private let sessionKey: SymmetricKey
    private let queue = DispatchQueue(label: "screx.netcontrol", qos: .userInteractive)
    private let debugId = String(UUID().uuidString.prefix(6))
    private static let maxControlFrame = 65536

    /// Latency diagnostics: a `PING`/`PONG` round trip every second on the control
    /// channel, averaged into an on-screen RTT + jitter reading.
    private static let pingInterval: TimeInterval = 1.0
    private static let maxLatencySamples = 8

    private var sendSeq: UInt32 = 0
    private var isClosed = false
    private var recvBuffer = Data()
    private var readOffset = 0
    private static let recvBufferCompactThreshold = 64 * 1024
    private var pingTimer: DispatchSourceTimer?
    private var pendingPingSentAt: TimeInterval?
    private var latencySamples: [Double] = []

    var onDisconnect: (() -> Void)?
    var onTraffic: ((Int, Int) -> Void)?
    var onCapabilities: ((DaemonCapabilities) -> Void)?
    var onDisplayStatus: ((DisplayStatus) -> Void)?
    /// (rttMs, jitterMs) of the control-channel latency probe.
    var onLatency: ((Double, Double) -> Void)?

    /// `initialData`: control bytes already read during the handshake (see `PairingService`).
    init(connection: NWConnection, sessionKey: SymmetricKey, initialData: Data = Data()) {
        self.connection = connection
        self.sessionKey = sessionKey
        self.recvBuffer = Data(initialData)
    }

    private func log(_ message: String) {
        print("[control \(debugId)] \(message)")
    }

    func start() {
        connection.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            self.log("tcp control state -> \(state)")
            switch state {
            case .failed(let error):
                self.log("tcp control failed: \(error.localizedDescription)")
                self.handleDisconnect()
            case .cancelled:
                self.log("tcp control cancelled")
                self.handleDisconnect()
            default:
                break
            }
        }
        startPingLoop()
        // Parse frames that arrived with the handshake reply before reading anything new, so
        // they keep their order. No receive callback can run yet: the loop hasn't started.
        if !recvBuffer.isEmpty {
            log("processing \(recvBuffer.count) control bytes received with the handshake")
            processReceiveBuffer()
        }
        receiveLoop()
    }

    /// Latency probe. The daemon answers each `PING` with a `PONG` on the same
    /// control channel; `handleInboundPayload` times the round trip.
    private func startPingLoop() {
        pingTimer?.cancel()
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + Self.pingInterval, repeating: Self.pingInterval)
        timer.setEventHandler { [weak self] in
            self?.sendPing()
        }
        timer.resume()
        pingTimer = timer
    }

    private func sendPing() {
        queue.async { [weak self] in
            guard let self, !self.isClosed else { return }
            self.pendingPingSentAt = CACurrentMediaTime()
            self.sendFrame(Data([ScrexMessage.Client.ping]), label: "ping")
        }
    }

    func disconnect() {
        queue.async { [weak self] in
            guard let self, !self.isClosed else { return }
            self.isClosed = true
            self.pingTimer?.cancel()
            self.pingTimer = nil
            self.pendingPingSentAt = nil
            self.latencySamples.removeAll()
            self.connection.cancel()
        }
    }

    func disconnectGracefully() {
        queue.async { [weak self] in
            guard let self, !self.isClosed else { return }
            guard let frame = self.makeFrame(Data([ScrexMessage.Client.disconnect]), label: "disconnect") else {
                self.log("graceful disconnect frame build failed, cancelling")
                self.isClosed = true
                self.pingTimer?.cancel()
                self.pingTimer = nil
                self.connection.cancel()
                return
            }

            self.isClosed = true
            self.pingTimer?.cancel()
            self.pingTimer = nil
            self.pendingPingSentAt = nil
            self.log("sending graceful disconnect")
            self.connection.send(content: frame, completion: .contentProcessed { error in
                if let error {
                    self.log("disconnect send error: \(error.localizedDescription)")
                } else {
                    self.log("graceful disconnect sent")
                }
                self.connection.cancel()
            })

            self.queue.asyncAfter(deadline: .now() + 0.2) { [weak self] in
                guard let self, self.isClosed else { return }
                self.connection.cancel()
            }
        }
    }

    private func handleDisconnect() {
        guard !isClosed else { return }
        isClosed = true
        onDisconnect?()
    }

    private func protocolFailure(_ message: String) {
        log(message)
        recvBuffer.removeAll()
        readOffset = 0
        connection.cancel()
        handleDisconnect()
    }

    private func receiveLoop() {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 4096) { [weak self] data, _, isComplete, error in
            guard let self else { return }

            if let error {
                self.log("tcp control read error: \(error.localizedDescription)")
                self.handleDisconnect()
                return
            }

            if isComplete {
                self.log("tcp control stream completed")
                self.handleDisconnect()
                return
            }

            if let data, !data.isEmpty {
                self.onTraffic?(data.count, 0)
                self.recvBuffer.append(data)
                self.processReceiveBuffer()
            }

            self.receiveLoop()
        }
    }

    private func processReceiveBuffer() {
        while readOffset + 4 <= recvBuffer.count {
            let bodyLen = recvBuffer[readOffset..<readOffset + 4].withUnsafeBytes { raw -> Int in
                let bytes = raw.bindMemory(to: UInt8.self)
                return Int(bytes[0]) << 24
                    | Int(bytes[1]) << 16
                    | Int(bytes[2]) << 8
                    | Int(bytes[3])
            }

            guard bodyLen >= 4 + ScrexCrypto.tagLen, bodyLen <= Self.maxControlFrame else {
                protocolFailure("invalid inbound tcp control frame length: \(bodyLen)")
                return
            }

            let totalLen = 4 + bodyLen
            guard readOffset + totalLen <= recvBuffer.count else { return }

            let frame = recvBuffer.subdata(in: readOffset..<readOffset + totalLen)
            readOffset += totalLen

            let body = frame.dropFirst(4)
            let seqNum = body.prefix(4).withUnsafeBytes { raw -> UInt32 in
                let bytes = raw.bindMemory(to: UInt8.self)
                return UInt32(bytes[0]) << 24
                    | UInt32(bytes[1]) << 16
                    | UInt32(bytes[2]) << 8
                    | UInt32(bytes[3])
            }

            let aad = Data(body.prefix(4))
            let ciphertext = Data(body.dropFirst(4))
            guard let plaintext = ScrexCrypto.decrypt(
                key: sessionKey,
                nonce: ScrexCrypto.nonceControlServer(seqNum: seqNum),
                ciphertextAndTag: ciphertext,
                aad: aad
            ) else {
                log("failed to decrypt inbound tcp control frame")
                continue
            }

            handleInboundPayload(plaintext)
        }

        if readOffset > Self.recvBufferCompactThreshold {
            recvBuffer.removeSubrange(0..<readOffset)
            readOffset = 0
        }
    }

    private func handleInboundPayload(_ payload: Data) {
        guard let type = payload.first else { return }
        let body = payload.dropFirst()
        switch type {
        case ScrexMessage.Daemon.pong:
            queue.async { [weak self] in
                guard let self, let sentAt = self.pendingPingSentAt else { return }
                self.pendingPingSentAt = nil
                let rttMs = (CACurrentMediaTime() - sentAt) * 1000.0
                self.latencySamples.append(rttMs)
                if self.latencySamples.count > Self.maxLatencySamples {
                    self.latencySamples.removeFirst(self.latencySamples.count - Self.maxLatencySamples)
                }
                let pairs = zip(self.latencySamples, self.latencySamples.dropFirst())
                let deltaSum = pairs.reduce(0.0) { $0 + abs($1.1 - $1.0) }
                let jitterMs = deltaSum / Double(max(1, self.latencySamples.count - 1))
                self.onLatency?(rttMs, jitterMs)
            }
        case ScrexMessage.Daemon.caps:
            guard let capabilities = DaemonCapabilities.parse(Data(body)) else {
                log("failed to parse CAPS payload: \(payload.count) bytes")
                return
            }
            onCapabilities?(capabilities)
        case ScrexMessage.Daemon.displayStatus:
            if let status = DisplayStatus.parse(Data(body)) { onDisplayStatus?(status) }
        default:
            log("unexpected inbound tcp control message type 0x\(String(type, radix: 16)) (\(payload.count) bytes)")
        }
    }

    private func makeFrame(_ payload: Data, label: String) -> Data? {
        let seq = sendSeq
        sendSeq = sendSeq &+ 1

        var seqData = Data(count: 4)
        seqData.withUnsafeMutableBytes { buf in
            buf.storeBytes(of: seq.bigEndian, as: UInt32.self)
        }

        let nonce = ScrexCrypto.nonceControlClient(seqNum: seq)
        guard let encrypted = ScrexCrypto.encrypt(
            key: sessionKey,
            nonce: nonce,
            plaintext: payload,
            aad: seqData
        ) else {
            log("\(label) encrypt failed")
            return nil
        }

        let bodyLen = UInt32(seqData.count + encrypted.count)
        var frame = Data()
        withUnsafeBytes(of: bodyLen.bigEndian) { frame.append(contentsOf: $0) }
        frame.append(seqData)
        frame.append(encrypted)
        return frame
    }

    private func sendFrame(_ payload: Data, label: String) {
        queue.async { [weak self] in
            guard let self, !self.isClosed else { return }
            guard let frame = self.makeFrame(payload, label: label) else { return }

            self.connection.send(content: frame, completion: .contentProcessed { error in
                if let error {
                    self.log("\(label) send error: \(error.localizedDescription)")
                    self.handleDisconnect()
                } else {
                    self.onTraffic?(0, frame.count)
                }
            })
        }
    }

    /// Sends one `type(u8) + body` control message.
    private func send(_ type: UInt8, _ body: Data = Data(), label: String) {
        var payload = Data([type])
        payload.append(body)
        sendFrame(payload, label: label)
    }

    func sendPli(display: StreamDisplayID, generation: UInt32) {
        var body = Data([display.rawValue])
        withUnsafeBytes(of: generation.bigEndian) { body.append(contentsOf: $0) }
        send(ScrexMessage.Client.pli, body, label: "PLI")
    }

    func sendDisplayPresented(revision: UInt32, display: StreamDisplayID, generation: UInt32) {
        var body = Data()
        withUnsafeBytes(of: revision.bigEndian) { body.append(contentsOf: $0) }
        body.append(display.rawValue)
        withUnsafeBytes(of: generation.bigEndian) { body.append(contentsOf: $0) }
        send(ScrexMessage.Client.displayPresented, body, label: "display presented")
    }

    func sendSpeakerState(isEnabled: Bool) {
        send(ScrexMessage.Client.speaker, Data([isEnabled ? 1 : 0]), label: isEnabled ? "speaker-on" : "speaker-off")
    }

    /// Tells the daemon to create or destroy the virtual microphone device.
    func sendMicState(isEnabled: Bool) {
        send(ScrexMessage.Client.mic, Data([isEnabled ? 1 : 0]), label: isEnabled ? "mic-on" : "mic-off")
    }

    /// Sends the camera configuration (width, height, fps) to the daemon so it
    /// can create or reconfigure the virtual webcam device to match. All zero = off.
    func sendCameraConfig(width: UInt16, height: UInt16, fps: UInt16) {
        var body = Data()
        withUnsafeBytes(of: width.bigEndian) { body.append(contentsOf: $0) }
        withUnsafeBytes(of: height.bigEndian) { body.append(contentsOf: $0) }
        withUnsafeBytes(of: fps.bigEndian) { body.append(contentsOf: $0) }
        send(ScrexMessage.Client.camera, body, label: "camera-\(width)x\(height)@\(fps)")
    }

    func sendTouch(_ contactData: Data) {
        send(ScrexMessage.Client.touch, contactData, label: "touch")
    }

    func sendKey(_ keyData: Data) {
        send(ScrexMessage.Client.key, keyData, label: "key")
    }

    func sendMouse(_ mouseData: Data) {
        send(ScrexMessage.Client.mouse, mouseData, label: "mouse")
    }

    func sendRawKey(_ keyData: Data) {
        send(ScrexMessage.Client.rawKey, keyData, label: "rawkey")
    }

    func sendPeripheral(_ periphData: Data) {
        send(ScrexMessage.Client.peripheral, periphData, label: "periph")
    }

    func sendGamepad(_ gamepadData: Data) {
        send(ScrexMessage.Client.gamepad, gamepadData, label: "gamepad")
    }

    /// Sends the stream settings to the daemon in reply to `CAPS`.
    func sendStreamSettings(_ settings: StreamSettings) {
        send(ScrexMessage.Client.settings, settings.encode(), label: "settings")
    }
}

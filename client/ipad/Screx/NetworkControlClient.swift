import Foundation
import Network
import CryptoKit

final class NetworkControlClient {
    private let connection: NWConnection
    private let sessionKey: SymmetricKey
    private let queue = DispatchQueue(label: "screx.netcontrol", qos: .userInteractive)
    private let debugId = String(UUID().uuidString.prefix(6))
    private static let disconnectMagic = Data("DISCONNECT".utf8)
    private static let speakerMagic = Data("SPKR".utf8)
    private static let micCfgMagic = Data("MICCFG".utf8)
    private static let cameraCfgMagic = Data("CAMCFG".utf8)
    private static let hostnameMagic = Data("HOST".utf8)
    private static let capsMagic = Data("CAPS".utf8)
    private static let maxControlFrame = 65536

    private var sendSeq: UInt32 = 0
    private var isClosed = false
    private var recvBuffer = Data()
    private var readOffset = 0
    private static let recvBufferCompactThreshold = 64 * 1024

    var onDisconnect: (() -> Void)?
    var onTraffic: ((Int, Int) -> Void)?
    var onHostname: ((String) -> Void)?
    var onCapabilities: ((DaemonCapabilities) -> Void)?

    init(connection: NWConnection, sessionKey: SymmetricKey) {
        self.connection = connection
        self.sessionKey = sessionKey
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
        receiveLoop()
    }

    func disconnect() {
        queue.async { [weak self] in
            guard let self, !self.isClosed else { return }
            self.isClosed = true
            self.connection.cancel()
        }
    }

    func disconnectGracefully() {
        queue.async { [weak self] in
            guard let self, !self.isClosed else { return }
            guard let frame = self.makeFrame(Self.disconnectMagic, label: "disconnect") else {
                self.log("graceful disconnect frame build failed, cancelling")
                self.isClosed = true
                self.connection.cancel()
                return
            }

            self.isClosed = true
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
        if payload.starts(with: Self.hostnameMagic) {
            let hostData = payload.dropFirst(Self.hostnameMagic.count)
            let hostname = String(decoding: hostData, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
            guard !hostname.isEmpty else { return }
            onHostname?(hostname)
        } else if payload.starts(with: Self.capsMagic) {
            guard let capabilities = DaemonCapabilities.parse(payload) else {
                log("failed to parse CAPS payload: \(payload.count) bytes")
                return
            }
            onCapabilities?(capabilities)
        } else {
            log("unexpected inbound tcp control payload: \(payload.count) bytes")
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

    func sendPli() {
        sendFrame(Data("PLI".utf8), label: "PLI")
    }

    func sendSpeakerState(isEnabled: Bool) {
        var payload = Self.speakerMagic
        payload.append(isEnabled ? 1 : 0)
        sendFrame(payload, label: isEnabled ? "speaker-on" : "speaker-off")
    }

    /// Tells the daemon to create or destroy the virtual microphone device.
    func sendMicState(isEnabled: Bool) {
        var payload = Self.micCfgMagic
        payload.append(isEnabled ? 1 : 0)
        sendFrame(payload, label: isEnabled ? "mic-on" : "mic-off")
    }

    /// Sends the camera configuration (width, height, fps) to the daemon so it
    /// can create or reconfigure the virtual webcam device to match.
    func sendCameraConfig(width: UInt16, height: UInt16, fps: UInt16) {
        var payload = Self.cameraCfgMagic
        withUnsafeBytes(of: width.bigEndian) { payload.append(contentsOf: $0) }
        withUnsafeBytes(of: height.bigEndian) { payload.append(contentsOf: $0) }
        withUnsafeBytes(of: fps.bigEndian) { payload.append(contentsOf: $0) }
        sendFrame(payload, label: "camcfg-\(width)x\(height)@\(fps)")
    }

    func sendTouch(_ contactData: Data) {
        var payload = Data("TOUCH".utf8)
        payload.append(contactData)
        sendFrame(payload, label: "touch")
    }

    func sendKey(_ keyData: Data) {
        var payload = Data("KEY".utf8)
        payload.append(keyData)
        sendFrame(payload, label: "key")
    }

    func sendMouse(_ mouseData: Data) {
        var payload = Data("MOUSE".utf8)
        payload.append(mouseData)
        sendFrame(payload, label: "mouse")
    }

    func sendRawKey(_ keyData: Data) {
        var payload = Data("RAWKEY".utf8)
        payload.append(keyData)
        sendFrame(payload, label: "rawkey")
    }

    func sendPeripheral(_ periphData: Data) {
        var payload = Data("PERIPH".utf8)
        payload.append(periphData)
        sendFrame(payload, label: "periph")
    }

    func sendGamepad(_ gamepadData: Data) {
        var payload = Data("GPAD".utf8)
        payload.append(gamepadData)
        sendFrame(payload, label: "gpad")
    }

    /// Sends client-proposed stream settings (resolution/fps/codec/bitrate) to the
    /// daemon in response to a `CAPS` message, per the capability-negotiation protocol.
    func sendStreamSettings(_ settings: StreamSettings) {
        sendFrame(settings.encode(), label: "stng")
    }
}

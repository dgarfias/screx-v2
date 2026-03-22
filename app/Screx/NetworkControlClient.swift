import Foundation
import Network
import CryptoKit

final class NetworkControlClient {
    private let connection: NWConnection
    private let sessionKey: SymmetricKey
    private let queue = DispatchQueue(label: "screx.netcontrol", qos: .userInteractive)
    private let debugId = String(UUID().uuidString.prefix(6))

    private var sendSeq: UInt32 = 0
    private var isClosed = false

    var onDisconnect: (() -> Void)?

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

    private func handleDisconnect() {
        guard !isClosed else { return }
        isClosed = true
        onDisconnect?()
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
                self.log("unexpected inbound tcp control payload: \(data.count) bytes")
            }

            self.receiveLoop()
        }
    }

    private func sendFrame(_ payload: Data, label: String) {
        queue.async { [weak self] in
            guard let self, !self.isClosed else { return }

            let seq = self.sendSeq
            self.sendSeq = self.sendSeq &+ 1

            var seqData = Data(count: 4)
            seqData.withUnsafeMutableBytes { buf in
                buf.storeBytes(of: seq.bigEndian, as: UInt32.self)
            }

            let nonce = ScrexCrypto.nonceControlClient(seqNum: seq)
            guard let encrypted = ScrexCrypto.encrypt(
                key: self.sessionKey,
                nonce: nonce,
                plaintext: payload,
                aad: seqData
            ) else {
                self.log("\(label) encrypt failed")
                return
            }

            let bodyLen = UInt32(seqData.count + encrypted.count)
            var frame = Data()
            withUnsafeBytes(of: bodyLen.bigEndian) { frame.append(contentsOf: $0) }
            frame.append(seqData)
            frame.append(encrypted)

            self.connection.send(content: frame, completion: .contentProcessed { error in
                if let error {
                    self.log("\(label) send error: \(error.localizedDescription)")
                    self.handleDisconnect()
                }
            })
        }
    }

    func sendPli() {
        sendFrame(Data("PLI".utf8), label: "PLI")
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
}

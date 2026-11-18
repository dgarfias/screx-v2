import Foundation
import Network
import Security
import CryptoKit

/// One QUIC connection, with separate reliable control/input streams and a
/// datagram flow for media. All stream/datagram sends are serialized on `queue`.
nonisolated final class QUICSession: @unchecked Sendable {
    let group: NWConnectionGroup
    let queue = DispatchQueue(label: "screx.quic", qos: .userInteractive)
    private(set) var control: NWConnection?
    private(set) var input: NWConnection?
    private(set) var media: NWConnection?
    private let peerCertificate: PeerCertificate
    var onFailure: ((String) -> Void)?
    private var onDatagram: ((Data) -> Void)?
    private var closed = false
    static let alpn = "screx-quic-1"
    static let chunkPayload = 1000

    init(endpoint: NWEndpoint, pin: Data?) {
        let peer = PeerCertificate()
        peerCertificate = peer
        let options = NWProtocolQUIC.Options(alpn: [Self.alpn])
        options.maxDatagramFrameSize = 65535
        options.idleTimeout = 15000
        options.initialMaxStreamsBidirectional = 2
        options.initialMaxStreamsUnidirectional = 0
        options.direction = .bidirectional
        sec_protocol_options_set_verify_block(options.securityProtocolOptions, { _, trust, complete in
            let ref = sec_trust_copy_ref(trust).takeRetainedValue()
            guard let chain = SecTrustCopyCertificateChain(ref) as? [SecCertificate],
                  let certificate = chain.first else { complete(false); return }
            let hash = Data(SHA256.hash(data: SecCertificateCopyData(certificate) as Data))
            peer.lock.lock()
            peer.hash = hash
            peer.lock.unlock()
            // A missing pin is allowed only for the bootstrap TLS channel.
            // CPace must authenticate this exact certificate before any session is exposed.
            complete(pin == nil || pin == hash)
        }, queue)
        let parameters = NWParameters(quic: options)
        group = NWConnectionGroup(with: NWMultiplexGroup(to: endpoint), using: parameters)
    }

    var peerCertificateHash: Data {
        peerCertificate.lock.lock()
        defer { peerCertificate.lock.unlock() }
        return peerCertificate.hash
    }
    func setDatagramHandler(_ handler: ((Data) -> Void)?) {
        queue.async { [self] in onDatagram = handler }
    }

    func start(ready: @escaping (NWConnection) -> Void) {
        group.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            print("[quic] connection group: \(state)")
            switch state {
            case .ready:
                self.openControl(ready: ready)
            case .failed(let error): self.fail(error.localizedDescription)
            case .cancelled:
                if !self.closed { self.fail("QUIC connection closed") }
            default: break
            }
        }
        group.newConnectionHandler = { connection in connection.cancel() }
        group.start(queue: queue)
    }

    private func openControl(ready: @escaping (NWConnection) -> Void) {
        guard control == nil else { return }
        guard let control = group.extract() else {
            fail("Cannot open QUIC control stream"); return
        }
        self.control = control
        control.stateUpdateHandler = { [weak self, weak control] state in
            guard let self, let control else { return }
            switch state {
            case .ready:
                ready(control)
            case .failed(let error): self.fail(error.localizedDescription)
            default: break
            }
        }
        control.start(queue: queue)
    }

    func exporter() throws -> Data {
        guard let metadata = control?.metadata(definition: NWProtocolQUIC.definition) as? NWProtocolQUIC.Metadata else {
            throw Self.error("QUIC security metadata unavailable")
        }
        let label = "EXPORTER-screx-quic-pair-v1"
        let output = label.withCString {
            sec_protocol_metadata_create_secret(metadata.securityProtocolMetadata, label.utf8.count, $0, 32)
        }
        guard let output else { throw Self.error("TLS exporter unavailable") }
        return Data(output as DispatchData)
    }

    /// Only called after the server's authentication success.
    func openMedia(ready: @escaping () -> Void) {
        let streamOptions = NWProtocolQUIC.Options()
        streamOptions.direction = .bidirectional
        let datagramOptions = NWProtocolQUIC.Options()
        datagramOptions.isDatagram = true
        guard let input = NWConnection(from: group, using: streamOptions),
              let media = NWConnection(from: group, using: datagramOptions) else {
            fail("Cannot open QUIC input/media flows"); return
        }
        self.input = input
        self.media = media
        input.stateUpdateHandler = { [weak self] state in
            if case .failed(let error) = state { self?.fail(error.localizedDescription) }
        }
        media.stateUpdateHandler = { [weak self, weak media] state in
            guard let self, let media else { return }
            switch state {
            case .ready:
                guard media.maximumDatagramSize >= Self.chunkPayload + 22 else {
                    self.fail("QUIC path cannot carry the media packet size"); return
                }
                print("[quic] negotiated datagram limit: \(media.maximumDatagramSize) bytes")
                self.receiveMedia(media)
                ready()
            case .failed(let error): self.fail(error.localizedDescription)
            default: break
            }
        }
        input.start(queue: queue)
        media.start(queue: queue)
    }

    func sendInput(_ payload: Data) {
        queue.async { [weak self] in
            guard let self, !self.closed, let input = self.input else { return }
            guard !payload.isEmpty, payload.count <= QUICFrame.maxLength else {
                self.fail("Input message exceeds the protocol limit"); return
            }
            input.send(content: QUICFrame.encode(payload), completion: .contentProcessed { [weak self] error in
                if let error { self?.fail(error.localizedDescription) }
            })
        }
    }

    func sendDatagram(_ payload: Data) {
        queue.async { [weak self] in
            guard let self, !self.closed, let media = self.media else { return }
            guard payload.count <= media.maximumDatagramSize else {
                self.fail("Oversized media datagram"); return
            }
            media.send(content: payload, completion: .contentProcessed { [weak self] error in
                if let error { self?.fail(error.localizedDescription) }
            })
        }
    }

    private func receiveMedia(_ connection: NWConnection) {
        connection.receiveMessage { [weak self] data, _, _, error in
            guard let self, !self.closed else { return }
            if let error { self.fail(error.localizedDescription); return }
            if let data, !data.isEmpty { self.onDatagram?(data) }
            self.receiveMedia(connection)
        }
    }

    func close() {
        queue.async { [self] in
            guard !closed else { return }
            closed = true
            control?.cancel(); input?.cancel(); media?.cancel(); group.cancel()
        }
    }
    private func fail(_ message: String) {
        guard !closed else { return }
        onFailure?(message)
        close()
    }
    static func error(_ message: String) -> NSError {
        NSError(domain: "ScrexQUIC", code: 1, userInfo: [NSLocalizedDescriptionKey: message])
    }
}

nonisolated private final class PeerCertificate: @unchecked Sendable {
    let lock = NSLock()
    var hash = Data()
}

nonisolated enum QUICFrame {
    static let maxLength = 65536
    static func encode(_ body: Data) -> Data {
        var result = withUnsafeBytes(of: UInt32(body.count).bigEndian) { Data($0) }
        result.append(body)
        return result
    }
    static func read(_ connection: NWConnection, completion: @escaping (Result<Data, Error>) -> Void) {
        exact(connection, length: 4) { result in
            switch result {
            case .failure(let error): completion(.failure(error))
            case .success(let header):
                let length = header.reduce(0) { $0 << 8 | Int($1) }
                guard (1...maxLength).contains(length) else {
                    completion(.failure(QUICSession.error("Invalid QUIC frame length"))); return
                }
                exact(connection, length: length, completion: completion)
            }
        }
    }
    private static func exact(_ connection: NWConnection, length: Int, prefix: Data = Data(),
                              completion: @escaping (Result<Data, Error>) -> Void) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: length - prefix.count) { bytes, _, ended, error in
            if let error { completion(.failure(error)); return }
            var data = prefix
            if let bytes { data.append(bytes) }
            if data.count == length { completion(.success(data)); return }
            guard !ended else { completion(.failure(QUICSession.error("QUIC stream ended"))); return }
            exact(connection, length: length, prefix: data, completion: completion)
        }
    }
}

import Foundation
import Network
import CryptoKit
import Security

enum PairingResult {
    case sessionEstablished(session: QUICSession)
    case pinRequired(completion: (String) -> Void)
    case rejected(reason: String)
    case error(String)
}

/// Pairing is carried over the first QUIC stream. Bootstrap TLS alone is not
/// trusted: an eight-digit CPace exchange confirms the actual TLS certificate,
/// client signing key and TLS exporter before either side stores a pairing.
final class PairingService {
    private let store: PairingStore
    private var session: QUICSession?
    private var completed = false
    private var timeout: DispatchWorkItem?
    var onResult: ((PairingResult) -> Void)?

    init(store: PairingStore = SystemPairingStore()) { self.store = store }

    func pair(host: String, port: UInt16, tileId: String, deviceId: Data, clientName: String) {
        do {
            let record = try store.load(tileId: tileId)
            let signing = try record.map { try Curve25519.Signing.PrivateKey(rawRepresentation: $0.signingKey) }
                ?? Curve25519.Signing.PrivateKey()
            let endpoint = NWEndpoint.hostPort(host: NWEndpoint.Host(host), port: NWEndpoint.Port(rawValue: port)!)
            let session = QUICSession(endpoint: endpoint, pin: record?.serverPin)
            self.session = session
            session.onFailure = { [weak self] message in self?.finish(.error(message)) }
            let task = DispatchWorkItem { [weak self] in self?.finish(.error("QUIC connection timed out")) }
            timeout = task
            session.queue.asyncAfter(deadline: .now() + 8, execute: task)
            session.start { [weak self] control in
                guard let self, !self.completed else { return }
                var hello = Data([0xe0])
                hello.append(deviceId)
                hello.append(signing.publicKey.rawRepresentation)
                var name = Data(clientName.utf8)
                while name.count > 63 { name.removeLast() }
                // Drop any incomplete UTF-8 suffix after truncation.
                while String(data: name, encoding: .utf8) == nil { name.removeLast() }
                if name.isEmpty { name = Data("iPad".utf8) }
                hello.append(name)
                control.send(content: QUICFrame.encode(hello), completion: .contentProcessed { [self] error in
                    if let error { self.finish(.error(error.localizedDescription)); return }
                    QUICFrame.read(control) { result in
                        guard !self.completed else { return }
                        do {
                            let challenge = try result.get()
                            guard challenge.count >= 82, challenge[0] == 0xe1 else {
                                throw QUICSession.error("Invalid pairing challenge")
                            }
                            self.timeout?.cancel()
                            let serverId = Data(challenge[2..<18])
                            let pin = Data(challenge[18..<50])
                            let sid = Data(challenge[50..<82])
                            guard pin == session.peerCertificateHash else {
                                throw QUICSession.error("Pairing certificate does not match TLS")
                            }
                            let binding = CPace.lv([Data("screx-quic-pair-v1".utf8), deviceId, serverId,
                                signing.publicKey.rawRepresentation, pin, try session.exporter()])
                            if challenge[1] == 1 {
                                guard let record, record.serverPin == pin else {
                                    throw QUICSession.error("Unknown server identity; remove this host and pair again")
                                }
                                let signature = try signing.signature(for:
                                    CPace.lv([Data("screx-quic-reconnect-v1".utf8), sid, binding]))
                                control.send(content: QUICFrame.encode(Data([0xe2]) + signature),
                                    completion: .contentProcessed { error in
                                        if let error { self.finish(.error(error.localizedDescription)); return }
                                        self.waitForReady(session, control: control)
                                    })
                            } else {
                                guard challenge.count == 114 else { throw QUICSession.error("Missing PAKE share") }
                                let yb = Data(challenge[82..<114])
                                let expiry = DispatchWorkItem { [weak self] in self?.finish(.error("Pairing PIN expired")) }
                                self.timeout = expiry
                                session.queue.asyncAfter(deadline: .now() + 120, execute: expiry)
                                DispatchQueue.main.async {
                                    self.onResult?(.pinRequired { [weak self] code in
                                        session.queue.async {
                                            guard let self, !self.completed else { return }
                                            do {
                                                guard code.utf8.count == 8, code.utf8.allSatisfy({ (48...57).contains($0) }) else {
                                                    throw QUICSession.error("Enter eight digits")
                                                }
                                                let exchange = try CPace.Exchange(password: Data(code.utf8), context: binding, sid: sid)
                                                let key = try exchange.finish(peer: yb, sid: sid, ya: exchange.share,
                                                    ada: deviceId, yb: yb, adb: serverId)
                                                var proof = Data([0xe3])
                                                proof.append(exchange.share)
                                                proof.append(CPace.confirmation(key: key, role: "client", binding: binding))
                                                control.send(content: QUICFrame.encode(proof), completion: .contentProcessed { error in
                                                    if let error { self.finish(.error(error.localizedDescription)); return }
                                                    QUICFrame.read(control) { result in
                                                        guard !self.completed else { return }
                                                        do {
                                                            let reply = try result.get()
                                                            guard reply.count == 33, reply[0] == 0xe4 else {
                                                                throw QUICSession.error("Pairing rejected")
                                                            }
                                                            try CPace.verify(key: key, role: "server", binding: binding,
                                                                tag: Data(reply.dropFirst()))
                                                            try self.store.save(
                                                                .init(signingKey: signing.rawRepresentation, serverPin: pin), tileId: tileId)
                                                            self.waitForReady(session, control: control)
                                                        } catch { self.finish(.error("Pairing failed: \(error.localizedDescription)")) }
                                                    }
                                                })
                                            } catch { self.finish(.error(error.localizedDescription)) }
                                        }
                                    })
                                }
                            }
                        } catch { self.finish(.error(error.localizedDescription)) }
                    }
                })
            }
        } catch { finish(.error(error.localizedDescription)) }
    }

    /// After authentication the daemon answers READY (0xe5), or BUSY (0xe6) when another
    /// device holds the session. Replacing this device's own stale session can take a few
    /// seconds, so the deadline covers that and opening the media flow.
    private func waitForReady(_ session: QUICSession, control: NWConnection) {
        timeout?.cancel()
        let task = DispatchWorkItem { [weak self] in self?.finish(.error("Session start timed out")) }
        timeout = task
        session.queue.asyncAfter(deadline: .now() + 8, execute: task)
        QUICFrame.read(control) { [weak self] result in
            guard let self, !self.completed else { return }
            do {
                switch try result.get() {
                case Data([0xe5]):
                    session.openMedia { self.finish(.sessionEstablished(session: session)) }
                case Data([0xe6]):
                    self.finish(.rejected(reason: "Host is busy with another device"))
                default:
                    throw QUICSession.error("Host rejected the session")
                }
            } catch { self.finish(.error(error.localizedDescription)) }
        }
    }
    func cancel() {
        // The session reference is set before starting the connection and is
        // not replaced. Its queue owns the pairing state once started.
        guard let session else { return }
        session.queue.async { [self] in
            completed = true
            timeout?.cancel()
            session.close()
        }
    }
    private func finish(_ result: PairingResult) {
        guard !completed else { return }
        completed = true
        timeout?.cancel()
        if case .sessionEstablished = result {
            session?.onFailure = nil
        } else { session?.close() }
        DispatchQueue.main.async { [self] in onResult?(result) }
    }
}

/// Storage is injectable so transport/authentication tests can run over SSH,
/// where macOS deliberately disallows interactive Keychain access.
protocol PairingStore {
    func load(tileId: String) throws -> KeychainHelper.Record?
    func save(_ record: KeychainHelper.Record, tileId: String) throws
}
struct SystemPairingStore: PairingStore {
    func load(tileId: String) throws -> KeychainHelper.Record? { try KeychainHelper.loadRecord(tileId: tileId) }
    func save(_ record: KeychainHelper.Record, tileId: String) throws { try KeychainHelper.storeRecord(record, tileId: tileId) }
}

/// Per-tile pairing records: the client signing key and the pinned server certificate.
enum KeychainHelper {
    struct Record: Codable {
        let signingKey: Data
        let serverPin: Data
    }
    private static let service = "com.screx.quic-pairing.v1"
    private static let idsKey = "screx_quic_paired_tile_ids_v1"
    static func loadRecord(tileId: String) throws -> Record? {
        var value: CFTypeRef?
        let status = SecItemCopyMatching([
            kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service,
            kSecAttrAccount as String: tileId, kSecReturnData as String: true, kSecMatchLimit as String: kSecMatchLimitOne,
        ] as CFDictionary, &value)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess, let data = value as? Data else { throw QUICSession.error("Keychain read failed: \(status)") }
        return try JSONDecoder().decode(Record.self, from: data)
    }
    static func storeRecord(_ record: Record, tileId: String) throws {
        let query = [kSecClass as String: kSecClassGenericPassword,
                     kSecAttrService as String: service, kSecAttrAccount as String: tileId] as CFDictionary
        let data = try JSONEncoder().encode(record)
        var status = SecItemUpdate(query, [kSecValueData as String: data] as CFDictionary)
        if status == errSecItemNotFound {
            status = SecItemAdd([
                kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service, kSecAttrAccount as String: tileId,
                kSecValueData as String: data, kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
            ] as CFDictionary, nil)
        }
        guard status == errSecSuccess else { throw QUICSession.error("Keychain write failed: \(status)") }
        var ids = UserDefaults.standard.stringArray(forKey: idsKey) ?? []
        if !ids.contains(tileId) { ids.append(tileId); UserDefaults.standard.set(ids, forKey: idsKey) }
    }
    static func deletePairingKeys(except keep: Set<String>) {
        let ids = UserDefaults.standard.stringArray(forKey: idsKey) ?? []
        for id in ids where !keep.contains(id) {
            SecItemDelete([kSecClass as String: kSecClassGenericPassword,
                           kSecAttrService as String: service, kSecAttrAccount as String: id] as CFDictionary)
        }
        UserDefaults.standard.set(ids.filter { keep.contains($0) }, forKey: idsKey)
    }
}

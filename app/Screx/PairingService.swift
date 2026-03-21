import Foundation
import Network
import CryptoKit

enum PairingResult {
    case sessionEstablished(sessionKey: SymmetricKey)
    case pinRequired(completion: (String) -> Void)
    case rejected(reason: String)
    case error(String)
}

final class PairingService {
    private let queue = DispatchQueue(label: "screx.pairing", qos: .userInitiated)
    private var connection: NWConnection?

    private static let magicPair   = Data("SCREX_PAIR".utf8)    // 10 bytes
    private static let magicHello  = Data("SCREX_HELLO".utf8)   // 11 bytes
    private static let magicPin    = Data("SCREX_PIN\0".utf8)   // 10 bytes
    private static let magicAnswer = Data("SCREX_ANSWER".utf8)  // 12 bytes
    private static let magicBusy   = Data("SCREX_BUSY\0\0".utf8) // 12 bytes
    private static let magicOK     = Data("SCREX_OK\0\0".utf8)  // 10 bytes
    private static let magicReject = Data("SCREX_REJECT".utf8)  // 12 bytes

    private static let deviceIdLen = 16
    private static let pubkeyLen   = 32
    private static let nonceLen    = 32
    private static let hmacLen     = 32
    private static let pinDigits   = 6
    private static let tagLen      = 16

    var onResult: ((PairingResult) -> Void)?

    func pair(host: String, port: UInt16) {
        let deviceId = Self.getOrCreateDeviceId()
        let pairingKey = KeychainHelper.loadPairingKey(for: host)

        let endpoint = NWEndpoint.hostPort(
            host: NWEndpoint.Host(host),
            port: NWEndpoint.Port(integerLiteral: port)
        )
        let conn = NWConnection(to: endpoint, using: .tcp)
        self.connection = conn

        conn.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            switch state {
            case .ready:
                if pairingKey != nil {
                    self.sendHello(conn: conn, host: host, deviceId: deviceId, pairingKey: pairingKey!)
                } else {
                    self.sendPairRequest(conn: conn, host: host, deviceId: deviceId)
                }
            case .failed(let error):
                self.emitResult(.error("TCP connect failed: \(error.localizedDescription)"))
            default:
                break
            }
        }

        conn.start(queue: queue)
    }

    func cancel() {
        connection?.cancel()
        connection = nil
    }

    // MARK: - New device pairing (SCREX_PAIR flow)

    private func sendPairRequest(conn: NWConnection, host: String, deviceId: Data) {
        let keyPair = ScrexCrypto.generateKeyPair()
        let pubKey = keyPair.publicKey.rawRepresentation

        var packet = Self.magicPair
        packet.append(deviceId)
        packet.append(pubKey)

        conn.send(content: packet, completion: .contentProcessed { [weak self] error in
            guard let self else { return }
            if let error {
                self.emitResult(.error("Send pair request failed: \(error.localizedDescription)"))
                return
            }
            self.waitForPinChallenge(conn: conn, host: host, deviceId: deviceId, keyPair: keyPair)
        })
    }

    private func waitForPinChallenge(
        conn: NWConnection,
        host: String,
        deviceId: Data,
        keyPair: Curve25519.KeyAgreement.PrivateKey
    ) {
        // Response is either:
        // SCREX_PIN(10) + server_pubkey(32) = 42 bytes (new pairing)
        // SCREX_OK(10) + server_pubkey(32) + hmac(32) = 74 bytes (already paired)
        conn.receive(minimumIncompleteLength: 10, maximumLength: 128) { [weak self] data, _, _, error in
            guard let self else { return }

            if let error {
                self.emitResult(.error("Receive failed: \(error.localizedDescription)"))
                return
            }
            guard let data, data.count >= 10 else {
                self.emitResult(.error("Empty response from daemon"))
                return
            }

            // Check for busy (daemon already has an active session)
            if data.count >= Self.magicBusy.count && data.prefix(Self.magicBusy.count) == Self.magicBusy {
                self.emitResult(.rejected(reason: "Daemon is busy with another client"))
                return
            }

            let magic = data.prefix(10)

            if magic == Self.magicPin {
                // New pairing: need PIN
                guard data.count >= 10 + Self.pubkeyLen else {
                    self.emitResult(.error("Invalid PIN challenge"))
                    return
                }
                let serverPubKey = data.subdata(in: 10..<(10 + Self.pubkeyLen))

                guard let ecdhSecret = ScrexCrypto.ecdh(privateKey: keyPair, publicKeyBytes: serverPubKey) else {
                    self.emitResult(.error("ECDH failed"))
                    return
                }

                self.emitResult(.pinRequired { [weak self] pin in
                    self?.sendPinAnswer(conn: conn, host: host, deviceId: deviceId, pin: pin, ecdhSecret: ecdhSecret)
                })
            } else if magic == Self.magicOK {
                // Already paired — daemon recognized us
                guard data.count >= 10 + Self.pubkeyLen + Self.hmacLen else {
                    self.emitResult(.error("Invalid OK response"))
                    return
                }
                let serverPubKey = data.subdata(in: 10..<(10 + Self.pubkeyLen))
                let serverHmac = data.subdata(in: (10 + Self.pubkeyLen)..<(10 + Self.pubkeyLen + Self.hmacLen))

                guard let ecdhSecret = ScrexCrypto.ecdh(privateKey: keyPair, publicKeyBytes: serverPubKey) else {
                    self.emitResult(.error("ECDH failed"))
                    return
                }

                guard let pairingKey = KeychainHelper.loadPairingKey(for: host) else {
                    self.emitResult(.error("Pairing key missing"))
                    return
                }

                var ikm = pairingKey
                ikm.append(ecdhSecret)
                let sessionKeyData = ScrexCrypto.hkdfSHA256Bytes(
                    ikm: ikm,
                    salt: Data("screx-reconnect-salt".utf8),
                    info: Data("screx-session".utf8)
                )
                let sessionKey = SymmetricKey(data: sessionKeyData)

                let expectedHmac = ScrexCrypto.hmacSHA256(key: sessionKeyData, data: Data("server-verify".utf8))
                guard expectedHmac == serverHmac else {
                    self.emitResult(.rejected(reason: "Server verification failed"))
                    return
                }

                conn.cancel()
                self.emitResult(.sessionEstablished(sessionKey: sessionKey))
            } else {
                self.emitResult(.error("Unexpected response from daemon"))
            }
        }
    }

    private func sendPinAnswer(conn: NWConnection, host: String, deviceId: Data, pin: String, ecdhSecret: Data) {
        let pinKey = ScrexCrypto.hkdfSHA256Bytes(
            ikm: ecdhSecret,
            salt: Data("screx-pin-exchange".utf8),
            info: Data("pin-encrypt".utf8)
        )
        let pinKeySymmetric = SymmetricKey(data: pinKey)

        // Generate random 12-byte nonce
        var nonceBytes = Data(count: 12)
        _ = nonceBytes.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, 12, $0.baseAddress!) }

        guard let encrypted = ScrexCrypto.encrypt(
            key: pinKeySymmetric,
            nonce: nonceBytes,
            plaintext: Data(pin.utf8),
            aad: Data("screx-pin-verify".utf8)
        ) else {
            emitResult(.error("PIN encryption failed"))
            return
        }

        var packet = Self.magicAnswer
        packet.append(nonceBytes) // 12 bytes
        packet.append(encrypted)  // PIN_DIGITS + TAG_LEN bytes

        conn.send(content: packet, completion: .contentProcessed { [weak self] error in
            guard let self else { return }
            if let error {
                self.emitResult(.error("Send PIN answer failed: \(error.localizedDescription)"))
                return
            }
            self.waitForPinResult(conn: conn, host: host, deviceId: deviceId, pin: pin, ecdhSecret: ecdhSecret)
        })
    }

    private func waitForPinResult(conn: NWConnection, host: String, deviceId: Data, pin: String, ecdhSecret: Data) {
        conn.receive(minimumIncompleteLength: 10, maximumLength: 128) { [weak self] data, _, _, error in
            guard let self else { return }

            if let error {
                self.emitResult(.error("PIN result receive failed: \(error.localizedDescription)"))
                return
            }
            guard let data, data.count >= 10 else {
                self.emitResult(.error("Empty PIN result"))
                return
            }

            let magic = data.prefix(10)

            if data.count >= Self.magicReject.count && data.prefix(Self.magicReject.count) == Self.magicReject {
                self.emitResult(.rejected(reason: "Wrong PIN"))
                return
            }

            guard magic == Self.magicOK else {
                self.emitResult(.error("Unexpected PIN result"))
                return
            }

            // SCREX_OK(10) + session_salt(32) + hmac(32) = 74
            guard data.count >= 10 + Self.nonceLen + Self.hmacLen else {
                self.emitResult(.error("Invalid OK response"))
                return
            }

            let sessionSalt = data.subdata(in: 10..<(10 + Self.nonceLen))
            let serverHmac = data.subdata(in: (10 + Self.nonceLen)..<(10 + Self.nonceLen + Self.hmacLen))

            // Derive pairing key
            var ikm = ecdhSecret
            ikm.append(Data(pin.utf8))
            let pairingKey = ScrexCrypto.hkdfSHA256Bytes(
                ikm: ikm,
                salt: Data("screx-pairing-salt".utf8),
                info: Data("screx-pairing".utf8)
            )

            // Store pairing key
            KeychainHelper.storePairingKey(pairingKey, for: host)

            // Derive session key
            let sessionKeyData = ScrexCrypto.hkdfSHA256Bytes(
                ikm: pairingKey,
                salt: sessionSalt,
                info: Data("screx-session".utf8)
            )
            let sessionKey = SymmetricKey(data: sessionKeyData)

            // Verify server HMAC
            let expectedHmac = ScrexCrypto.hmacSHA256(key: sessionKeyData, data: Data("server-verify".utf8))
            guard expectedHmac == serverHmac else {
                self.emitResult(.rejected(reason: "Server verification failed after pairing"))
                return
            }

            conn.cancel()
            self.emitResult(.sessionEstablished(sessionKey: sessionKey))
        }
    }

    // MARK: - Reconnection (SCREX_HELLO flow)

    private func sendHello(conn: NWConnection, host: String, deviceId: Data, pairingKey: Data) {
        var clientNonce = Data(count: Self.nonceLen)
        _ = clientNonce.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, Self.nonceLen, $0.baseAddress!) }

        var packet = Self.magicHello
        packet.append(deviceId)
        packet.append(clientNonce)

        conn.send(content: packet, completion: .contentProcessed { [weak self] error in
            guard let self else { return }
            if let error {
                self.emitResult(.error("Send hello failed: \(error.localizedDescription)"))
                return
            }
            self.waitForHelloResponse(conn: conn, host: host, pairingKey: pairingKey, clientNonce: clientNonce)
        })
    }

    private func waitForHelloResponse(conn: NWConnection, host: String, pairingKey: Data, clientNonce: Data) {
        conn.receive(minimumIncompleteLength: 10, maximumLength: 128) { [weak self] data, _, _, error in
            guard let self else { return }

            if let error {
                self.emitResult(.error("Hello response receive failed: \(error.localizedDescription)"))
                return
            }
            guard let data, data.count >= 10 else {
                self.emitResult(.error("Empty hello response"))
                return
            }

            if data.count >= Self.magicBusy.count && data.prefix(Self.magicBusy.count) == Self.magicBusy {
                self.emitResult(.rejected(reason: "Daemon is busy with another client"))
                return
            }

            if data.count >= Self.magicReject.count && data.prefix(Self.magicReject.count) == Self.magicReject {
                KeychainHelper.deletePairingKey(for: host)
                self.emitResult(.error("Not recognized by daemon — please pair again"))
                return
            }

            guard data.prefix(10) == Self.magicOK else {
                self.emitResult(.error("Unexpected hello response"))
                return
            }

            // SCREX_OK(10) + server_nonce(32) + hmac(32) = 74
            guard data.count >= 10 + Self.nonceLen + Self.hmacLen else {
                self.emitResult(.error("Invalid OK response"))
                return
            }

            let serverNonce = data.subdata(in: 10..<(10 + Self.nonceLen))
            let serverHmac = data.subdata(in: (10 + Self.nonceLen)..<(10 + Self.nonceLen + Self.hmacLen))

            var salt = clientNonce
            salt.append(serverNonce)
            let sessionKeyData = ScrexCrypto.hkdfSHA256Bytes(
                ikm: pairingKey,
                salt: salt,
                info: Data("screx-session".utf8)
            )
            let sessionKey = SymmetricKey(data: sessionKeyData)

            let expectedHmac = ScrexCrypto.hmacSHA256(key: sessionKeyData, data: Data("server-verify".utf8))
            guard expectedHmac == serverHmac else {
                self.emitResult(.rejected(reason: "Server verification failed"))
                return
            }

            conn.cancel()
            self.emitResult(.sessionEstablished(sessionKey: sessionKey))
        }
    }

    // MARK: - Helpers

    private func emitResult(_ result: PairingResult) {
        DispatchQueue.main.async { [weak self] in
            self?.onResult?(result)
        }
    }

    /// Stable device ID persisted in UserDefaults.
    private static func getOrCreateDeviceId() -> Data {
        let key = "screx_device_id"
        if let existing = UserDefaults.standard.data(forKey: key), existing.count == deviceIdLen {
            return existing
        }
        var id = Data(count: deviceIdLen)
        _ = id.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, deviceIdLen, $0.baseAddress!) }
        UserDefaults.standard.set(id, forKey: key)
        return id
    }
}

// MARK: - Keychain helpers

enum KeychainHelper {
    private static let servicePrefix = "com.screx.pairing."

    static func storePairingKey(_ key: Data, for host: String) {
        let service = servicePrefix + host
        // Delete existing
        let deleteQuery: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
        ]
        SecItemDelete(deleteQuery as CFDictionary)

        let addQuery: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecValueData as String: key,
            kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlock,
        ]
        let status = SecItemAdd(addQuery as CFDictionary, nil)
        if status != errSecSuccess {
            print("[keychain] store failed: \(status)")
        }
    }

    static func loadPairingKey(for host: String) -> Data? {
        let service = servicePrefix + host
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var result: AnyObject?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        guard status == errSecSuccess else { return nil }
        return result as? Data
    }

    static func deletePairingKey(for host: String) {
        let service = servicePrefix + host
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
        ]
        SecItemDelete(query as CFDictionary)
    }
}

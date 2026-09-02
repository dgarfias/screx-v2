import Foundation
import Network
import CryptoKit

enum PairingResult {
    case sessionEstablished(sessionKey: SymmetricKey, connection: NWConnection)
    case pinRequired(completion: (String) -> Void)
    case rejected(reason: String)
    case error(String)
}

final class PairingService {
    private let queue = DispatchQueue(label: "screx.pairing", qos: .userInitiated)
    private var connection: NWConnection?
    private let debugId = String(UUID().uuidString.prefix(6))
    private var didComplete = false
    private var connectTimeoutWorkItem: DispatchWorkItem?

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
    private static let connectTimeout: TimeInterval = 8

    /// TCP parameters for the pairing/control connection. Disables Nagle's
    /// algorithm (`noDelay`) so small input packets (touch/mouse/key) are sent
    /// immediately instead of being held behind the receiver's delayed-ACK
    /// timer — the daemon already sets `TCP_NODELAY` on its side, and this
    /// connection later becomes the control/input channel.
    private static func tcpParameters() -> NWParameters {
        let params = NWParameters.tcp
        if let tcp = params.defaultProtocolStack.transportProtocol as? NWProtocolTCP.Options {
            tcp.noDelay = true
        }
        return params
    }

    var onResult: ((PairingResult) -> Void)?

    private func log(_ message: String) {
        print("[pairing \(debugId)] \(message)")
    }

    func pair(host: String, port: UInt16) {
        let deviceId = Self.getOrCreateDeviceId()
        let candidateKeys = KeychainHelper.loadCandidateKeys(host: host, port: port)
        log("starting pair(host=\(host), port=\(port)) deviceId=\(deviceId.map { String(format: "%02x", $0) }.joined()) candidateKeys=\(candidateKeys.count)")

        let endpoint = NWEndpoint.hostPort(
            host: NWEndpoint.Host(host),
            port: NWEndpoint.Port(integerLiteral: port)
        )
        let conn = NWConnection(to: endpoint, using: Self.tcpParameters())
        self.connection = conn

        conn.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            self.log("tcp state -> \(state)")
            switch state {
            case .ready:
                self.cancelConnectTimeout()
                if let firstKey = candidateKeys.first {
                    self.log("tcp ready, trying reconnect HELLO flow")
                    self.sendHello(conn: conn, host: host, port: port, deviceId: deviceId, candidateKeys: candidateKeys)
                } else {
                    self.log("tcp ready, using first-time PAIR flow")
                    self.sendPairRequest(conn: conn, host: host, port: port, deviceId: deviceId)
                }
            case .waiting(let error):
                self.cancelConnectTimeout()
                self.connection?.cancel()
                self.connection = nil
                self.emitResult(.error("TCP connect failed: \(error.localizedDescription)"))
            case .failed(let error):
                self.cancelConnectTimeout()
                self.connection = nil
                self.emitResult(.error("TCP connect failed: \(error.localizedDescription)"))
            case .cancelled:
                self.log("tcp state -> cancelled")
            default:
                break
            }
        }

        let timeout = DispatchWorkItem { [weak self, weak conn] in
            guard let self, let conn else { return }
            guard self.connection === conn, !self.didComplete else { return }
            self.log("TCP connect timed out after \(Self.connectTimeout)s")
            self.connectTimeoutWorkItem = nil
            self.connection = nil
            conn.cancel()
            self.emitResult(.error("TCP connect timed out after \(Int(Self.connectTimeout)) seconds"))
        }
        connectTimeoutWorkItem = timeout
        queue.asyncAfter(deadline: .now() + Self.connectTimeout, execute: timeout)
        conn.start(queue: queue)
    }

    func cancel() {
        log("cancel()")
        cancelConnectTimeout()
        didComplete = true
        connection?.cancel()
        connection = nil
    }

    private func cancelConnectTimeout() {
        connectTimeoutWorkItem?.cancel()
        connectTimeoutWorkItem = nil
    }

    // MARK: - New device pairing (SCREX_PAIR flow)

    private func sendPairRequest(conn: NWConnection, host: String, port: UInt16, deviceId: Data) {
        let keyPair = ScrexCrypto.generateKeyPair()
        let pubKey = keyPair.publicKey.rawRepresentation
        log("sending PAIR request: deviceId=\(deviceId.map { String(format: "%02x", $0) }.joined()) pubKeyLen=\(pubKey.count)")

        var packet = Self.magicPair
        packet.append(deviceId)
        packet.append(pubKey)

        conn.send(content: packet, completion: .contentProcessed { [weak self] error in
            guard let self else { return }
            if let error {
                self.emitResult(.error("Send pair request failed: \(error.localizedDescription)"))
                return
            }
            self.log("PAIR request sent (\(packet.count) bytes), waiting for daemon response")
            self.waitForPinChallenge(conn: conn, host: host, port: port, deviceId: deviceId, keyPair: keyPair)
        })
    }

    private func waitForPinChallenge(
        conn: NWConnection,
        host: String,
        port: UInt16,
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
            self.log("received pair challenge/ok response: \(data.count) bytes prefix=\(String(decoding: data.prefix(min(12, data.count)), as: UTF8.self))")

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
                self.log("daemon requested PIN entry, serverPubKeyLen=\(serverPubKey.count)")

                guard let ecdhSecret = ScrexCrypto.ecdh(privateKey: keyPair, publicKeyBytes: serverPubKey) else {
                    self.emitResult(.error("ECDH failed"))
                    return
                }

                self.emitResult(.pinRequired { [weak self] pin in
                    self?.sendPinAnswer(conn: conn, host: host, port: port, deviceId: deviceId, pin: pin, ecdhSecret: ecdhSecret)
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

                guard let pairingKey = KeychainHelper.loadPairingKey(forHost: host, port: port) else {
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
                self.log("received reconnect OK, derived session key and validating server HMAC")

                let expectedHmac = ScrexCrypto.hmacSHA256(key: sessionKeyData, data: Data("server-verify".utf8))
                guard expectedHmac == serverHmac else {
                    self.emitResult(.rejected(reason: "Server verification failed"))
                    return
                }

                self.log("reconnect HELLO flow complete, session established")
                conn.stateUpdateHandler = nil
                self.connection = nil
                self.emitResult(.sessionEstablished(sessionKey: sessionKey, connection: conn))
            } else {
                self.emitResult(.error("Unexpected response from daemon"))
            }
        }
    }

    private func sendPinAnswer(conn: NWConnection, host: String, port: UInt16, deviceId: Data, pin: String, ecdhSecret: Data) {
        log("sending PIN answer for host=\(host) deviceId=\(deviceId.map { String(format: "%02x", $0) }.joined())")
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
            self.log("PIN answer sent, waiting for final OK")
            self.waitForPinResult(conn: conn, host: host, port: port, deviceId: deviceId, pin: pin, ecdhSecret: ecdhSecret)
        })
    }

    private func waitForPinResult(conn: NWConnection, host: String, port: UInt16, deviceId: Data, pin: String, ecdhSecret: Data) {
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
            self.log("received PIN result: \(data.count) bytes prefix=\(String(decoding: data.prefix(min(12, data.count)), as: UTF8.self))")

            let magic = data.prefix(10)

            if data.count >= Self.magicReject.count && data.prefix(Self.magicReject.count) == Self.magicReject {
                self.emitResult(.rejected(reason: "Wrong PIN"))
                return
            }

            guard magic == Self.magicOK else {
                self.emitResult(.error("Unexpected PIN result"))
                return
            }

            // New daemons: SCREX_OK(10) + daemon_device_id(16) + session_salt(32) + hmac(32) = 90
            // Old daemons: SCREX_OK(10) + session_salt(32) + hmac(32) = 74
            let daemonDeviceId: Data?
            let sessionSalt: Data
            let serverHmac: Data
            if data.count >= 10 + Self.deviceIdLen + Self.nonceLen + Self.hmacLen {
                daemonDeviceId = data.subdata(in: 10..<(10 + Self.deviceIdLen))
                sessionSalt = data.subdata(in: (10 + Self.deviceIdLen)..<(10 + Self.deviceIdLen + Self.nonceLen))
                serverHmac = data.subdata(in: (10 + Self.deviceIdLen + Self.nonceLen)..<(10 + Self.deviceIdLen + Self.nonceLen + Self.hmacLen))
            } else if data.count >= 10 + Self.nonceLen + Self.hmacLen {
                daemonDeviceId = nil
                sessionSalt = data.subdata(in: 10..<(10 + Self.nonceLen))
                serverHmac = data.subdata(in: (10 + Self.nonceLen)..<(10 + Self.nonceLen + Self.hmacLen))
            } else {
                self.emitResult(.error("Invalid OK response"))
                return
            }

            // Derive pairing key
            var ikm = ecdhSecret
            ikm.append(Data(pin.utf8))
            let pairingKey = ScrexCrypto.hkdfSHA256Bytes(
                ikm: ikm,
                salt: Data("screx-pairing-salt".utf8),
                info: Data("screx-pairing".utf8)
            )

            // Store pairing key by daemon identity (new daemons) or host:port (legacy fallback).
            if let daemonDeviceId = daemonDeviceId {
                KeychainHelper.storePairingKey(pairingKey, daemonId: daemonDeviceId)
            } else {
                KeychainHelper.storePairingKey(pairingKey, forHost: host, port: port)
            }

            // Derive session key
            let sessionKeyData = ScrexCrypto.hkdfSHA256Bytes(
                ikm: pairingKey,
                salt: sessionSalt,
                info: Data("screx-session".utf8)
            )
            let sessionKey = SymmetricKey(data: sessionKeyData)
            self.log("PIN flow complete, derived session key and validating server HMAC")

            // Verify server HMAC
            let expectedHmac = ScrexCrypto.hmacSHA256(key: sessionKeyData, data: Data("server-verify".utf8))
            guard expectedHmac == serverHmac else {
                self.emitResult(.rejected(reason: "Server verification failed after pairing"))
                return
            }

            self.log("pairing flow complete, session established")
            conn.stateUpdateHandler = nil
            self.connection = nil
            self.emitResult(.sessionEstablished(sessionKey: sessionKey, connection: conn))
        }
    }

    // MARK: - Reconnection (SCREX_HELLO flow)

    private func sendHello(conn: NWConnection, host: String, port: UInt16, deviceId: Data, candidateKeys: [Data]) {
        var clientNonce = Data(count: Self.nonceLen)
        _ = clientNonce.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, Self.nonceLen, $0.baseAddress!) }
        log("sending HELLO: host=\(host) deviceId=\(deviceId.map { String(format: "%02x", $0) }.joined()) candidateKeys=\(candidateKeys.count)")

        var packet = Self.magicHello
        packet.append(deviceId)
        packet.append(clientNonce)

        conn.send(content: packet, completion: .contentProcessed { [weak self] error in
            guard let self else { return }
            if let error {
                self.emitResult(.error("Send hello failed: \(error.localizedDescription)"))
                return
            }
            self.log("HELLO sent (\(packet.count) bytes), waiting for hello response")
            self.waitForHelloResponse(conn: conn, host: host, port: port, candidateKeys: candidateKeys, clientNonce: clientNonce)
        })
    }

    private func waitForHelloResponse(conn: NWConnection, host: String, port: UInt16, candidateKeys: [Data], clientNonce: Data) {
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
            self.log("received hello response: \(data.count) bytes prefix=\(String(decoding: data.prefix(min(12, data.count)), as: UTF8.self))")

            if data.count >= Self.magicBusy.count && data.prefix(Self.magicBusy.count) == Self.magicBusy {
                self.emitResult(.rejected(reason: "Daemon is busy with another client"))
                return
            }

            if data.count >= Self.magicReject.count && data.prefix(Self.magicReject.count) == Self.magicReject {
                self.log("daemon rejected HELLO — stale key, switching to PAIR flow")
                KeychainHelper.deletePairingKey(forHost: host, port: port)
                self.sendPairRequest(conn: conn, host: host, port: port, deviceId: Self.getOrCreateDeviceId())
                return
            }

            guard data.prefix(10) == Self.magicOK else {
                self.emitResult(.error("Unexpected hello response"))
                return
            }

            // New daemons: SCREX_OK(10) + daemon_device_id(16) + server_nonce(32) + hmac(32) = 90
            // Old daemons: SCREX_OK(10) + server_nonce(32) + hmac(32) = 74
            let daemonDeviceId: Data?
            let serverNonce: Data
            let serverHmac: Data
            if data.count >= 10 + Self.deviceIdLen + Self.nonceLen + Self.hmacLen {
                daemonDeviceId = data.subdata(in: 10..<(10 + Self.deviceIdLen))
                serverNonce = data.subdata(in: (10 + Self.deviceIdLen)..<(10 + Self.deviceIdLen + Self.nonceLen))
                serverHmac = data.subdata(in: (10 + Self.deviceIdLen + Self.nonceLen)..<(10 + Self.deviceIdLen + Self.nonceLen + Self.hmacLen))
            } else if data.count >= 10 + Self.nonceLen + Self.hmacLen {
                daemonDeviceId = nil
                serverNonce = data.subdata(in: 10..<(10 + Self.nonceLen))
                serverHmac = data.subdata(in: (10 + Self.nonceLen)..<(10 + Self.nonceLen + Self.hmacLen))
            } else {
                self.emitResult(.error("Invalid OK response"))
                return
            }

            var salt = clientNonce
            salt.append(serverNonce)

            // Try every candidate key. This makes reconnect work even when the
            // daemon's IP address changes, because the correct key is found by
            // verifying the HMAC rather than by host:port lookup.
            guard let (matchedKey, sessionKeyData) = candidateKeys.lazy.compactMap({ key -> (Data, Data)? in
                let derived = ScrexCrypto.hkdfSHA256Bytes(
                    ikm: key,
                    salt: salt,
                    info: Data("screx-session".utf8)
                )
                let expectedHmac = ScrexCrypto.hmacSHA256(key: derived, data: Data("server-verify".utf8))
                if expectedHmac == serverHmac {
                    return (key, derived)
                }
                return nil
            }).first else {
                self.log("no stored key verified daemon HMAC — re-pairing")
                KeychainHelper.deletePairingKey(forHost: host, port: port)
                self.sendPairRequest(conn: conn, host: host, port: port, deviceId: Self.getOrCreateDeviceId())
                return
            }

            self.log("HELLO response validated, session established")
            let sessionKey = SymmetricKey(data: sessionKeyData)

            // Remember this daemon by identity (new daemons) or host:port (legacy).
            if let daemonDeviceId = daemonDeviceId {
                KeychainHelper.storePairingKey(matchedKey, daemonId: daemonDeviceId)
            }

            conn.stateUpdateHandler = nil
            self.connection = nil
            self.emitResult(.sessionEstablished(sessionKey: sessionKey, connection: conn))
        }
    }

    // MARK: - Helpers

    private func emitResult(_ result: PairingResult) {
        switch result {
        case .sessionEstablished, .rejected, .error:
            guard !didComplete else {
                log("emitResult ignored after completion")
                return
            }
            didComplete = true
        case .pinRequired:
            break
        }

        switch result {
        case .sessionEstablished:
            log("emitResult(.sessionEstablished)")
        case .pinRequired:
            log("emitResult(.pinRequired)")
        case .rejected(let reason):
            log("emitResult(.rejected: \(reason))")
        case .error(let message):
            log("emitResult(.error: \(message))")
        }
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
    private static let knownDaemonIdsKey = "screx_known_daemon_ids"
    private static func serviceName(host: String, port: UInt16) -> String {
        servicePrefix + formatEndpointInput(host: host, port: port)
    }
    private static func daemonServiceName(daemonId: Data) -> String {
        servicePrefix + "daemon." + daemonId.map { String(format: "%02x", $0) }.joined()
    }

    /// Legacy host-based storage. Kept for migration; new keys are stored by daemon ID.
    static func storePairingKey(_ key: Data, forHost host: String, port: UInt16) {
        let service = serviceName(host: host, port: port)
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
            print("[keychain] legacy store failed: \(status)")
        }
    }

    /// Preferred storage by daemon identity so IP changes do not break pairing.
    static func storePairingKey(_ key: Data, daemonId: Data) {
        let service = daemonServiceName(daemonId: daemonId)
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
        if status == errSecSuccess {
            registerKnownDaemonId(daemonId)
        } else {
            print("[keychain] daemon-id store failed: \(status)")
        }
    }

    static func loadPairingKey(forHost host: String, port: UInt16) -> Data? {
        let services = [serviceName(host: host, port: port), servicePrefix + host]
        for service in services {
            let query: [String: Any] = [
                kSecClass as String: kSecClassGenericPassword,
                kSecAttrService as String: service,
                kSecReturnData as String: true,
                kSecMatchLimit as String: kSecMatchLimitOne,
            ]
            var result: AnyObject?
            let status = SecItemCopyMatching(query as CFDictionary, &result)
            if status == errSecSuccess {
                return result as? Data
            }
        }
        return nil
    }

    static func loadPairingKey(daemonId: Data) -> Data? {
        let service = daemonServiceName(daemonId: daemonId)
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var result: AnyObject?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecSuccess {
            return result as? Data
        }
        return nil
    }

    /// Returns every stored key that might unlock a HELLO response from this daemon.
    /// Includes legacy host-based keys and all daemon-identity keys.
    static func loadCandidateKeys(host: String, port: UInt16) -> [Data] {
        var keys: [Data] = []
        if let legacy = loadPairingKey(forHost: host, port: port) {
            keys.append(legacy)
        }
        for id in knownDaemonIds() {
            if let idData = hexDecode(id), let key = loadPairingKey(daemonId: idData) {
                keys.append(key)
            }
        }
        return keys
    }

    static func deletePairingKey(forHost host: String, port: UInt16) {
        for service in [serviceName(host: host, port: port), servicePrefix + host] {
            let query: [String: Any] = [
                kSecClass as String: kSecClassGenericPassword,
                kSecAttrService as String: service,
            ]
            SecItemDelete(query as CFDictionary)
        }
    }

    static func deletePairingKey(daemonId: Data) {
        let service = daemonServiceName(daemonId: daemonId)
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
        ]
        SecItemDelete(query as CFDictionary)
        unregisterKnownDaemonId(daemonId)
    }

    // MARK: - Known daemon IDs (stored in UserDefaults; keys themselves stay in Keychain)

    private static func knownDaemonIds() -> [String] {
        UserDefaults.standard.stringArray(forKey: knownDaemonIdsKey) ?? []
    }

    private static func registerKnownDaemonId(_ daemonId: Data) {
        let hex = daemonId.map { String(format: "%02x", $0) }.joined()
        var ids = knownDaemonIds()
        if !ids.contains(hex) {
            ids.append(hex)
            UserDefaults.standard.set(ids, forKey: knownDaemonIdsKey)
        }
    }

    private static func unregisterKnownDaemonId(_ daemonId: Data) {
        let hex = daemonId.map { String(format: "%02x", $0) }.joined()
        var ids = knownDaemonIds()
        ids.removeAll { $0 == hex }
        UserDefaults.standard.set(ids, forKey: knownDaemonIdsKey)
    }

    private static func hexDecode(_ hex: String) -> Data? {
        guard hex.count % 2 == 0 else { return nil }
        var data = Data(capacity: hex.count / 2)
        for i in stride(from: 0, to: hex.count, by: 2) {
            let start = hex.index(hex.startIndex, offsetBy: i)
            let end = hex.index(start, offsetBy: 2)
            guard let byte = UInt8(hex[start..<end], radix: 16) else { return nil }
            data.append(byte)
        }
        return data
    }
}

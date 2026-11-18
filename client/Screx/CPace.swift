import Foundation
import CryptoKit

/// CPACE-X25519-SHA512 (draft-irtf-cfrg-cpace-21), initiator/responder transcript.
/// Only pairing authentication uses this. Network.framework provides transport TLS.
nonisolated enum CPace {
    static func lv(_ fields: [Data]) -> Data {
        var result = Data()
        for field in fields {
            var count = field.count
            repeat {
                result.append(UInt8(count & 127) | (count >= 128 ? 128 : 0))
                count >>= 7
            } while count > 0
            result.append(field)
        }
        return result
    }

    static func generator(password: Data, context: Data, sid: Data) -> Data {
        let domain = Data("CPace255".utf8)
        let padding = Data(repeating: 0, count: max(0, 128 - 1 - lv([domain, password]).count))
        let hash = Data(SHA512.hash(data: lv([domain, password, padding, context, sid])))
        var point = Data(count: 32)
        point.withUnsafeMutableBytes { out in
            hash.withUnsafeBytes { input in
                screx_cpace_map(out.bindMemory(to: UInt8.self).baseAddress!,
                               input.bindMemory(to: UInt8.self).baseAddress!)
            }
        }
        return point
    }

    struct Exchange {
        let secret: Curve25519.KeyAgreement.PrivateKey
        let share: Data
        init(password: Data, context: Data, sid: Data, scalar: Data? = nil) throws {
            if let scalar {
                secret = try Curve25519.KeyAgreement.PrivateKey(rawRepresentation: scalar)
            } else {
                secret = Curve25519.KeyAgreement.PrivateKey()
            }
            let base = try Curve25519.KeyAgreement.PublicKey(rawRepresentation:
                CPace.generator(password: password, context: context, sid: sid))
            share = try secret.sharedSecretFromKeyAgreement(with: base).withUnsafeBytes { Data($0) }
        }
        func finish(peer: Data, sid: Data, ya: Data, ada: Data, yb: Data, adb: Data) throws -> Data {
            let publicKey = try Curve25519.KeyAgreement.PublicKey(rawRepresentation: peer)
            // CryptoKit rejects non-contributory (low-order) shared secrets.
            let key = try secret.sharedSecretFromKeyAgreement(with: publicKey).withUnsafeBytes { Data($0) }
            guard key.contains(where: { $0 != 0 }) else { throw PairingError.invalidPeer }
            var input = CPace.lv([Data("CPace255_ISK".utf8), sid, key])
            input.append(CPace.lv([ya, ada, yb, adb]))
            return Data(SHA512.hash(data: input))
        }
    }
    enum PairingError: Error { case invalidPeer, confirmationFailed }
    static func confirmation(key: Data, role: String, binding: Data) -> Data {
        Data(HMAC<SHA256>.authenticationCode(
            for: lv([Data("screx-quic-pair-v1".utf8), Data(role.utf8), binding]),
            using: SymmetricKey(data: key)))
    }
    static func verify(key: Data, role: String, binding: Data, tag: Data) throws {
        guard HMAC<SHA256>.isValidAuthenticationCode(tag,
            authenticating: lv([Data("screx-quic-pair-v1".utf8), Data(role.utf8), binding]),
            using: SymmetricKey(data: key)) else { throw PairingError.confirmationFailed }
    }
}

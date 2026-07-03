import Foundation
import CryptoKit

struct ScrexCrypto {
    static let tagLen = 16

    // MARK: - AES-256-GCM

    static func encrypt(
        key: SymmetricKey,
        nonce nonceBytes: Data,
        plaintext: Data,
        aad: Data = Data()
    ) -> Data? {
        guard nonceBytes.count == 12 else { return nil }
        guard let nonce = try? AES.GCM.Nonce(data: nonceBytes) else { return nil }
        guard let sealed = try? AES.GCM.seal(plaintext, using: key, nonce: nonce, authenticating: aad) else { return nil }
        // Return ciphertext + tag (no nonce prefix — caller manages nonce)
        var result = Data()
        result.reserveCapacity(sealed.ciphertext.count + tagLen)
        result.append(sealed.ciphertext)
        result.append(sealed.tag)
        return result
    }

    static func decrypt(
        key: SymmetricKey,
        nonce nonceBytes: Data,
        ciphertextAndTag: Data,
        aad: Data = Data()
    ) -> Data? {
        guard nonceBytes.count == 12, ciphertextAndTag.count >= tagLen else { return nil }
        guard let nonce = try? AES.GCM.Nonce(data: nonceBytes) else { return nil }
        let ciphertext = ciphertextAndTag.prefix(ciphertextAndTag.count - tagLen)
        let tag = ciphertextAndTag.suffix(tagLen)
        guard let box_ = try? AES.GCM.SealedBox(nonce: nonce, ciphertext: ciphertext, tag: tag) else { return nil }
        return try? AES.GCM.open(box_, using: key, authenticating: aad)
    }

    // MARK: - X25519

    static func generateKeyPair() -> Curve25519.KeyAgreement.PrivateKey {
        Curve25519.KeyAgreement.PrivateKey()
    }

    /// Perform X25519 ECDH and return the raw 32-byte shared secret.
    /// Both ring (Rust) and CryptoKit produce identical raw X25519 output.
    static func ecdh(
        privateKey: Curve25519.KeyAgreement.PrivateKey,
        publicKeyBytes: Data
    ) -> Data? {
        guard let pubKey = try? Curve25519.KeyAgreement.PublicKey(rawRepresentation: publicKeyBytes) else { return nil }
        guard let shared = try? privateKey.sharedSecretFromKeyAgreement(with: pubKey) else { return nil }
        // SharedSecret's raw bytes are the X25519 output; use withUnsafeBytes to extract them
        return shared.withUnsafeBytes { Data($0) }
    }

    // MARK: - HKDF

    static func hkdfSHA256(ikm: Data, salt: Data, info: Data) -> SymmetricKey {
        let ikmKey = SymmetricKey(data: ikm)
        return HKDF<SHA256>.deriveKey(
            inputKeyMaterial: ikmKey,
            salt: salt,
            info: info,
            outputByteCount: 32
        )
    }

    static func hkdfSHA256Bytes(ikm: Data, salt: Data, info: Data) -> Data {
        let key = hkdfSHA256(ikm: ikm, salt: salt, info: info)
        return key.withUnsafeBytes { Data($0) }
    }

    // MARK: - HMAC

    static func hmacSHA256(key: Data, data: Data) -> Data {
        let hmacKey = SymmetricKey(data: key)
        let auth = HMAC<SHA256>.authenticationCode(for: data, using: hmacKey)
        return Data(auth)
    }

    static func hmacSHA256Verify(key: Data, data: Data, tag: Data) -> Bool {
        let hmacKey = SymmetricKey(data: key)
        return HMAC<SHA256>.isValidAuthenticationCode(tag, authenticating: data, using: hmacKey)
    }

    // MARK: - Nonce construction

    /// Server→client nonce from packet header fields.
    static func nonceServer(frameId: UInt32, chunkIdx: UInt16, flags: UInt8) -> Data {
        withUnsafeTemporaryAllocation(of: UInt8.self, capacity: 12) { ptr in
            // The daemon zero-pads unused nonce bytes; the temporary buffer is
            // uninitialized stack memory, so it must be zeroed before use.
            ptr.initialize(repeating: 0)
            ptr[0] = 0x00
            ptr[1] = UInt8(truncatingIfNeeded: frameId >> 24)
            ptr[2] = UInt8(truncatingIfNeeded: frameId >> 16)
            ptr[3] = UInt8(truncatingIfNeeded: frameId >> 8)
            ptr[4] = UInt8(truncatingIfNeeded: frameId)
            ptr[5] = UInt8(truncatingIfNeeded: chunkIdx >> 8)
            ptr[6] = UInt8(truncatingIfNeeded: chunkIdx)
            ptr[7] = flags
            return Data(UnsafeBufferPointer(start: ptr.baseAddress!, count: 12))
        }
    }

    /// Client→server nonce from sequence number.
    static func nonceClient(seqNum: UInt32) -> Data {
        withUnsafeTemporaryAllocation(of: UInt8.self, capacity: 12) { ptr in
            ptr.initialize(repeating: 0)
            ptr[0] = 0xFF
            ptr[1] = UInt8(truncatingIfNeeded: seqNum >> 24)
            ptr[2] = UInt8(truncatingIfNeeded: seqNum >> 16)
            ptr[3] = UInt8(truncatingIfNeeded: seqNum >> 8)
            ptr[4] = UInt8(truncatingIfNeeded: seqNum)
            return Data(UnsafeBufferPointer(start: ptr.baseAddress!, count: 12))
        }
    }

    /// Client→server TCP control nonce from sequence number.
    static func nonceControlClient(seqNum: UInt32) -> Data {
        withUnsafeTemporaryAllocation(of: UInt8.self, capacity: 12) { ptr in
            ptr.initialize(repeating: 0)
            ptr[0] = 0x7F
            ptr[1] = UInt8(truncatingIfNeeded: seqNum >> 24)
            ptr[2] = UInt8(truncatingIfNeeded: seqNum >> 16)
            ptr[3] = UInt8(truncatingIfNeeded: seqNum >> 8)
            ptr[4] = UInt8(truncatingIfNeeded: seqNum)
            return Data(UnsafeBufferPointer(start: ptr.baseAddress!, count: 12))
        }
    }

    /// Server→client TCP control nonce from sequence number.
    static func nonceControlServer(seqNum: UInt32) -> Data {
        withUnsafeTemporaryAllocation(of: UInt8.self, capacity: 12) { ptr in
            ptr.initialize(repeating: 0)
            ptr[0] = 0x80
            ptr[1] = UInt8(truncatingIfNeeded: seqNum >> 24)
            ptr[2] = UInt8(truncatingIfNeeded: seqNum >> 16)
            ptr[3] = UInt8(truncatingIfNeeded: seqNum >> 8)
            ptr[4] = UInt8(truncatingIfNeeded: seqNum)
            return Data(UnsafeBufferPointer(start: ptr.baseAddress!, count: 12))
        }
    }
}

import Foundation
import CryptoKit

// From client/ipad:
// swiftc Screx/MediaPacket.swift Screx/Crypto.swift Tests/MediaPacketTests.swift -o /tmp/screx-media-tests
@main
struct MediaPacketTests {
    static func main() {
        // Identical fixture in daemon/src/crypto.rs, independently checked against AESGCM.
        let header = Data([1, 2, 3, 4, 0, 2, 0, 3, 0, 1, 0x11, 1, 0, 4, 5, 6, 7, 8, 9, 10, 11, 12])
        let packet = MediaPacketHeader(header)!
        precondition(packet.frameID == 0x01020304 && packet.chunkIndex == 2)
        precondition(packet.totalData == 3 && packet.totalParity == 1)
        precondition(packet.displayID == .externalDisplay && packet.isIDR && !packet.isAudio && !packet.isHeartbeat)
        precondition(packet.codecID == 1 && packet.payloadLength == 4)
        precondition(packet.timestampMs == 0x05060708 && packet.generation == 0x090a0b0c)
        precondition(MediaPacketHeader(header.prefix(21)) == nil)
        var internalHeader = header
        internalHeader[10] = 1
        precondition(MediaPacketHeader(internalHeader)!.displayID == .internalDisplay)

        let nonce = ScrexCrypto.nonceServer(frameId: packet.frameID, chunkIdx: packet.chunkIndex,
                                          flags: packet.flags, generation: packet.generation)
        precondition(nonce == Data([0, 1, 2, 3, 4, 0, 2, 0x11, 9, 10, 11, 12]))
        let key = SymmetricKey(data: Data(repeating: 0x42, count: 32))
        let cleartext = Data([0xde, 0xad, 0xbe, 0xef])
        let encrypted = Data([0x8f, 0x85, 0x5c, 0x3b, 0xad, 0x35, 0x03, 0x12, 0xbe, 0x50,
                              0x87, 0xc8, 0xfa, 0xe9, 0xdd, 0x7e, 0xdb, 0x52, 0xe6, 0x77])
        precondition(ScrexCrypto.encrypt(key: key, nonce: nonce, plaintext: cleartext, aad: header) == encrypted)
        precondition(ScrexCrypto.decrypt(key: key, nonce: nonce, ciphertextAndTag: encrypted, aad: header) == cleartext)
        var tampered = header
        tampered[18] ^= 1
        precondition(ScrexCrypto.decrypt(key: key, nonce: nonce, ciphertextAndTag: encrypted, aad: tampered) == nil)

        var internalTimeline = VideoStreamTimeline()
        var externalTimeline = VideoStreamTimeline()
        precondition(!internalTimeline.accept(generation: 0))
        precondition(internalTimeline.accept(generation: 2))
        precondition(externalTimeline.accept(generation: 3))
        precondition(!internalTimeline.complete(0) && !externalTimeline.complete(0))
        precondition(!internalTimeline.isNewFrame(0), "Completed frame duplicates must be rejected")
        precondition(internalTimeline.complete(2), "One missing internal frame must signal a gap")
        precondition(!externalTimeline.complete(1), "The other display has no gap")
        precondition(internalTimeline.accept(generation: 4))
        precondition(internalTimeline.isNewFrame(0), "A new generation restarts frame numbers")
        precondition(!internalTimeline.complete(0))
        precondition(!internalTimeline.accept(generation: 2), "Delayed old shards must not resurrect a retired stream")
        precondition(internalTimeline.generation == 4 && internalTimeline.lastCompletedFrameID == 0)
        precondition(externalTimeline.generation == 3 && externalTimeline.lastCompletedFrameID == 1)
        precondition(internalTimeline.complete(UInt32.max))
        precondition(!internalTimeline.isNewFrame(0), "Frame counters never wrap within a generation")
        print("Media packet, crypto, and independent timeline tests passed")
    }
}

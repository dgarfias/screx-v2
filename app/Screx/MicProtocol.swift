import Foundation

enum MicProtocol {
    static let transportMagic = Data("MIC".utf8)

    static let version: UInt8 = 1
    static let codecPCM16LE: UInt8 = 1
    static let sampleRate: UInt16 = 48_000
    static let channels: UInt8 = 1
    static let samplesPerFrame: UInt16 = 480 // 10ms @ 48kHz
    static let bytesPerFrame = Int(samplesPerFrame) * 2

    // version(1) + codec(1) + sample_rate(2) + channels(1)
    // + samples_per_frame(2) + payload_len(2) + seq(4)
    static let headerLen = 13

    static func makePacket(seq: UInt32, pcmFrame: Data) -> Data {
        var packet = Data(capacity: transportMagic.count + headerLen + pcmFrame.count)
        packet.append(transportMagic)
        packet.append(version)
        packet.append(codecPCM16LE)

        withUnsafeBytes(of: sampleRate.bigEndian) { packet.append(contentsOf: $0) }
        packet.append(channels)
        withUnsafeBytes(of: samplesPerFrame.bigEndian) { packet.append(contentsOf: $0) }

        let payloadLen = UInt16(clamping: pcmFrame.count)
        withUnsafeBytes(of: payloadLen.bigEndian) { packet.append(contentsOf: $0) }
        withUnsafeBytes(of: seq.bigEndian) { packet.append(contentsOf: $0) }
        packet.append(pcmFrame)
        return packet
    }
}

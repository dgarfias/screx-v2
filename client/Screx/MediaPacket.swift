import Foundation

enum StreamDisplayID: UInt8, CaseIterable {
    case internalDisplay = 0
    case externalDisplay = 1
}

/// Shared daemon -> client media header, inside an authenticated QUIC datagram.
struct MediaPacketHeader {
    static let byteCount = 22
    let frameID: UInt32
    let chunkIndex: UInt16
    let totalData: UInt16
    let totalParity: UInt16
    let flags: UInt8
    let codecID: UInt8
    let payloadLength: UInt16
    let timestampMs: UInt32
    let generation: UInt32

    var isAudio: Bool { flags & 0x02 != 0 }
    var isIDR: Bool { flags & 0x01 != 0 }
    var displayID: StreamDisplayID { flags & 0x10 == 0 ? .internalDisplay : .externalDisplay }

    init?(_ data: Data) {
        guard data.count >= Self.byteCount else { return nil }
        func u16(_ offset: Int) -> UInt16 {
            UInt16(data[data.startIndex + offset]) << 8 | UInt16(data[data.startIndex + offset + 1])
        }
        func u32(_ offset: Int) -> UInt32 {
            UInt32(u16(offset)) << 16 | UInt32(u16(offset + 2))
        }
        frameID = u32(0)
        chunkIndex = u16(4)
        totalData = u16(6)
        totalParity = u16(8)
        flags = data[data.startIndex + 10]
        codecID = data[data.startIndex + 11]
        payloadLength = u16(12)
        timestampMs = u32(14)
        generation = u32(18)
    }
}

/// One display's authenticated stream epoch. A retired generation can never replace a new one.
/// No frame counter wraps within a generation; the daemon fails closed on exhaustion.
struct VideoStreamTimeline {
    private(set) var generation: UInt32?
    private(set) var lastCompletedFrameID: UInt32?

    mutating func accept(generation candidate: UInt32) -> Bool {
        guard candidate > 0 else { return false }
        if let generation, candidate < generation { return false }
        if generation != candidate {
            generation = candidate
            lastCompletedFrameID = nil
        }
        return true
    }

    func isNewFrame(_ frameID: UInt32) -> Bool {
        lastCompletedFrameID.map { frameID > $0 } ?? true
    }

    /// True if delivering `frameID` now would skip at least one earlier frame ID.
    func wouldSkip(_ frameID: UInt32) -> Bool {
        lastCompletedFrameID.map { frameID > $0 + 1 } ?? false
    }

    /// Call only for a successfully reassembled, newer frame. Returns whether frames were lost.
    mutating func complete(_ frameID: UInt32) -> Bool {
        let gap = lastCompletedFrameID.map { frameID > $0 && frameID - $0 > 1 } ?? false
        lastCompletedFrameID = frameID
        return gap
    }
}

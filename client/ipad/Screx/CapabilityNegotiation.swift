import Foundation

/// Capability negotiation wire format shared by the network control path
/// (`NetworkControlClient`) and the USB control path (`USBListener`).
///
/// This is a from-scratch Swift reimplementation of the TLV format documented in
/// docs/ARCHITECTURE.md ("Capability Negotiation") — there is no shared protocol
/// crate between the daemon and the clients, so this must match the daemon's Rust
/// implementation byte-for-byte. See that doc (and the capability-negotiation plan,
/// Part 0) before changing anything here.

/// What the daemon says it can do, parsed from a `CAPS` message.
///
/// `CAPS` wire format: `"CAPS"(4 bytes ASCII) + version(u8) + entry_count(u8) + entries`,
/// where each entry is `tag(u8) + length(u16 BE) + value(length bytes)`. A parser that
/// sees a tag it doesn't recognize skips it by its declared length and continues — it
/// never hard-fails on an unknown tag, so future tag additions stay backward compatible.
struct DaemonCapabilities: Equatable {
    var camera: Bool
    var microphone: Bool
    var speaker: Bool
    /// nil = gamepad passthrough unsupported; otherwise the max number of simultaneous
    /// controllers the daemon can pass through.
    var gamepadMaxControllers: UInt8?
    /// Codec ids the daemon can actually encode right now (0x00 = H.264, 0x01 = H.265).
    var codecs: [UInt8]
    var maxWidth: Int
    var maxHeight: Int
    var maxFps: Int
    var bitrateMinBps: UInt32
    var bitrateMaxBps: UInt32

    static let codecH264: UInt8 = 0x00
    static let codecH265: UInt8 = 0x01

    private static let magic = Data("CAPS".utf8)

    private static let tagCamera: UInt8 = 0x01
    private static let tagMicrophone: UInt8 = 0x02
    private static let tagSpeaker: UInt8 = 0x03
    private static let tagGamepad: UInt8 = 0x04
    private static let tagCodecs: UInt8 = 0x05
    private static let tagMaxResolution: UInt8 = 0x06
    private static let tagMaxFramerate: UInt8 = 0x07
    private static let tagBitrateRange: UInt8 = 0x08

    /// Permissive "assume everything works" defaults. Used two ways:
    /// - As the starting point while parsing a real `CAPS` message, so a daemon that
    ///   omits a tag still yields a usable value for that field.
    /// - As the legacy-daemon fallback when no `CAPS` message ever arrives at all (an
    ///   old daemon that predates this feature) — this preserves today's "everything is
    ///   available, just connect" behavior exactly.
    static let assumeAllAvailable = DaemonCapabilities(
        camera: true,
        microphone: true,
        speaker: true,
        gamepadMaxControllers: 4,
        codecs: [codecH264, codecH265],
        maxWidth: 3840,
        maxHeight: 2160,
        maxFps: 60,
        bitrateMinBps: 500_000,
        bitrateMaxBps: 20_000_000
    )

    /// Parses a `CAPS` payload. Returns nil only if the payload doesn't start with the
    /// `CAPS` magic or is too short to contain a version/entry_count header — malformed
    /// or truncated individual entries are simply skipped, not treated as a hard failure,
    /// so a partially-garbled message still yields whatever fields it could read.
    static func parse(_ payload: Data) -> DaemonCapabilities? {
        guard payload.starts(with: magic) else { return nil }

        var offset = payload.startIndex + magic.count
        guard offset + 2 <= payload.endIndex else { return nil }

        // version(u8): no behavior differs by version yet, just skip it.
        offset += 1
        let entryCount = Int(payload[offset])
        offset += 1

        var result = assumeAllAvailable

        for _ in 0..<entryCount {
            guard offset + 3 <= payload.endIndex else { break }
            let tag = payload[offset]
            offset += 1
            let length = Int(payload[offset]) << 8 | Int(payload[offset + 1])
            offset += 2
            guard offset + length <= payload.endIndex else { break }
            let value = payload[offset..<offset + length]
            offset += length

            switch tag {
            case tagCamera:
                guard length >= 1 else { continue }
                result.camera = value[value.startIndex] != 0

            case tagMicrophone:
                guard length >= 1 else { continue }
                result.microphone = value[value.startIndex] != 0

            case tagSpeaker:
                guard length >= 1 else { continue }
                result.speaker = value[value.startIndex] != 0

            case tagGamepad:
                guard length >= 2 else { continue }
                let available = value[value.startIndex] != 0
                let maxControllers = value[value.startIndex + 1]
                result.gamepadMaxControllers = available ? maxControllers : nil

            case tagCodecs:
                guard length >= 1 else { continue }
                let count = Int(value[value.startIndex])
                guard length >= 1 + count else { continue }
                let codecsStart = value.startIndex + 1
                result.codecs = Array(value[codecsStart..<codecsStart + count])

            case tagMaxResolution:
                guard length >= 4 else { continue }
                let b = value.startIndex
                let width = Int(value[b]) << 8 | Int(value[b + 1])
                let height = Int(value[b + 2]) << 8 | Int(value[b + 3])
                result.maxWidth = width
                result.maxHeight = height

            case tagMaxFramerate:
                guard length >= 1 else { continue }
                result.maxFps = Int(value[value.startIndex])

            case tagBitrateRange:
                guard length >= 8 else { continue }
                let b = value.startIndex
                let minBps = UInt32(value[b]) << 24 | UInt32(value[b + 1]) << 16
                    | UInt32(value[b + 2]) << 8 | UInt32(value[b + 3])
                let maxBps = UInt32(value[b + 4]) << 24 | UInt32(value[b + 5]) << 16
                    | UInt32(value[b + 6]) << 8 | UInt32(value[b + 7])
                result.bitrateMinBps = minBps
                result.bitrateMaxBps = maxBps

            default:
                // Unknown/future tag: already skipped above via `length`.
                break
            }
        }

        return result
    }

    /// Clamps client-preferred stream settings to this daemon's advertised bounds
    /// (Validation/clamping bounds: resolution floor 640x360, framerate floor 15,
    /// bitrate floor 500 Kbps fixed; ceilings come from this struct). A value that
    /// doesn't fit is dropped (set to nil) rather than snapped to the nearest bound, so
    /// the daemon falls back to its own configured default for that field instead of
    /// getting a fabricated substitute.
    func clamp(_ settings: StreamSettings) -> StreamSettings {
        var clamped = StreamSettings()

        if let width = settings.width, let height = settings.height,
           width >= 640, height >= 360, width <= maxWidth, height <= maxHeight {
            clamped.width = width
            clamped.height = height
        }

        if let fps = settings.fps, fps >= 15, fps <= maxFps {
            clamped.fps = fps
        }

        if let codecId = settings.codecId, codecs.contains(codecId) {
            clamped.codecId = codecId
        }

        if let bitrateBps = settings.bitrateBps,
           bitrateBps >= bitrateMinBps, bitrateBps <= bitrateMaxBps {
            clamped.bitrateBps = bitrateBps
        }

        return clamped
    }
}

/// Client-proposed stream settings, sent to the daemon as `STNG`. Any field left `nil`
/// means "use the daemon's own default for that field" — the corresponding TLV entry is
/// simply omitted rather than sent with a placeholder value.
///
/// `STNG` wire format: `"STNG"(4 bytes ASCII) + version(u8) + entry_count(u8) + entries`,
/// same entry shape as `CAPS` (`tag(u8) + length(u16 BE) + value`).
struct StreamSettings: Equatable {
    var width: Int?
    var height: Int?
    var fps: Int?
    var codecId: UInt8?
    var bitrateBps: UInt32?

    private static let magic = Data("STNG".utf8)
    private static let version: UInt8 = 1

    private static let tagResolution: UInt8 = 0x01
    private static let tagFramerate: UInt8 = 0x02
    private static let tagCodec: UInt8 = 0x03
    private static let tagBitrate: UInt8 = 0x04

    init(width: Int? = nil, height: Int? = nil, fps: Int? = nil, codecId: UInt8? = nil, bitrateBps: UInt32? = nil) {
        self.width = width
        self.height = height
        self.fps = fps
        self.codecId = codecId
        self.bitrateBps = bitrateBps
    }

    /// Builds the `STNG` message body (the control-channel framing around it, e.g. the
    /// encrypted-frame envelope on the network path or the `msgControl` byte on the USB
    /// path, is added by the caller exactly like it is for every other control message).
    /// A settings value with every field nil still produces a valid message with
    /// `entry_count = 0`, meaning "everything default, just connect."
    func encode() -> Data {
        var entries: [(tag: UInt8, value: Data)] = []

        if let width, let height, width > 0, height > 0, width <= Int(UInt16.max), height <= Int(UInt16.max) {
            var value = Data()
            withUnsafeBytes(of: UInt16(width).bigEndian) { value.append(contentsOf: $0) }
            withUnsafeBytes(of: UInt16(height).bigEndian) { value.append(contentsOf: $0) }
            entries.append((Self.tagResolution, value))
        }

        if let fps, fps > 0, fps <= Int(UInt8.max) {
            entries.append((Self.tagFramerate, Data([UInt8(fps)])))
        }

        if let codecId {
            entries.append((Self.tagCodec, Data([codecId])))
        }

        if let bitrateBps {
            var value = Data()
            withUnsafeBytes(of: bitrateBps.bigEndian) { value.append(contentsOf: $0) }
            entries.append((Self.tagBitrate, value))
        }

        var payload = Self.magic
        payload.append(Self.version)
        payload.append(UInt8(entries.count))
        for entry in entries {
            payload.append(entry.tag)
            withUnsafeBytes(of: UInt16(entry.value.count).bigEndian) { payload.append(contentsOf: $0) }
            payload.append(entry.value)
        }
        return payload
    }
}

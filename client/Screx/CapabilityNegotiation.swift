import Foundation

/// Capability negotiation messages carried on the control channel
/// (`NetworkControlClient`). Fixed layouts, big-endian; must match the daemon's
/// `build_caps_message` / `parse_settings_body` in daemon/src/stream_server.rs.

/// What the daemon says it can do, parsed from a `CAPS` message body (after the type byte):
/// `camera(u8) mic(u8) speaker(u8) gamepad_max(u8, 0 = unsupported)
/// codecs(u8 bitmask, bit n = codec id n) max_width(u16) max_height(u16) max_fps(u8)
/// bitrate_min(u32) bitrate_max(u32) hostname(UTF-8, rest of message)`.
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
    /// The daemon host's name, for display. May be empty.
    var hostname: String

    static let codecH264: UInt8 = 0x00
    static let codecH265: UInt8 = 0x01

    private static let fixedLength = 18

    /// Parses a `CAPS` body. Returns nil if it's shorter than the fixed fields.
    static func parse(_ body: Data) -> DaemonCapabilities? {
        let bytes = [UInt8](body)
        guard bytes.count >= fixedLength else { return nil }
        func u16(_ i: Int) -> Int { Int(bytes[i]) << 8 | Int(bytes[i + 1]) }
        func u32(_ i: Int) -> UInt32 {
            UInt32(bytes[i]) << 24 | UInt32(bytes[i + 1]) << 16 | UInt32(bytes[i + 2]) << 8 | UInt32(bytes[i + 3])
        }
        let codecMask = bytes[4]
        return DaemonCapabilities(
            camera: bytes[0] != 0,
            microphone: bytes[1] != 0,
            speaker: bytes[2] != 0,
            gamepadMaxControllers: bytes[3] == 0 ? nil : bytes[3],
            codecs: [codecH264, codecH265].filter { codecMask & (1 << $0) != 0 },
            maxWidth: u16(5),
            maxHeight: u16(7),
            maxFps: Int(bytes[9]),
            bitrateMinBps: u32(10),
            bitrateMaxBps: u32(14),
            hostname: String(decoding: bytes[fixedLength...], as: UTF8.self)
                .trimmingCharacters(in: .whitespacesAndNewlines)
        )
    }

    /// Returns human-readable violations of `settings` against these capabilities, empty
    /// if everything fits. Messages are shown verbatim on the host's tile, next to a Settings
    /// button. A `nil` field in `settings` means "use the daemon's own
    /// default", which always fits and is never checked.
    ///
    /// This is a fail-fast check, not a clamp: the caller must not modify or snap the
    /// user's configured settings when a violation is reported — it must refuse to
    /// connect so the user can adjust their settings and reconnect instead.
    func validate(_ settings: StreamSettings) -> [String] {
        var violations: [String] = []

        if let width = settings.width, let height = settings.height {
            if width > maxWidth || height > maxHeight {
                violations.append("Supports up to \(maxWidth)×\(maxHeight)")
            }
        }
        if settings.enabledDisplayMask & 2 != 0,
           let width = settings.externalWidth, let height = settings.externalHeight,
           width > maxWidth || height > maxHeight {
            violations.append("Supports up to \(maxWidth)×\(maxHeight) on the external display")
        }

        if let fps = settings.fps, fps > maxFps {
            violations.append("Supports up to \(maxFps) fps")
        }
        if settings.enabledDisplayMask & 2 != 0, let fps = settings.externalFps, fps > maxFps {
            violations.append("Supports up to \(maxFps) fps on the external display")
        }

        if let codecId = settings.codecId, !codecs.contains(codecId) {
            let codecLabel = codecId == Self.codecH265 ? "H.265" : "H.264"
            violations.append("Doesn’t support \(codecLabel)")
        }

        if let bitrateBps = settings.bitrateBps {
            if bitrateBps > bitrateMaxBps {
                violations.append("Supports up to \(Self.formatMbps(bitrateMaxBps)) Mbps")
            } else if bitrateBps < bitrateMinBps {
                violations.append("Needs at least \(Self.formatMbps(bitrateMinBps)) Mbps")
            }
        }

        return violations
    }

    /// Formats a bps value as a trimmed Mbps string, e.g. `12_500_000` -> "12.5",
    /// `20_000_000` -> "20".
    private static func formatMbps(_ bps: UInt32) -> String {
        let mbps = Double(bps) / 1_000_000
        let rounded = (mbps * 100).rounded() / 100
        if rounded == rounded.rounded() {
            return String(format: "%.0f", rounded)
        }
        var text = String(format: "%.2f", rounded)
        while text.hasSuffix("0") {
            text.removeLast()
        }
        if text.hasSuffix(".") {
            text.removeLast()
        }
        return text
    }
}

/// Client-proposed stream settings, sent to the daemon as `SETTINGS`. A `nil` field means
/// "use the daemon's own default" and goes on the wire as 0 (`codecDefault` for the codec).
///
/// Body: internal width/height/FPS, shared codec/bitrate, mask, external width/height, revision,
/// external FPS, internal EDID name, internal panel width/height in mm (0 = unknown).
struct StreamSettings: Equatable {
    var width: Int?
    var height: Int?
    var fps: Int?
    var codecId: UInt8?
    var bitrateBps: UInt32?
    var enabledDisplayMask: UInt8 = 1
    var externalWidth: Int?
    var externalHeight: Int?
    var externalFps: Int?
    var revision: UInt32 = 1
    var internalDisplayName = DeviceDisplayIdentity.current.edidName
    /// Gives the Linux monitor a real size, so the desktop scales it and maps touch to it
    /// like a real touch monitor. nil sends "unknown".
    var internalPanelSize: PanelSize?

    private static let codecDefault: UInt8 = 0xFF

    init(width: Int? = nil, height: Int? = nil, fps: Int? = nil, codecId: UInt8? = nil, bitrateBps: UInt32? = nil) {
        self.width = width
        self.height = height
        self.fps = fps
        self.codecId = codecId
        self.bitrateBps = bitrateBps
    }

    /// Builds the `SETTINGS` body (the caller adds the type byte and control framing).
    func encode() -> Data {
        let fitsU16 = { (v: Int?) -> Bool in v.map { $0 > 0 && $0 <= Int(UInt16.max) } ?? false }
        let hasResolution = fitsU16(width) && fitsU16(height)
        let w = UInt16(hasResolution ? width! : 0)
        let h = UInt16(hasResolution ? height! : 0)
        let f = UInt8(fps.map { $0 > 0 && $0 <= Int(UInt8.max) ? $0 : 0 } ?? 0)

        var body = Data()
        withUnsafeBytes(of: w.bigEndian) { body.append(contentsOf: $0) }
        withUnsafeBytes(of: h.bigEndian) { body.append(contentsOf: $0) }
        body.append(f)
        body.append(codecId ?? Self.codecDefault)
        withUnsafeBytes(of: (bitrateBps ?? 0).bigEndian) { body.append(contentsOf: $0) }
        body.append(enabledDisplayMask)
        let hasExternalResolution = fitsU16(externalWidth) && fitsU16(externalHeight)
        let ew = UInt16(hasExternalResolution ? externalWidth! : 0)
        let eh = UInt16(hasExternalResolution ? externalHeight! : 0)
        withUnsafeBytes(of: ew.bigEndian) { body.append(contentsOf: $0) }
        withUnsafeBytes(of: eh.bigEndian) { body.append(contentsOf: $0) }
        withUnsafeBytes(of: revision.bigEndian) { body.append(contentsOf: $0) }
        body.append(UInt8(externalFps.map { $0 > 0 && $0 <= 255 ? $0 : 0 } ?? 0))
        let name = Array(internalDisplayName.utf8.prefix(13))
        body.append(contentsOf: name)
        body.append(Data(repeating: 0x20, count: 13 - name.count))
        let mm = { (v: Int?) -> UInt16 in UInt16(v.map { $0 > 0 && $0 <= Int(UInt16.max) ? $0 : 0 } ?? 0) }
        withUnsafeBytes(of: mm(internalPanelSize?.widthMm).bigEndian) { body.append(contentsOf: $0) }
        withUnsafeBytes(of: mm(internalPanelSize?.heightMm).bigEndian) { body.append(contentsOf: $0) }
        return body
    }
}

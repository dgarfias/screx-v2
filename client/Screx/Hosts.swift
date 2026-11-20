import Foundation

/// A host tile on the connection screen. `tileId` is a random 16-byte identity that the iPad
/// presents to the daemon as its device ID when connecting through this tile, so every tile has
/// its own pairing. The tile's pairing key, stream settings and remembered limits are all stored
/// under it; removing the tile forgets them, and editing the address keeps them.
struct RecentConnection: Codable, Identifiable, Equatable {
    var host: String
    var port: UInt16
    var name: String
    var lastConnectedAt: Date
    var isPinned: Bool
    /// 32 lowercase hex characters (16 bytes).
    let tileId: String

    private enum CodingKeys: String, CodingKey {
        case host, port, name, lastConnectedAt, isPinned, tileId
    }

    init(host: String, port: UInt16, name: String, lastConnectedAt: Date, isPinned: Bool = false,
         tileId: String = RecentConnection.newTileId()) {
        self.host = host
        self.port = port
        self.name = name
        self.lastConnectedAt = lastConnectedAt
        self.isPinned = isPinned
        self.tileId = tileId
    }

    /// Entries saved before tiles had IDs get a new one (and pair again once).
    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        host = try container.decode(String.self, forKey: .host)
        port = try container.decode(UInt16.self, forKey: .port)
        name = try container.decode(String.self, forKey: .name)
        lastConnectedAt = try container.decode(Date.self, forKey: .lastConnectedAt)
        isPinned = try container.decodeIfPresent(Bool.self, forKey: .isPinned) ?? false
        let stored = try container.decodeIfPresent(String.self, forKey: .tileId)
        tileId = stored.flatMap { Data(hex: $0)?.count == 16 ? $0 : nil } ?? Self.newTileId()
    }

    var id: String { tileId }
    var displayName: String { name.isEmpty ? host : name }
    var endpointLabel: String { formatEndpointInput(host: host, port: port) }
    /// The device ID sent to the daemon.
    var deviceId: Data { Data(hex: tileId) ?? Data(count: 16) }

    static func newTileId() -> String {
        var bytes = Data(count: 16)
        _ = bytes.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, 16, $0.baseAddress!) }
        return bytes.hexString
    }
}

/// Speaker-audio presets, persisted and sent by the iPad like video bitrate presets.
enum AudioBitratePreset: String, CaseIterable, Identifiable {
    case ultraLow = "ultralow"
    case veryLow = "verylow"
    case low, medium, high
    case veryHigh = "veryhigh"

    var id: String { rawValue }
    var bitrateBps: UInt32 {
        switch self {
        case .ultraLow: return 48_000
        case .veryLow: return 64_000
        case .low: return 96_000
        case .medium: return 128_000
        case .high: return 192_000
        case .veryHigh: return 256_000
        }
    }
    var label: String {
        switch self {
        case .ultraLow: return "Ultra low (48 kbps)"
        case .veryLow: return "Very low (64 kbps)"
        case .low: return "Low (96 kbps)"
        case .medium: return "Medium (128 kbps)"
        case .high: return "High (192 kbps)"
        case .veryHigh: return "Very high (256 kbps)"
        }
    }
}

/// Stream settings chosen for one host.
struct HostStreamSettings: Codable, Equatable {
    var resolution: StreamResolutionPreset
    var framerate: StreamFrameratePreset
    var codec: StreamCodecPreset
    var bitrate: StreamBitratePreset
    var audioBitrate: AudioBitratePreset = .medium
    /// Used when `bitrate == .custom`.
    var customBitrateMbps: Double

    /// A new host starts at this iPad's resolution, 60 fps, the host's codec and 8 Mbps.
    static var defaults: HostStreamSettings {
        HostStreamSettings(resolution: StreamResolutionPreset.deviceNative ?? .native, framerate: .fps60,
                           codec: .daemonDefault, bitrate: .medium, customBitrateMbps: 10)
    }

    init(resolution: StreamResolutionPreset, framerate: StreamFrameratePreset, codec: StreamCodecPreset,
         bitrate: StreamBitratePreset, customBitrateMbps: Double) {
        self.resolution = resolution
        self.framerate = framerate
        self.codec = codec
        self.bitrate = bitrate
        self.customBitrateMbps = customBitrateMbps
    }

    private enum CodingKeys: String, CodingKey {
        case resolution, framerate, codec, bitrate, audioBitrate, customBitrateMbps
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(resolution.rawValue, forKey: .resolution)
        try container.encode(framerate.rawValue, forKey: .framerate)
        try container.encode(codec.rawValue, forKey: .codec)
        try container.encode(bitrate.rawValue, forKey: .bitrate)
        try container.encode(audioBitrate.rawValue, forKey: .audioBitrate)
        try container.encode(customBitrateMbps, forKey: .customBitrateMbps)
    }

    /// Unknown values (a preset removed in a later version) fall back to the defaults.
    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let fallback = Self.defaults
        func raw(_ key: CodingKeys) -> String { (try? container.decode(String.self, forKey: key)) ?? "" }
        var resolution = StreamResolutionPreset(rawValue: raw(.resolution)) ?? fallback.resolution
        // "This iPad" is offered as the matching listed size when one exists.
        if resolution == .native, let listed = StreamResolutionPreset.deviceNative { resolution = listed }
        self.resolution = resolution
        framerate = StreamFrameratePreset(rawValue: raw(.framerate)) ?? fallback.framerate
        codec = StreamCodecPreset(rawValue: raw(.codec)) ?? fallback.codec
        bitrate = StreamBitratePreset(rawValue: raw(.bitrate)) ?? fallback.bitrate
        audioBitrate = AudioBitratePreset(rawValue: raw(.audioBitrate)) ?? .medium
        let custom = (try? container.decode(Double.self, forKey: .customBitrateMbps)) ?? fallback.customBitrateMbps
        customBitrateMbps = custom.isFinite && custom > 0 ? custom : fallback.customBitrateMbps
    }

    /// Sent to the daemon unchanged once `DaemonCapabilities.validate` confirms it fits; a
    /// setting that doesn't fit fails the connection instead of being adjusted.
    var streamSettings: StreamSettings {
        let size = resolution.resolution
        var settings = StreamSettings(width: size?.width, height: size?.height, fps: framerate.fps,
                                      codecId: codec.codecId, bitrateBps: bitrateBps)
        settings.audioBitrateBps = audioBitrate.bitrateBps
        let native = StreamResolutionPreset.nativeResolution
        settings.internalPanelSize = DeviceDisplayIdentity.current.panelSizeMm(
            nativePixels: (long: native.width, short: native.height))
        return settings
    }

    /// nil means the host's default. A custom value is capped only to fit the wire field.
    var bitrateBps: UInt32? {
        guard bitrate == .custom else { return bitrate.bitrateBps }
        let bps = (customBitrateMbps * 1_000_000).rounded()
        guard bps.isFinite, bps > 0 else { return nil }
        return UInt32(min(bps, Double(UInt32.max)))
    }
}

/// The stream limits a host advertised in its last `CAPS`, so its Settings only offer what it
/// supports.
struct HostLimits: Codable, Equatable {
    var maxWidth: Int
    var maxHeight: Int
    var maxFps: Int
    var codecs: [UInt8]
    var bitrateMinBps: UInt32
    var bitrateMaxBps: UInt32

    init(_ capabilities: DaemonCapabilities) {
        maxWidth = capabilities.maxWidth
        maxHeight = capabilities.maxHeight
        maxFps = capabilities.maxFps
        codecs = capabilities.codecs
        bitrateMinBps = capabilities.bitrateMinBps
        bitrateMaxBps = capabilities.bitrateMaxBps
    }

    func supports(_ preset: StreamResolutionPreset) -> Bool {
        guard let size = preset.resolution else { return true }
        return size.width <= maxWidth && size.height <= maxHeight
    }

    func supports(_ preset: StreamFrameratePreset) -> Bool {
        preset.fps.map { $0 <= maxFps } ?? true
    }

    func supports(_ preset: StreamCodecPreset) -> Bool {
        preset.codecId.map { codecs.contains($0) } ?? true
    }

    func supports(bitrateBps: UInt32?) -> Bool {
        bitrateBps.map { $0 >= bitrateMinBps && $0 <= bitrateMaxBps } ?? true
    }
}

/// What a host tile on the connection screen shows.
enum HostTileState: Equatable {
    case idle
    case connecting(String)
    case failed(String)
}

extension Data {
    init?(hex: String) {
        guard hex.count % 2 == 0 else { return nil }
        var bytes = [UInt8]()
        bytes.reserveCapacity(hex.count / 2)
        var index = hex.startIndex
        while index < hex.endIndex {
            let next = hex.index(index, offsetBy: 2)
            guard let byte = UInt8(hex[index..<next], radix: 16) else { return nil }
            bytes.append(byte)
            index = next
        }
        self.init(bytes)
    }

    var hexString: String { map { String(format: "%02x", $0) }.joined() }
}

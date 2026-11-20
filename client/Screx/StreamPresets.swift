import Foundation
import UIKit

enum ConnectionTransport: String {
    case none = ""
    case network = "Network"
}

enum ConnectionHealthState: String {
    case idle = "Idle"
    case connecting = "Connecting"
    case pairing = "Pairing"
    case waitingForVideo = "Waiting for video"
    case streaming = "Streaming"
    case busy = "Busy"
    case connectionRefused = "Connection refused"
    case timedOut = "Timed out"
    case sessionStale = "Session stale, try again"
    case connectionError = "Connection error"

    var isConnected: Bool {
        switch self {
        case .streaming:
            return true
        default:
            return false
        }
    }

    var isConnecting: Bool {
        switch self {
        case .connecting, .pairing, .waitingForVideo:
            return true
        default:
            return false
        }
    }

    var isTerminalFailure: Bool {
        switch self {
        case .busy, .connectionRefused, .timedOut, .sessionStale, .connectionError:
            return true
        default:
            return false
        }
    }
}

/// User-facing resolution presets offered in a host's Settings sheet. `daemonDefault`
/// means "send 0 for the resolution in `SETTINGS`" (let the daemon pick). `native`
/// ("This iPad") is this device's own panel resolution, detected from `UIScreen`; it is
/// only offered in the picker when no listed preset already matches the panel (see
/// `pickerCases`), so current models don't see a duplicate entry but a future unlisted
/// iPad still defaults to its exact panel resolution.
enum StreamResolutionPreset: String, CaseIterable, Identifiable {
    case daemonDefault = "default"
    case native = "native"
    case p720 = "720p"
    case p1080 = "1080p"
    case p1440 = "1440p"
    case uhd4k = "4k"
    case wxga1610 = "1280x800"
    case wuxga = "1920x1200"
    case wqxga = "2560x1600"
    case ipadMini = "2266x1488"
    case ipadBase = "2160x1620"
    case ipadA16 = "2360x1640"
    case ipadPro11 = "2388x1668"
    case ipadPro11M4 = "2420x1668"
    case ipadPro129 = "2732x2048"
    case ipadPro13M4 = "2752x2064"

    var id: String { rawValue }

    /// This device's native panel resolution in landscape orientation (width >= height),
    /// derived once from `UIScreen.main.nativeBounds` (a pixel size, not points).
    /// Cached because `UIScreen.main` access and the width/height normalization never
    /// change for the lifetime of the process.
    static let nativeResolution: (width: Int, height: Int) = {
        let bounds = UIScreen.main.nativeBounds
        let long = Int(max(bounds.width, bounds.height))
        let short = Int(min(bounds.width, bounds.height))
        return (long, short)
    }()

    /// The listed preset matching this device's native panel resolution, if the list has
    /// one (it does for every iPad model the list was built from). `.native` itself is
    /// excluded from the search — it always matches by construction, and the point is to
    /// find a *listed* duplicate of it.
    static let deviceNative: StreamResolutionPreset? = allCases.first { preset in
        guard preset != .native, let resolution = preset.resolution else { return false }
        return resolution == nativeResolution
    }

    /// The cases offered in the Settings resolution picker: `.native` ("This iPad") is
    /// included only when no listed preset matches this device's panel, so it never
    /// shows up as a duplicate of a listed value.
    static let pickerCases: [StreamResolutionPreset] =
        deviceNative == nil ? allCases : allCases.filter { $0 != .native }

    /// One line in the Settings row, always size first: this device's panel adds "This iPad", other iPad sizes
    /// name one model instead of the full list that shares the size.
    var label: String {
        if self == Self.deviceNative || self == .native, let size = resolution {
            return "\(size.width)×\(size.height) · This iPad"
        }
        switch self {
        case .daemonDefault: return "Host default"
        case .native: return "This iPad"
        case .p720: return "1280×720"
        case .p1080: return "1920×1080"
        case .p1440: return "2560×1440"
        case .uhd4k: return "3840×2160 (4K)"
        case .wxga1610: return "1280×800"
        case .wuxga: return "1920×1200"
        case .wqxga: return "2560×1600"
        case .ipadMini: return "2266×1488 · iPad mini"
        case .ipadBase: return "2160×1620 · iPad 9th gen"
        case .ipadA16: return "2360×1640 · iPad Air 11″"
        case .ipadPro11: return "2388×1668 · iPad Pro 11″"
        case .ipadPro11M4: return "2420×1668 · iPad Pro 11″ M4"
        case .ipadPro129: return "2732×2048 · iPad Pro 12.9″"
        case .ipadPro13M4: return "2752×2064 · iPad Pro 13″ M4"
        }
    }

    var resolution: (width: Int, height: Int)? {
        switch self {
        case .daemonDefault: return nil
        case .native: return Self.nativeResolution
        case .p720: return (1280, 720)
        case .p1080: return (1920, 1080)
        case .p1440: return (2560, 1440)
        case .uhd4k: return (3840, 2160)
        case .wxga1610: return (1280, 800)
        case .wuxga: return (1920, 1200)
        case .wqxga: return (2560, 1600)
        case .ipadMini: return (2266, 1488)
        case .ipadBase: return (2160, 1620)
        case .ipadA16: return (2360, 1640)
        case .ipadPro11: return (2388, 1668)
        case .ipadPro11M4: return (2420, 1668)
        case .ipadPro129: return (2732, 2048)
        case .ipadPro13M4: return (2752, 2064)
        }
    }
}

/// User-facing framerate presets offered in a host's Settings sheet.
enum StreamFrameratePreset: String, CaseIterable, Identifiable {
    case daemonDefault = "default"
    case fps30 = "30"
    case fps60 = "60"

    var id: String { rawValue }

    var label: String {
        switch self {
        case .daemonDefault: return "Host default"
        case .fps30: return "30 fps"
        case .fps60: return "60 fps"
        }
    }

    var fps: Int? {
        switch self {
        case .daemonDefault: return nil
        case .fps30: return 30
        case .fps60: return 60
        }
    }
}

/// User-facing codec presets offered in a host's Settings sheet.
enum StreamCodecPreset: String, CaseIterable, Identifiable {
    case daemonDefault = "default"
    case h264 = "h264"
    case h265 = "h265"

    var id: String { rawValue }

    var label: String {
        switch self {
        case .daemonDefault: return "Host default"
        case .h264: return "H.264"
        case .h265: return "H.265"
        }
    }

    var codecId: UInt8? {
        switch self {
        case .daemonDefault: return nil
        case .h264: return DaemonCapabilities.codecH264
        case .h265: return DaemonCapabilities.codecH265
        }
    }
}

/// User-facing bitrate presets offered in a host's Settings sheet. `.custom` has no
/// fixed `bitrateBps` of its own — its effective value comes from
/// `HostStreamSettings.customBitrateMbps` instead (see `HostStreamSettings.bitrateBps`).
enum StreamBitratePreset: String, CaseIterable, Identifiable {
    case daemonDefault = "default"
    case veryLow = "verylow"
    case low = "low"
    case medium = "medium"
    case high = "high"
    case veryHigh = "veryhigh"
    case custom = "custom"

    var id: String { rawValue }

    var label: String {
        switch self {
        case .daemonDefault: return "Host default"
        case .veryLow: return "Very low (1 Mbps)"
        case .low: return "Low (3 Mbps)"
        case .medium: return "Medium (8 Mbps)"
        case .high: return "High (15 Mbps)"
        case .veryHigh: return "Very high (20 Mbps)"
        case .custom: return "Custom…"
        }
    }

    var bitrateBps: UInt32? {
        switch self {
        case .daemonDefault: return nil
        case .veryLow: return 1_000_000
        case .low: return 3_000_000
        case .medium: return 8_000_000
        case .high: return 15_000_000
        case .veryHigh: return 20_000_000
        case .custom: return nil
        }
    }
}


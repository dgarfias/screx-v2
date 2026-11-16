import Foundation

enum DisplayOutputMode: UInt8, CaseIterable, Identifiable {
    case internalOnly = 1
    case externalOnly = 2
    case both = 3

    var id: UInt8 { rawValue }
    var label: String {
        switch self {
        case .internalOnly: return "Internal only"
        case .externalOnly: return "External only"
        case .both: return "Both"
        }
    }
}

struct DisplayResolution: Hashable, Identifiable {
    let width: Int
    let height: Int
    var id: String { "\(width)x\(height)" }
    var label: String { "\(width)×\(height)" }
}

/// UIKit exposes resolution modes and a screen-wide FPS ceiling, not EDID refresh-rate pairs.
struct ExternalDisplayCapabilities: Equatable {
    var resolutions: [DisplayResolution]
    var preferredResolution: DisplayResolution?
    var maximumFramesPerSecond: Int

    func resolutionChoices(maxWidth: Int, maxHeight: Int) -> [DisplayResolution] {
        return Set(resolutions).filter {
            $0.width > 0 && $0.height > 0 && $0.width <= maxWidth && $0.height <= maxHeight
        }.sorted { ($0.width, $0.height) < ($1.width, $1.height) }
    }

    func frameRateChoices(daemonMaximum: Int, selected: Int) -> [Int] {
        let ceiling = min(maximumFramesPerSecond, daemonMaximum, 255)
        return Set([15, 24, 25, 30, 50, 60, 90, 120, selected]).filter { $0 >= 15 && $0 <= ceiling }.sorted()
    }
}

/// Per-host display choice, persisted in UserDefaults.
struct HostDisplayPreferences: Codable, Equatable {
    var mode: UInt8
    var externalWidth: Int?
    var externalHeight: Int?
    var externalFps: Int?
}

/// Pure selection rules, so connect-time defaults are testable without UIKit.
enum DisplayPreferenceRules {
    /// A remembered external choice falls back to internal while no monitor is attached;
    /// the monitor appearing later re-applies the remembered mode.
    static func mode(storedMode: UInt8?, externalAvailable: Bool) -> DisplayOutputMode {
        let wanted = storedMode.flatMap(DisplayOutputMode.init(rawValue:)) ?? .internalOnly
        guard externalAvailable || wanted == .internalOnly else { return .internalOnly }
        return wanted
    }

    /// Prefer the remembered resolution while the display still reports it, then the display's
    /// recommended mode, then the largest compatible mode. Never invent a resolution.
    static func resolution(stored: HostDisplayPreferences?,
                           available: [DisplayResolution],
                           recommended: DisplayResolution?) -> DisplayResolution? {
        if let width = stored?.externalWidth, let height = stored?.externalHeight {
            let remembered = DisplayResolution(width: width, height: height)
            if available.isEmpty || available.contains(remembered) { return remembered }
        }
        if let recommended, available.isEmpty || available.contains(recommended) { return recommended }
        return available.last
    }
}

/// Keep both old and requested surfaces attached until the daemon commits the transition.
struct DisplayRoutingState {
    var sessionEnabled = false
    var externalAvailable = false
    var requestedMode: DisplayOutputMode = .internalOnly
    var committedMask: UInt8 = 0

    var internalVideoVisible: Bool {
        !sessionEnabled || (requestedMode.rawValue | committedMask) & 1 != 0
    }
    var externalWindowNeeded: Bool {
        sessionEnabled && externalAvailable && (requestedMode.rawValue | committedMask) & 2 != 0
    }

    func canAcknowledge(_ display: StreamDisplayID, surfaceAttached: Bool) -> Bool {
        sessionEnabled && surfaceAttached && requestedMode.rawValue & (1 << display.rawValue) != 0
            && (display == .internalDisplay || externalAvailable)
    }
}

enum DisplayStatusPhase: UInt8 {
    case streaming = 0
    case committed = 1
    case failed = 2
    case removed = 3
}

struct DisplayStatus {
    let revision: UInt32
    let display: StreamDisplayID
    let phase: DisplayStatusPhase
    let activeMask: UInt8
    let generation: UInt32
    let width: Int
    let height: Int
    let detail: String

    static func parse(_ body: Data) -> DisplayStatus? {
        let bytes = [UInt8](body)
        guard bytes.count >= 15,
              let display = StreamDisplayID(rawValue: bytes[4]),
              let phase = DisplayStatusPhase(rawValue: bytes[5]), bytes[6] <= 3 else { return nil }
        func u32(_ i: Int) -> UInt32 {
            UInt32(bytes[i]) << 24 | UInt32(bytes[i + 1]) << 16 | UInt32(bytes[i + 2]) << 8 | UInt32(bytes[i + 3])
        }
        return DisplayStatus(revision: u32(0), display: display, phase: phase, activeMask: bytes[6],
                             generation: u32(7), width: Int(bytes[11]) << 8 | Int(bytes[12]),
                             height: Int(bytes[13]) << 8 | Int(bytes[14]),
                             detail: String(decoding: bytes.dropFirst(15), as: UTF8.self))
    }
}

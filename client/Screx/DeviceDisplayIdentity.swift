import Foundation
#if canImport(Darwin)
import Darwin
#endif

/// Marketing names/sizes, not measurements inferred from pixels. Updated 2026-11-15.
/// Models: https://support.apple.com/108043 (including mini A20 Pro, specs /149482).
/// Hardware IDs cross-checked against https://api.ipsw.me/v4/devices and DeviceKit's model table.
/// A panel's physical size in landscape orientation.
struct PanelSize: Equatable {
    var widthMm: Int
    var heightMm: Int
}

struct DeviceDisplayIdentity: Equatable {
    let name: String
    let edidName: String
    let diagonalInches: Double?

    struct Model {
        let identifiers: [String]
        let identity: DeviceDisplayIdentity
    }

    private static func model(_ ids: String, _ name: String, _ short: String, _ inches: Double) -> Model {
        Model(identifiers: ids.split(separator: " ").map(String.init),
              identity: DeviceDisplayIdentity(name: name, edidName: short, diagonalInches: inches))
    }

    static let models: [Model] = [
        model("iPad1,1", "iPad (1st gen) 9.7″", "iPad 1 9.7\"", 9.7),
        model("iPad2,1 iPad2,2 iPad2,3 iPad2,4", "iPad 2 9.7″", "iPad 2 9.7\"", 9.7),
        model("iPad3,1 iPad3,2 iPad3,3", "iPad (3rd gen) 9.7″", "iPad 3 9.7\"", 9.7),
        model("iPad3,4 iPad3,5 iPad3,6", "iPad (4th gen) 9.7″", "iPad 4 9.7\"", 9.7),
        model("iPad6,11 iPad6,12", "iPad (5th gen) 9.7″", "iPad 5 9.7\"", 9.7),
        model("iPad7,5 iPad7,6", "iPad (6th gen) 9.7″", "iPad 6 9.7\"", 9.7),
        model("iPad7,11 iPad7,12", "iPad (7th gen) 10.2″", "iPad 7 10.2\"", 10.2),
        model("iPad11,6 iPad11,7", "iPad (8th gen) 10.2″", "iPad 8 10.2\"", 10.2),
        model("iPad12,1 iPad12,2", "iPad (9th gen) 10.2″", "iPad 9 10.2\"", 10.2),
        model("iPad13,18 iPad13,19", "iPad (10th gen) 10.9″", "iPad10 10.9\"", 10.9),
        model("iPad15,7 iPad15,8", "iPad (A16) 11″", "iPad A16 11\"", 11),

        model("iPad4,1 iPad4,2 iPad4,3", "iPad Air 9.7″", "iPad Air 9.7\"", 9.7),
        model("iPad5,3 iPad5,4", "iPad Air 2 9.7″", "Air2 9.7\"", 9.7),
        model("iPad11,3 iPad11,4", "iPad Air (3rd gen) 10.5″", "Air3 10.5\"", 10.5),
        model("iPad13,1 iPad13,2", "iPad Air (4th gen) 10.9″", "Air4 10.9\"", 10.9),
        model("iPad13,16 iPad13,17", "iPad Air (M1) 10.9″", "Air M1 10.9\"", 10.9),
        model("iPad14,8 iPad14,9", "iPad Air (M2) 11″", "Air M2 11\"", 11),
        model("iPad14,10 iPad14,11", "iPad Air (M2) 13″", "Air M2 13\"", 13),
        model("iPad15,3 iPad15,4", "iPad Air (M3) 11″", "Air M3 11\"", 11),
        model("iPad15,5 iPad15,6", "iPad Air (M3) 13″", "Air M3 13\"", 13),
        model("iPad16,8 iPad16,9", "iPad Air (M4) 11″", "Air M4 11\"", 11),
        model("iPad16,10 iPad16,11", "iPad Air (M4) 13″", "Air M4 13\"", 13),

        model("iPad2,5 iPad2,6 iPad2,7", "iPad mini 7.9″", "mini 1 7.9\"", 7.9),
        model("iPad4,4 iPad4,5 iPad4,6", "iPad mini 2 7.9″", "mini 2 7.9\"", 7.9),
        model("iPad4,7 iPad4,8 iPad4,9", "iPad mini 3 7.9″", "mini 3 7.9\"", 7.9),
        model("iPad5,1 iPad5,2", "iPad mini 4 7.9″", "mini 4 7.9\"", 7.9),
        model("iPad11,1 iPad11,2", "iPad mini (5th gen) 7.9″", "mini 5 7.9\"", 7.9),
        model("iPad14,1 iPad14,2", "iPad mini (6th gen) 8.3″", "mini 6 8.3\"", 8.3),
        model("iPad16,1 iPad16,2", "iPad mini (A17 Pro) 8.3″", "mini 7 8.3\"", 8.3),
        model("iPad19,1 iPad19,2", "iPad mini (A20 Pro) 8.5″", "mini 8 8.5\"", 8.5),

        model("iPad6,3 iPad6,4", "iPad Pro 9.7″", "Pro 9.7\"", 9.7),
        model("iPad6,7 iPad6,8", "iPad Pro (1st gen) 12.9″", "Pro12.9 G1", 12.9),
        model("iPad7,1 iPad7,2", "iPad Pro (2nd gen) 12.9″", "Pro12.9 G2", 12.9),
        model("iPad7,3 iPad7,4", "iPad Pro 10.5″", "Pro 10.5\"", 10.5),
        model("iPad8,1 iPad8,2 iPad8,3 iPad8,4", "iPad Pro (1st gen) 11″", "Pro11 G1", 11),
        model("iPad8,5 iPad8,6 iPad8,7 iPad8,8", "iPad Pro (3rd gen) 12.9″", "Pro12.9 G3", 12.9),
        model("iPad8,9 iPad8,10", "iPad Pro (2nd gen) 11″", "Pro11 G2", 11),
        model("iPad8,11 iPad8,12", "iPad Pro (4th gen) 12.9″", "Pro12.9 G4", 12.9),
        model("iPad13,4 iPad13,5 iPad13,6 iPad13,7", "iPad Pro (M1) 11″", "Pro M1 11\"", 11),
        model("iPad13,8 iPad13,9 iPad13,10 iPad13,11", "iPad Pro (M1) 12.9″", "Pro M1 12.9\"", 12.9),
        model("iPad14,3 iPad14,4", "iPad Pro (M2) 11″", "Pro M2 11\"", 11),
        model("iPad14,5 iPad14,6", "iPad Pro (M2) 12.9″", "Pro M2 12.9\"", 12.9),
        model("iPad16,3 iPad16,4", "iPad Pro (M4) 11″", "Pro M4 11\"", 11),
        model("iPad16,5 iPad16,6", "iPad Pro (M4) 13″", "Pro M4 13\"", 13),
        model("iPad17,1 iPad17,2", "iPad Pro (M5) 11″", "Pro M5 11\"", 11),
        model("iPad17,3 iPad17,4", "iPad Pro (M5) 13″", "Pro M5 13\"", 13)
    ]

    /// The panel's width and height in mm (width along `pixels.long`), from the marketing
    /// diagonal and the native pixel shape. Marketing diagonals are rounded, so this can be a
    /// few percent off; the daemon gives the monitor and its touchscreen the same value, so
    /// matching them doesn't depend on it being exact. nil for unknown models.
    func panelSizeMm(nativePixels pixels: (long: Int, short: Int)) -> PanelSize? {
        guard let diagonalInches, pixels.long > 0, pixels.short > 0 else { return nil }
        let long = Double(pixels.long), short = Double(pixels.short)
        let mmPerPixel = diagonalInches * 25.4 / (long * long + short * short).squareRoot()
        return PanelSize(widthMm: Int((long * mmPerPixel).rounded()),
                         heightMm: Int((short * mmPerPixel).rounded()))
    }

    static func lookup(_ identifier: String) -> DeviceDisplayIdentity {
        models.first { $0.identifiers.contains(identifier) }?.identity
            ?? DeviceDisplayIdentity(name: "Screx", edidName: "Screx", diagonalInches: nil)
    }

    static let current: DeviceDisplayIdentity = {
        #if canImport(Darwin)
        var system = utsname()
        guard uname(&system) == 0 else { return lookup("") }
        let capacity = MemoryLayout.size(ofValue: system.machine)
        let identifier = withUnsafePointer(to: &system.machine) {
            $0.withMemoryRebound(to: CChar.self, capacity: capacity) { String(cString: $0) }
        }
        return lookup(identifier)
        #else
        return lookup("")
        #endif
    }()
}

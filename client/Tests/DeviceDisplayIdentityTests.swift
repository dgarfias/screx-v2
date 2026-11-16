import Foundation

// swiftc Screx/DeviceDisplayIdentity.swift Tests/DeviceDisplayIdentityTests.swift -o /tmp/screx-device-tests
@main
struct DeviceDisplayIdentityTests {
    static func main() {
        // Released hardware-ID snapshot from api.ipsw.me/v4/devices, 2026-11-15.
        let expected = Set("""
        iPad1,1 iPad2,1 iPad2,2 iPad2,3 iPad2,4 iPad2,5 iPad2,6 iPad2,7
        iPad3,1 iPad3,2 iPad3,3 iPad3,4 iPad3,5 iPad3,6
        iPad4,1 iPad4,2 iPad4,3 iPad4,4 iPad4,5 iPad4,6 iPad4,7 iPad4,8 iPad4,9
        iPad5,1 iPad5,2 iPad5,3 iPad5,4 iPad6,3 iPad6,4 iPad6,7 iPad6,8 iPad6,11 iPad6,12
        iPad7,1 iPad7,2 iPad7,3 iPad7,4 iPad7,5 iPad7,6 iPad7,11 iPad7,12
        iPad8,1 iPad8,2 iPad8,3 iPad8,4 iPad8,5 iPad8,6 iPad8,7 iPad8,8 iPad8,9 iPad8,10 iPad8,11 iPad8,12
        iPad11,1 iPad11,2 iPad11,3 iPad11,4 iPad11,6 iPad11,7 iPad12,1 iPad12,2
        iPad13,1 iPad13,2 iPad13,4 iPad13,5 iPad13,6 iPad13,7 iPad13,8 iPad13,9 iPad13,10 iPad13,11
        iPad13,16 iPad13,17 iPad13,18 iPad13,19
        iPad14,1 iPad14,2 iPad14,3 iPad14,4 iPad14,5 iPad14,6 iPad14,8 iPad14,9 iPad14,10 iPad14,11
        iPad15,3 iPad15,4 iPad15,5 iPad15,6 iPad15,7 iPad15,8
        iPad16,1 iPad16,2 iPad16,3 iPad16,4 iPad16,5 iPad16,6 iPad16,8 iPad16,9 iPad16,10 iPad16,11
        iPad17,1 iPad17,2 iPad17,3 iPad17,4 iPad19,1 iPad19,2
        """.split(whereSeparator: \.isWhitespace).map(String.init))
        let identifiers = DeviceDisplayIdentity.models.flatMap(\.identifiers)
        precondition(identifiers.count == 107 && Set(identifiers).count == identifiers.count)
        precondition(Set(identifiers) == expected)
        for model in DeviceDisplayIdentity.models {
            precondition(model.identity.diagonalInches! > 0)
            precondition(!model.identity.edidName.isEmpty && model.identity.edidName.utf8.count <= 13)
            precondition(model.identity.edidName.utf8.allSatisfy { (0x20...0x7e).contains($0) && $0 != 39 && $0 != 92 })
            for identifier in model.identifiers {
                precondition(DeviceDisplayIdentity.lookup(identifier) == model.identity)
            }
        }
        precondition(DeviceDisplayIdentity.lookup("iPad15,7").name == "iPad (A16) 11″")
        precondition(DeviceDisplayIdentity.lookup("iPad19,1").diagonalInches == 8.5)
        for unknown in ["iPhone17,1", "Mac15,1", "iPad99,99", "arm64", ""] {
            precondition(DeviceDisplayIdentity.lookup(unknown).name == "Screx")
            precondition(DeviceDisplayIdentity.lookup(unknown).edidName == "Screx")
            precondition(DeviceDisplayIdentity.lookup(unknown).diagonalInches == nil)
        }
        print("Device display identity tests passed: 107 hardware identifiers")
    }
}

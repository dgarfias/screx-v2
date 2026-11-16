import Foundation

/// Message type bytes for the encrypted control channel (TCP) and for client -> daemon
/// UDP payloads. Every message is `type(u8) + body`. Must match `daemon/src/protocol.rs`;
/// see docs/ARCHITECTURE.md "Protocol Details" for the body layouts.
enum ScrexMessage {
    /// Client -> daemon control messages (TCP).
    enum Client {
        static let settings: UInt8 = 0x01
        static let pli: UInt8 = 0x02
        static let ping: UInt8 = 0x03
        static let disconnect: UInt8 = 0x04
        static let touch: UInt8 = 0x10
        static let key: UInt8 = 0x11
        static let rawKey: UInt8 = 0x12
        static let mouse: UInt8 = 0x13
        static let peripheral: UInt8 = 0x14
        static let gamepad: UInt8 = 0x15
        static let speaker: UInt8 = 0x20
        static let mic: UInt8 = 0x21
        static let camera: UInt8 = 0x22
    }

    /// Daemon -> client control messages (TCP).
    enum Daemon {
        static let caps: UInt8 = 0x80
        static let pong: UInt8 = 0x81
    }

    /// Client -> daemon UDP payloads.
    enum Udp {
        static let keepalive: UInt8 = 0x01
        static let camera: UInt8 = 0x02
        static let mic: UInt8 = 0x03
    }
}

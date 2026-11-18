import Foundation
import CryptoKit

// clang -c Screx/CPacePrimitives.c -o /tmp/cpace-primitives.o
// swiftc -import-objc-header Screx/Screx-Bridging-Header.h Screx/CPace.swift \
//   Tests/CPaceTests.swift /tmp/cpace-primitives.o -o /tmp/cpace-tests && /tmp/cpace-tests
@main
struct CPaceTests {
    static func hex(_ text: String) -> Data {
        Data(stride(from: 0, to: text.count, by: 2).map { offset in
            let i = text.index(text.startIndex, offsetBy: offset)
            return UInt8(text[i..<text.index(i, offsetBy: 2)], radix: 16)!
        })
    }
    static func main() throws {
        let sid = hex("7e4b4791d6a8ef019b936c79fb7f2c57")
        let context = CPace.lv([Data("A_initiator".utf8), Data("B_responder".utf8)])
        let password = Data("Password".utf8)
        precondition(CPace.generator(password: password, context: context, sid: sid) ==
            hex("d04bf6d41f6a289632a2e929fa29bebd51092512a7829fdde7d314b62f05a73f"))
        let a = try CPace.Exchange(password: password, context: context, sid: sid,
            scalar: hex("21b4f4bd9e64ed355c3eb676a28ebedaf6d8f17bdc365995b319097153044080"))
        let b = try CPace.Exchange(password: password, context: context, sid: sid,
            scalar: hex("848b0779ff415f0af4ea14df9dd1d3c29ac41d836c7808896c4eba19c51ac40a"))
        precondition(a.share == hex("1d13c89278cdadd826f6d8d7f887701430f8380ddc17611cdd6dc989ce0c9f32"))
        precondition(b.share == hex("248cccf6d5cdc3646f0ad593f9e6cef4e69d4945f8372e623512ecea32185623"))
        let key = try a.finish(peer: b.share, sid: sid, ya: a.share, ada: Data("ADa".utf8),
            yb: b.share, adb: Data("ADb".utf8))
        precondition(key == hex("6e19b875f7a561d6b3ca3dbb9ef42ac55de3e717881018204b8922b4d5e53bb2aa82c300bea7b65d2b671da71922ddf6472301b79bc270adfa8bf413285f2263"))
        print("CPace draft-21 vectors passed")
    }
}

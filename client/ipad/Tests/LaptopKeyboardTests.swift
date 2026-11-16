import Foundation

// swiftc Screx/LaptopKeyboardLayout.swift Tests/LaptopKeyboardTests.swift -o /tmp/screx-laptop-tests
@main
struct LaptopKeyboardTests {
    static func main() {
        for id in LaptopKeyboardLayoutID.allCases {
            let layout = LaptopKeyboardLayout.make(id)
            precondition(layout.rows.count == 6, "\(id): function row + 4 rows + bottom row")
            for (index, row) in layout.rows.enumerated() {
                let units = row.reduce(0) { $0 + $1.units }
                precondition(abs(units - LaptopKeyboardLayout.rowUnits) < 0.001, "\(id) row \(index) is \(units) units")
            }
            let functionCodes = layout.rows[0].compactMap { cell -> UInt8? in
                if case let .special(_, code, _) = cell.key { return code }
                return nil
            }
            precondition(functionCodes == [0x04] + (0x10...0x1B).map(UInt8.init) + [0x0A, 0x0B, 0x0F, 0x09],
                         "esc, F1–F12, home, end, ins, del")
        }

        // Detection follows the enabled iPad keyboards, in order, then the device language.
        precondition(LaptopKeyboardLayoutID.detect(keyboardLanguages: ["es-MX", "en-US"], deviceLanguage: "en") == [.es, .us])
        precondition(LaptopKeyboardLayoutID.detect(keyboardLanguages: ["fr_CA", "fr_FR"], deviceLanguage: nil) == [.fr])
        precondition(LaptopKeyboardLayoutID.detect(keyboardLanguages: ["ja-JP", "emoji"], deviceLanguage: "de-CH") == [.de])
        precondition(LaptopKeyboardLayoutID.detect(keyboardLanguages: [], deviceLanguage: "zh-Hans") == [.us])

        // Layout-specific keys.
        func characters(_ id: LaptopKeyboardLayoutID) -> [String] {
            LaptopKeyboardLayout.make(id).rows.flatMap { $0 }.compactMap {
                if case let .character(base, _, _, _) = $0.key { return base }
                return nil
            }
        }
        precondition(characters(.es).contains("ñ"))
        precondition(characters(.fr).starts(with: ["²", "&", "é"]) && characters(.fr).contains("a"))
        precondition(characters(.de).contains("ß") && characters(.de).contains("z"))
        let esAltGr = LaptopKeyboardLayout.make(.es).rows.flatMap { $0 }.contains {
            if case let .character(base, _, altGr, _) = $0.key { return base == "2" && altGr == "@" }
            return false
        }
        precondition(esAltGr, "Spanish @ is AltGr+2")

        // Dead keys.
        precondition(DeadKeyComposer.compose(dead: "´", next: "e") == "é")
        precondition(DeadKeyComposer.compose(dead: "´", next: "E") == "É")
        precondition(DeadKeyComposer.compose(dead: "^", next: "o") == "ô")
        precondition(DeadKeyComposer.compose(dead: "¨", next: "u") == "ü")
        precondition(DeadKeyComposer.compose(dead: "`", next: "a") == "à")
        precondition(DeadKeyComposer.compose(dead: "~", next: "n") == "ñ")
        precondition(DeadKeyComposer.compose(dead: "´", next: " ") == "´", "Space sends the accent itself")
        precondition(DeadKeyComposer.compose(dead: "´", next: "x") == "´x", "No composed form: send both")
        print("Laptop keyboard tests passed")
    }
}

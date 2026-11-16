import Foundation

/// Pure description of the hand-drawn laptop keyboard. No UIKit, so layouts and composition are
/// testable on their own. Characters are sent as text, exactly like the iOS keyboard path; the
/// drawn layout only decides which character each key produces.
enum LaptopKeyboardLayoutID: String, CaseIterable {
    case us, es, fr, de

    var shortName: String {
        switch self {
        case .us: return "US"
        case .es: return "ES"
        case .fr: return "FR"
        case .de: return "DE"
        }
    }

    /// Map an iPad keyboard/locale language (`en_US`, `es-MX`, `fr_CA`, `de_CH`…) to a layout.
    static func forLanguage(_ language: String) -> LaptopKeyboardLayoutID? {
        let code = language.lowercased().split(whereSeparator: { $0 == "_" || $0 == "-" }).first.map(String.init)
        switch code {
        case "en": return .us
        case "es", "ca", "gl", "eu": return .es
        case "fr": return .fr
        case "de": return .de
        default: return nil
        }
    }

    /// Supported layouts for the user's enabled keyboards, in their Settings order. Falls back to
    /// the device language, then US, so the list is never empty.
    static func detect(keyboardLanguages: [String], deviceLanguage: String?) -> [LaptopKeyboardLayoutID] {
        var result: [LaptopKeyboardLayoutID] = []
        for language in keyboardLanguages {
            if let id = forLanguage(language), !result.contains(id) { result.append(id) }
        }
        if result.isEmpty, let deviceLanguage, let id = forLanguage(deviceLanguage) { result.append(id) }
        return result.isEmpty ? [.us] : result
    }
}

enum LaptopKey: Equatable {
    /// A character key. `dead` marks legends that compose with the next character (´ ` ^ ¨ ~).
    case character(base: String, shifted: String, altGr: String? = nil, dead: Set<String> = [])
    case special(label: String, code: UInt8, repeats: Bool = false)
    case modifier(label: String, mask: UInt8)
    case shift, capsLock, altGr, layout, space, backspace, enter, tab

    static let ctrl: UInt8 = 0x01
    static let alt: UInt8 = 0x02
    static let superKey: UInt8 = 0x04
}

struct LaptopKeyCell: Equatable {
    let key: LaptopKey
    /// Width in key units; every row sums to `LaptopKeyboardLayout.rowUnits`.
    let units: Double
}

struct LaptopKeyboardLayout {
    static let rowUnits = 15.0
    let id: LaptopKeyboardLayoutID
    let rows: [[LaptopKeyCell]]

    private static func c(_ base: String, _ shifted: String, _ altGr: String? = nil,
                          dead: Set<String> = [], _ units: Double = 1) -> LaptopKeyCell {
        LaptopKeyCell(key: .character(base: base, shifted: shifted, altGr: altGr, dead: dead), units: units)
    }

    private static func letters(_ s: String) -> [LaptopKeyCell] {
        s.map { c(String($0), String($0).uppercased()) }
    }

    /// esc, F1–F12, home, end, ins, del: 17 equal keys across the 15-unit row.
    private static let functionRow: [LaptopKeyCell] = {
        let keys: [LaptopKey] = [.special(label: "esc", code: 0x04)]
            + (1...12).map { .special(label: "F\($0)", code: UInt8(0x0F + $0)) }
            + [.special(label: "home", code: 0x0A), .special(label: "end", code: 0x0B),
               .special(label: "ins", code: 0x0F), .special(label: "del", code: 0x09, repeats: true)]
        return keys.map { LaptopKeyCell(key: $0, units: rowUnits / Double(keys.count)) }
    }()

    private static let bottomRow: [LaptopKeyCell] = [
        LaptopKeyCell(key: .modifier(label: "ctrl", mask: LaptopKey.ctrl), units: 1.25),
        LaptopKeyCell(key: .layout, units: 1),
        LaptopKeyCell(key: .modifier(label: "super", mask: LaptopKey.superKey), units: 1.25),
        LaptopKeyCell(key: .modifier(label: "alt", mask: LaptopKey.alt), units: 1.25),
        LaptopKeyCell(key: .space, units: 5.25),
        LaptopKeyCell(key: .altGr, units: 1),
        LaptopKeyCell(key: .special(label: "←", code: 0x05, repeats: true), units: 1),
        LaptopKeyCell(key: .special(label: "↑", code: 0x07, repeats: true), units: 1),
        LaptopKeyCell(key: .special(label: "↓", code: 0x08, repeats: true), units: 1),
        LaptopKeyCell(key: .special(label: "→", code: 0x06, repeats: true), units: 1),
    ]

    private static func cell(_ key: LaptopKey, _ units: Double) -> LaptopKeyCell {
        LaptopKeyCell(key: key, units: units)
    }

    static func make(_ id: LaptopKeyboardLayoutID) -> LaptopKeyboardLayout {
        let rows: [[LaptopKeyCell]]
        switch id {
        case .us:
            rows = [
                functionRow,
                [c("`", "~"), c("1", "!"), c("2", "@"), c("3", "#"), c("4", "$"), c("5", "%"), c("6", "^"),
                 c("7", "&"), c("8", "*"), c("9", "("), c("0", ")"), c("-", "_"), c("=", "+"), cell(.backspace, 2)],
                [cell(.tab, 1.5)] + letters("qwertyuiop") + [c("[", "{"), c("]", "}"), c("\\", "|", nil, 1.5)],
                [cell(.capsLock, 1.75)] + letters("asdfghjkl") + [c(";", ":"), c("'", "\""), cell(.enter, 2.25)],
                [cell(.shift, 2.25)] + letters("zxcvbnm") + [c(",", "<"), c(".", ">"), c("/", "?"), cell(.shift, 2.75)],
                bottomRow,
            ]
        case .es:
            rows = [
                functionRow,
                [c("º", "ª", "\\"), c("1", "!", "|"), c("2", "\"", "@"), c("3", "·", "#"), c("4", "$", "~"),
                 c("5", "%", "€"), c("6", "&", "¬"), c("7", "/"), c("8", "("), c("9", ")"), c("0", "="),
                 c("'", "?"), c("¡", "¿"), cell(.backspace, 2)],
                [cell(.tab, 1.5)] + letters("qwertyuiop")
                    + [c("`", "^", "[", dead: ["`", "^"]), c("+", "*", "]"), c("ç", "Ç", "}", dead: [], 1.5)],
                [cell(.capsLock, 1.75)] + letters("asdfghjkl")
                    + [c("ñ", "Ñ"), c("´", "¨", "{", dead: ["´", "¨"]), cell(.enter, 2.25)],
                [cell(.shift, 1.25), c("<", ">")] + letters("zxcvbnm")
                    + [c(",", ";"), c(".", ":"), c("-", "_"), cell(.shift, 2.75)],
                bottomRow,
            ]
        case .fr:
            rows = [
                functionRow,
                [c("²", "³"), c("&", "1"), c("é", "2", "~"), c("\"", "3", "#"), c("'", "4", "{"), c("(", "5", "["),
                 c("-", "6", "|"), c("è", "7", "`"), c("_", "8", "\\"), c("ç", "9", "^"), c("à", "0", "@"),
                 c(")", "°", "]"), c("=", "+", "}"), cell(.backspace, 2)],
                [cell(.tab, 1.5), c("a", "A"), c("z", "Z"), c("e", "E", "€")] + letters("rtyuiop")
                    + [c("^", "¨", dead: ["^", "¨"]), c("$", "£", "¤"), c("*", "µ", nil, 1.5)],
                [cell(.capsLock, 1.75)] + letters("qsdfghjklm") + [c("ù", "%"), cell(.enter, 2.25)],
                [cell(.shift, 1.25), c("<", ">")] + letters("wxcvbn")
                    + [c(",", "?"), c(";", "."), c(":", "/"), c("!", "§"), cell(.shift, 2.75)],
                bottomRow,
            ]
        case .de:
            rows = [
                functionRow,
                [c("^", "°", dead: ["^"]), c("1", "!"), c("2", "\"", "²"), c("3", "§", "³"), c("4", "$"),
                 c("5", "%"), c("6", "&"), c("7", "/", "{"), c("8", "(", "["), c("9", ")", "]"), c("0", "=", "}"),
                 c("ß", "?", "\\"), c("´", "`", dead: ["´", "`"]), cell(.backspace, 2)],
                [cell(.tab, 1.5), c("q", "Q", "@"), c("w", "W"), c("e", "E", "€")] + letters("rtzuiop")
                    + [c("ü", "Ü"), c("+", "*", "~"), c("#", "'", nil, 1.5)],
                [cell(.capsLock, 1.75)] + letters("asdfghjkl") + [c("ö", "Ö"), c("ä", "Ä"), cell(.enter, 2.25)],
                [cell(.shift, 1.25), c("<", ">", "|")] + letters("yxcvbn")
                    + [c("m", "M", "µ"), c(",", ";"), c(".", ":"), c("-", "_"), cell(.shift, 2.75)],
                bottomRow,
            ]
        }
        return LaptopKeyboardLayout(id: id, rows: rows)
    }
}

/// Dead-key composition, using Unicode canonical composition rather than hand-written tables.
enum DeadKeyComposer {
    private static let combining: [String: String] = [
        "´": "\u{0301}", "`": "\u{0300}", "^": "\u{0302}", "¨": "\u{0308}", "~": "\u{0303}",
    ]

    /// The text to send for `dead` followed by `next`. Unsupported pairs send both characters,
    /// and a space after a dead key sends the accent itself, as on a desktop keyboard.
    static func compose(dead: String, next: String) -> String {
        if next == " " { return dead }
        guard let mark = combining[dead], next.unicodeScalars.count == 1 else { return dead + next }
        let composed = (next + mark).precomposedStringWithCanonicalMapping
        return composed.unicodeScalars.count == 1 ? composed : dead + next
    }
}

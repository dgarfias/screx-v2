import SwiftUI
import UIKit

/// Hand-drawn laptop keyboard used by Laptop mode. Sends the same KEY messages as the iOS
/// keyboard path: text for characters, special codes for editing/navigation keys, and
/// modifier combos for Ctrl/Alt/Super.
struct LaptopKeyboardView: UIViewRepresentable {
    let onText: (String) -> Void
    let onSpecial: (UInt8) -> Void
    let onComboText: (UInt8, String) -> Void
    let onComboSpecial: (UInt8, UInt8) -> Void

    private static let storedLayoutKey = "screx_laptop_keyboard_layout"

    /// Detected layouts first (enabled iPad keyboards, in Settings order), then the rest, so the
    /// layout key can still reach every layout.
    static func layoutOrder() -> [LaptopKeyboardLayoutID] {
        let detected = LaptopKeyboardLayoutID.detect(
            keyboardLanguages: UITextInputMode.activeInputModes.compactMap(\.primaryLanguage),
            deviceLanguage: Locale.preferredLanguages.first
        )
        return detected + LaptopKeyboardLayoutID.allCases.filter { !detected.contains($0) }
    }

    func makeUIView(context: Context) -> LaptopKeyboardSurface {
        let order = Self.layoutOrder()
        let stored = UserDefaults.standard.string(forKey: Self.storedLayoutKey)
            .flatMap(LaptopKeyboardLayoutID.init(rawValue:))
        let view = LaptopKeyboardSurface(layouts: order, initial: stored ?? order[0])
        view.onLayoutChanged = { UserDefaults.standard.set($0.rawValue, forKey: Self.storedLayoutKey) }
        update(view)
        return view
    }

    func updateUIView(_ view: LaptopKeyboardSurface, context: Context) { update(view) }

    static func dismantleUIView(_ view: LaptopKeyboardSurface, coordinator: ()) {
        view.cancelAllTouches()
    }

    private func update(_ view: LaptopKeyboardSurface) {
        view.onText = onText
        view.onSpecial = onSpecial
        view.onComboText = onComboText
        view.onComboSpecial = onComboSpecial
    }
}

final class LaptopKeyboardSurface: UIView {
    var onText: ((String) -> Void)?
    var onSpecial: ((UInt8) -> Void)?
    var onComboText: ((UInt8, String) -> Void)?
    var onComboSpecial: ((UInt8, UInt8) -> Void)?
    var onLayoutChanged: ((LaptopKeyboardLayoutID) -> Void)?

    private let layouts: [LaptopKeyboardLayoutID]
    private var layout: LaptopKeyboardLayout

    private final class KeyView: UIView {
        let cell: LaptopKeyCell
        let label = UILabel()
        let corner = UILabel()
        init(cell: LaptopKeyCell) {
            self.cell = cell
            super.init(frame: .zero)
            layer.cornerRadius = 6
            layer.cornerCurve = .continuous
            isUserInteractionEnabled = false
            isAccessibilityElement = true
            accessibilityTraits = .keyboardKey
            label.textAlignment = .center
            label.textColor = .white
            label.adjustsFontSizeToFitWidth = true
            label.minimumScaleFactor = 0.6
            corner.textColor = UIColor(white: 1, alpha: 0.55)
            corner.textAlignment = .right
            addSubview(label)
            addSubview(corner)
        }
        required init?(coder: NSCoder) { fatalError() }
    }

    private var keyViews: [KeyView] = []
    private var touchKeys: [ObjectIdentifier: KeyView] = [:]
    private var repeatTimers: [ObjectIdentifier: Timer] = [:]

    /// Physical-keyboard semantics: Shift, AltGr, Ctrl, Alt and Super are active only while a
    /// finger holds them. A held modifier released without pressing another key is sent alone
    /// (e.g. Super opens the GNOME overview), like tapping it on a real keyboard.
    private struct Hold {
        let key: LaptopKey
        var used = false
    }
    private var holds: [ObjectIdentifier: Hold] = [:]
    private var capsLock = false
    private var pendingDead: String?

    private var shift: Bool { holds.values.contains { $0.key == .shift } }
    private var altGr: Bool { holds.values.contains { $0.key == .altGr } }
    private var modifiers: UInt8 {
        holds.values.reduce(0) { mask, hold in
            if case let .modifier(_, m) = hold.key { return mask | m }
            return mask
        }
    }
    private static let shiftMask: UInt8 = 0x08

    private static func isHoldKey(_ key: LaptopKey) -> Bool {
        switch key {
        case .shift, .altGr, .modifier: return true
        default: return false
        }
    }

    private static let loneModifierCodes: [UInt8: UInt8] = [
        LaptopKey.ctrl: 0x0C, LaptopKey.alt: 0x0D, LaptopKey.superKey: 0x0E,
    ]

    init(layouts: [LaptopKeyboardLayoutID], initial: LaptopKeyboardLayoutID) {
        self.layouts = layouts
        self.layout = LaptopKeyboardLayout.make(initial)
        super.init(frame: .zero)
        isMultipleTouchEnabled = true
        backgroundColor = .clear
        rebuildKeys()
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    // MARK: - Layout

    private func rebuildKeys() {
        keyViews.forEach { $0.removeFromSuperview() }
        keyViews = layout.rows.flatMap { $0 }.map(KeyView.init(cell:))
        keyViews.forEach(addSubview)
        setNeedsLayout()
        refreshAppearance()
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let gap: CGFloat = 6
        let rowHeight = bounds.height / CGFloat(layout.rows.count)
        let unit = bounds.width / CGFloat(LaptopKeyboardLayout.rowUnits)
        let fontSize = min(rowHeight * 0.36, 24)
        var index = 0
        for (r, row) in layout.rows.enumerated() {
            var x: CGFloat = 0
            for cell in row {
                let width = unit * CGFloat(cell.units)
                let key = keyViews[index]
                key.frame = CGRect(x: x, y: CGFloat(r) * rowHeight, width: width, height: rowHeight)
                    .insetBy(dx: gap / 2, dy: gap / 2)
                let isCharacter: Bool = { if case .character = cell.key { return true }; return false }()
                key.label.font = .systemFont(ofSize: isCharacter ? fontSize : fontSize * 0.72,
                                             weight: isCharacter ? .regular : .medium)
                key.label.frame = key.bounds.insetBy(dx: 4, dy: 2)
                key.corner.font = .systemFont(ofSize: fontSize * 0.55)
                key.corner.frame = CGRect(x: 4, y: 3, width: key.bounds.width - 8, height: fontSize * 0.7)
                x += width
                index += 1
            }
        }
    }

    // MARK: - Appearance

    private func characterLevel(base: String, shifted: String, altGrChar: String?, altGrShifted: String?) -> String {
        if altGr, let altGrChar {
            // Caps Lock capitalizes AltGr letters too (é → É), like Shift does.
            let isLetter = altGrChar.lowercased() != altGrChar.uppercased()
            if let altGrShifted, isLetter ? shift != capsLock : shift { return altGrShifted }
            return altGrChar
        }
        let isLetter = base.lowercased() != base.uppercased()
        return (isLetter ? shift != capsLock : shift) ? shifted : base
    }

    private func refreshAppearance() {
        for key in keyViews {
            var active = false
            var isCharacter = false
            key.accessibilityLabel = nil
            switch key.cell.key {
            case let .character(base, shifted, altGrChar, altGrShifted, dead):
                isCharacter = true
                let text = characterLevel(base: base, shifted: shifted, altGrChar: altGrChar, altGrShifted: altGrShifted)
                key.label.text = text
                key.corner.text = altGr ? nil : altGrChar
                active = pendingDead != nil && dead.contains(pendingDead!) && (text == pendingDead)
                key.accessibilityLabel = text
            case let .special(label, _, _):
                key.label.text = label
            case let .modifier(label, mask):
                key.label.text = label
                active = modifiers & mask != 0
            case .shift:
                key.label.text = capsLock ? "⇪" : "⇧"
                active = shift
            case .capsLock:
                key.label.text = "caps"
                active = capsLock
            case .altGr:
                key.label.text = "alt gr"
                active = altGr
            case .layout:
                key.label.text = "🌐 " + layout.id.shortName
                key.accessibilityLabel = "Keyboard layout, \(layout.id.shortName)"
            case .space:
                key.label.text = nil
                key.accessibilityLabel = "space"
            case .backspace:
                key.label.text = "⌫"
                key.accessibilityLabel = "delete"
            case .enter:
                key.label.text = "return"
            case .tab:
                key.label.text = "tab"
            }
            if !isCharacter { key.corner.text = nil }
            if key.accessibilityLabel == nil { key.accessibilityLabel = key.label.text }
            let pressed = touchKeys.values.contains { $0 === key }
            key.backgroundColor = active
                ? UIColor.systemBlue
                : UIColor(white: pressed ? 0.42 : (isCharacter ? 0.24 : 0.16), alpha: 1)
            key.accessibilityTraits = active ? [.keyboardKey, .selected] : .keyboardKey
        }
    }

    // MARK: - Touches

    private func isFingerTouch(_ touch: UITouch) -> Bool {
        touch.type == .direct || touch.type == .pencil
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        for touch in touches where isFingerTouch(touch) {
            let point = touch.location(in: self)
            guard let key = keyViews.first(where: { $0.frame.insetBy(dx: -2.5, dy: -2.5).contains(point) }) else { continue }
            let id = ObjectIdentifier(touch)
            touchKeys[id] = key
            if Self.isHoldKey(key.cell.key) {
                holds[id] = Hold(key: key.cell.key)
            } else {
                // Any other key pressed while a modifier is held makes it a chord, not a lone tap.
                for holdID in holds.keys { holds[holdID]?.used = true }
                // Fire on touch-down so fast, overlapping keystrokes are never lost.
                press(key.cell.key)
                if repeats(key.cell.key) { startRepeat(for: id, key: key.cell.key) }
            }
        }
        refreshAppearance()
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) { release(touches, cancelled: false) }
    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) { release(touches, cancelled: true) }

    private func release(_ touches: Set<UITouch>, cancelled: Bool) {
        for touch in touches {
            let id = ObjectIdentifier(touch)
            touchKeys.removeValue(forKey: id)
            repeatTimers.removeValue(forKey: id)?.invalidate()
            if let hold = holds.removeValue(forKey: id), !hold.used, !cancelled,
               case let .modifier(_, mask) = hold.key, let code = Self.loneModifierCodes[mask] {
                // Only if no other finger still holds the same modifier.
                if modifiers & mask == 0 { onSpecial?(code) }
            }
        }
        refreshAppearance()
    }

    func cancelAllTouches() {
        repeatTimers.values.forEach { $0.invalidate() }
        repeatTimers.removeAll()
        touchKeys.removeAll()
        holds.removeAll()
    }

    private func repeats(_ key: LaptopKey) -> Bool {
        switch key {
        case .backspace: return true
        case let .special(_, _, repeats): return repeats
        default: return false
        }
    }

    private func startRepeat(for id: ObjectIdentifier, key: LaptopKey) {
        let timer = Timer(timeInterval: 0.45, repeats: false) { [weak self] _ in
            guard let self, self.touchKeys[id] != nil else { return }
            let fast = Timer(timeInterval: 0.06, repeats: true) { [weak self] timer in
                guard let self, self.touchKeys[id] != nil else { timer.invalidate(); return }
                self.press(key)
            }
            RunLoop.main.add(fast, forMode: .common)
            self.repeatTimers[id] = fast
        }
        RunLoop.main.add(timer, forMode: .common)
        repeatTimers[id] = timer
    }

    // MARK: - Key actions

    private func press(_ key: LaptopKey) {
        switch key {
        case let .character(base, shifted, altGrChar, altGrShifted, dead):
            // The character already reflects Shift/AltGr, so only Ctrl/Alt/Super form a chord.
            let text = characterLevel(base: base, shifted: shifted, altGrChar: altGrChar, altGrShifted: altGrShifted)
            if modifiers != 0 {
                flushDeadKey()
                onComboText?(modifiers, text)
            } else if dead.contains(text), pendingDead == nil {
                pendingDead = text
            } else if let dead = pendingDead {
                pendingDead = nil
                onText?(DeadKeyComposer.compose(dead: dead, next: text))
            } else {
                onText?(text)
            }
        case let .special(_, code, _):
            sendSpecial(code)
        case .backspace:
            if pendingDead != nil { pendingDead = nil } else { sendSpecial(0x01) }
        case .enter:
            sendSpecial(0x02)
        case .tab:
            sendSpecial(0x03)
        case .space:
            if let dead = pendingDead {
                pendingDead = nil
                onText?(DeadKeyComposer.compose(dead: dead, next: " "))
            } else if modifiers != 0 {
                onComboText?(modifiers, " ")
            } else {
                onText?(" ")
            }
        case .capsLock:
            capsLock.toggle()
        case .layout:
            let next = layouts[((layouts.firstIndex(of: layout.id) ?? -1) + 1) % layouts.count]
            layout = LaptopKeyboardLayout.make(next)
            pendingDead = nil
            onLayoutChanged?(next)
            rebuildKeys()
        case .shift, .altGr, .modifier:
            break // Held keys: state comes from `holds`.
        }
        refreshAppearance()
    }

    /// Editing/navigation keys carry every held modifier, including Shift (Shift+arrows selects).
    private func sendSpecial(_ code: UInt8) {
        flushDeadKey()
        let mask = modifiers | (shift ? Self.shiftMask : 0)
        if mask != 0 {
            onComboSpecial?(mask, code)
        } else {
            onSpecial?(code)
        }
    }

    private func flushDeadKey() {
        if let dead = pendingDead {
            pendingDead = nil
            onText?(dead)
        }
    }
}

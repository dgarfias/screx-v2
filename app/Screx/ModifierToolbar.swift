import SwiftUI
import Combine

enum ModifierKey: String, CaseIterable, Identifiable {
    case ctrl = "Ctrl"
    case alt = "Alt"
    case cmd = "Super"
    case shift = "Shift"

    var id: String { rawValue }

    var protocolId: UInt8 {
        switch self {
        case .ctrl:  return 0x01
        case .alt:   return 0x02
        case .cmd:   return 0x03
        case .shift: return 0x04
        }
    }
}

enum ModifierState {
    case inactive
    case oneShot
    case locked
}

@MainActor
final class ModifierBarState: ObservableObject {
    @Published var states: [ModifierKey: ModifierState] = [:]

    var onModifier: ((UInt8, Bool) -> Void)?
    var onSpecialKey: ((UInt8) -> Void)?

    func toggle(_ key: ModifierKey) {
        let current = states[key] ?? .inactive
        switch current {
        case .inactive:
            states[key] = .oneShot
        case .oneShot:
            states[key] = .locked
            onModifier?(key.protocolId, true)
        case .locked:
            states[key] = .inactive
            onModifier?(key.protocolId, false)
        }
    }

    func state(for key: ModifierKey) -> ModifierState {
        states[key] ?? .inactive
    }

    func pressOneShotModifiers() -> [ModifierKey] {
        var pressed: [ModifierKey] = []
        for key in ModifierKey.allCases {
            if states[key] == .oneShot {
                onModifier?(key.protocolId, true)
                pressed.append(key)
            }
        }
        return pressed
    }

    func releaseOneShotModifiers(_ keys: [ModifierKey]) {
        for key in keys {
            onModifier?(key.protocolId, false)
            states[key] = .inactive
        }
    }

    func releaseAll() {
        for key in ModifierKey.allCases {
            let s = states[key] ?? .inactive
            if s != .inactive {
                onModifier?(key.protocolId, false)
            }
        }
        states = [:]
    }
}

struct ModifierToolbar: View {
    @ObservedObject var bar: ModifierBarState

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 3) {
                modButton("Esc") { bar.onSpecialKey?(0x04) }
                modButton("Tab") { bar.onSpecialKey?(0x03) }

                Divider().frame(height: 22).opacity(0.4)

                ForEach(ModifierKey.allCases) { key in
                    modKeyButton(key)
                }

                Divider().frame(height: 22).opacity(0.4)

                ForEach(1...12, id: \.self) { n in
                    modButton("F\(n)") { bar.onSpecialKey?(UInt8(0x0F + n)) }
                }

                Divider().frame(height: 22).opacity(0.4)

                modButton("PgUp") { bar.onSpecialKey?(0x1C) }
                modButton("PgDn") { bar.onSpecialKey?(0x1D) }
                modButton("Home") { bar.onSpecialKey?(0x0A) }
                modButton("End")  { bar.onSpecialKey?(0x0B) }
            }
            .padding(.horizontal, 8)
        }
        .frame(height: 36)
    }

    @ViewBuilder
    private func modKeyButton(_ key: ModifierKey) -> some View {
        let s = bar.state(for: key)
        Button { bar.toggle(key) } label: {
            Text(key.rawValue)
                .font(.system(size: 12, weight: .medium, design: .monospaced))
                .padding(.horizontal, 8)
                .padding(.vertical, 5)
                .background(background(for: s), in: RoundedRectangle(cornerRadius: 5))
                .foregroundStyle(foreground(for: s))
        }
        .buttonStyle(.plain)
        .overlay(alignment: .topTrailing) {
            if s == .locked {
                Circle()
                    .fill(.yellow)
                    .frame(width: 5, height: 5)
                    .offset(x: 2, y: -2)
            }
        }
    }

    private func modButton(_ label: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Text(label)
                .font(.system(size: 12, weight: .medium, design: .monospaced))
                .padding(.horizontal, 8)
                .padding(.vertical, 5)
        }
        .buttonStyle(.plain)
        .foregroundStyle(.white)
    }

    private func background(for state: ModifierState) -> Color {
        switch state {
        case .inactive: return .clear
        case .oneShot:  return .white.opacity(0.25)
        case .locked:   return .white.opacity(0.4)
        }
    }

    private func foreground(for state: ModifierState) -> Color {
        switch state {
        case .inactive: return .white
        case .oneShot:  return .yellow
        case .locked:   return .yellow
        }
    }
}

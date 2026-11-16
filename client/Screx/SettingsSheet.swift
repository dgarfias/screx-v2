import SwiftUI

/// One host's settings: its address and the stream it asks for. It doesn't observe
/// `StreamViewModel`: the model republishes about every second, and each re-render would rebuild
/// an open picker menu and snap its scroll position to the top. Everything is seeded once when
/// the sheet opens and saved when it closes.
struct HostSettingsSheet: View, Equatable {
    @Environment(\.dismiss) private var dismiss

    private let tileId: String
    private let hostName: String
    private let initialAddress: String
    private let initialSettings: HostStreamSettings
    /// What the host advertised on its last connection; nil until it has connected once.
    private let limits: HostLimits?
    private let addressError: (String) -> String?
    private let onSave: (HostStreamSettings, String) -> Void

    @State private var address: String
    @State private var settings: HostStreamSettings
    @State private var customBitrateText: String
    @State private var shownAddressError: String?

    static func == (lhs: HostSettingsSheet, rhs: HostSettingsSheet) -> Bool {
        lhs.tileId == rhs.tileId && lhs.hostName == rhs.hostName && lhs.initialAddress == rhs.initialAddress
            && lhs.initialSettings == rhs.initialSettings && lhs.limits == rhs.limits
    }

    init(tileId: String, hostName: String, address: String, settings: HostStreamSettings, limits: HostLimits?,
         addressError: @escaping (String) -> String?, onSave: @escaping (HostStreamSettings, String) -> Void) {
        self.tileId = tileId
        self.hostName = hostName
        initialAddress = address
        initialSettings = settings
        self.limits = limits
        self.addressError = addressError
        self.onSave = onSave
        _address = State(initialValue: address)
        _settings = State(initialValue: settings)
        _customBitrateText = State(initialValue: Self.formatMbps(settings.customBitrateMbps))
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("Host or IP address", text: $address)
                        .keyboardType(.URL)
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                        .onChange(of: address) { _ in shownAddressError = nil }
                } header: {
                    Text("Address")
                } footer: {
                    if let shownAddressError {
                        Text(shownAddressError).foregroundStyle(.red)
                    }
                }

                Section {
                    Picker("Resolution", selection: $settings.resolution) {
                        ForEach(options(StreamResolutionPreset.pickerCases, selected: settings.resolution) {
                            limits?.supports($0) ?? true
                        }) { Text($0.label).tag($0) }
                    }
                    Picker("Frame rate", selection: $settings.framerate) {
                        ForEach(options(StreamFrameratePreset.allCases, selected: settings.framerate) {
                            limits?.supports($0) ?? true
                        }) { Text($0.label).tag($0) }
                    }
                } header: {
                    Text("Video")
                } footer: {
                    warning(unsupportedVideo)
                }

                Section {
                    Picker("Codec", selection: $settings.codec) {
                        ForEach(options(StreamCodecPreset.allCases, selected: settings.codec) {
                            limits?.supports($0) ?? true
                        }) { Text($0.label).tag($0) }
                    }
                    Picker("Bitrate", selection: $settings.bitrate) {
                        ForEach(options(StreamBitratePreset.allCases, selected: settings.bitrate) {
                            $0 == .custom || $0 == .daemonDefault || (limits?.supports(bitrateBps: $0.bitrateBps) ?? true)
                        }) { Text($0.label).tag($0) }
                    }
                    if settings.bitrate == .custom {
                        HStack {
                            Text("Custom bitrate")
                            Spacer()
                            TextField("10", text: $customBitrateText)
                                .keyboardType(.decimalPad)
                                .multilineTextAlignment(.trailing)
                                .frame(width: 80)
                            Text("Mbps").foregroundStyle(.secondary)
                        }
                        .onChange(of: customBitrateText) { _ in
                            if let value = customBitrateMbps { settings.customBitrateMbps = value }
                        }
                    }
                } header: {
                    Text("Encoding")
                } footer: {
                    VStack(alignment: .leading, spacing: 6) {
                        warning(unsupportedEncoding)
                        Text("Higher bitrates look sharper but need a stronger Wi-Fi connection.")
                    }
                }
            }
            .navigationTitle(hostName)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") {
                        if let error = addressError(address) {
                            shownAddressError = error
                        } else {
                            dismiss()
                        }
                    }
                }
            }
        }
        // Swiping the sheet away also saves; an invalid address is the only thing dropped.
        .onDisappear { onSave(settings, address) }
    }

    /// The options the host supports, plus the current choice if it isn't one of them, so the
    /// picker can still show it (with a warning below).
    private func options<T: Equatable>(_ all: [T], selected: T, supported: (T) -> Bool) -> [T] {
        all.filter { $0 == selected || supported($0) }
    }

    @ViewBuilder
    private func warning(_ problems: [String]) -> some View {
        if !problems.isEmpty {
            Label("\(hostName) \(problems.joined(separator: " and "))", systemImage: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
        }
    }

    private var unsupportedVideo: [String] {
        guard let limits else { return [] }
        var problems: [String] = []
        if !limits.supports(settings.resolution) {
            problems.append("supports up to \(limits.maxWidth)×\(limits.maxHeight)")
        }
        if !limits.supports(settings.framerate) {
            problems.append("supports up to \(limits.maxFps) fps")
        }
        return problems
    }

    private var unsupportedEncoding: [String] {
        guard let limits else { return [] }
        var problems: [String] = []
        if !limits.supports(settings.codec) {
            problems.append("doesn’t support \(settings.codec.label)")
        }
        if let bps = settings.bitrateBps, !limits.supports(bitrateBps: bps) {
            problems.append(bps > limits.bitrateMaxBps
                ? "supports up to \(Self.formatMbps(Double(limits.bitrateMaxBps) / 1_000_000)) Mbps"
                : "needs at least \(Self.formatMbps(Double(limits.bitrateMinBps) / 1_000_000)) Mbps")
        }
        return problems
    }

    private var customBitrateMbps: Double? {
        guard let value = Double(customBitrateText.replacingOccurrences(of: ",", with: ".")),
              value.isFinite, value > 0 else { return nil }
        return value
    }

    private static func formatMbps(_ mbps: Double) -> String {
        String(format: "%g", (mbps * 10).rounded() / 10)
    }
}

import SwiftUI

struct DisplaySettingsView: View {
    @ObservedObject var model: StreamViewModel
    @Environment(\.dismiss) private var dismiss
    @State private var mode: DisplayOutputMode
    @State private var resolution: DisplayResolution
    @State private var frameRate: Int

    init(model: StreamViewModel) {
        self.model = model
        let settings = model.currentDisplayConfiguration
        _mode = State(initialValue: model.selectedDisplayMode)
        let stored = model.displayPreferences(forHost: model.currentHostKey)
        let available = model.availableExternalResolutions
        let current = DisplayResolution(width: settings?.externalWidth ?? 1920,
                                        height: settings?.externalHeight ?? 1080)
        let remembered = DisplayPreferenceRules.resolution(stored: stored, available: available,
                                                           recommended: model.recommendedExternalResolution)
        _resolution = State(initialValue: remembered
            ?? (available.contains(current) ? current : available.last ?? current))
        let maximum = min(model.externalDisplayCapabilities?.maximumFramesPerSecond ?? 60,
                          model.daemonCapabilities?.maxFps ?? 60, 255)
        _frameRate = State(initialValue: min(stored?.externalFps ?? settings?.externalFps
                                             ?? settings?.fps ?? maximum, maximum))
    }

    private var capabilities: ExternalDisplayCapabilities {
        model.externalDisplayCapabilities ?? ExternalDisplayCapabilities(resolutions: [], preferredResolution: nil,
                                                                          maximumFramesPerSecond: 60)
    }
    private var resolutions: [DisplayResolution] {
        capabilities.resolutionChoices(maxWidth: model.daemonCapabilities?.maxWidth ?? 3840,
                                       maxHeight: model.daemonCapabilities?.maxHeight ?? 2160)
    }
    private var frameRates: [Int] {
        capabilities.frameRateChoices(daemonMaximum: model.daemonCapabilities?.maxFps ?? 60, selected: frameRate)
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    ForEach(DisplayOutputMode.allCases) { option in
                        Button {
                            mode = option
                        } label: {
                            HStack {
                                Text(option.label).foregroundStyle(.primary)
                                Spacer()
                                if mode == option { Image(systemName: "checkmark").foregroundStyle(.tint) }
                            }
                            .frame(minHeight: 28)
                            .contentShape(Rectangle())
                        }
                        .disabled(option != .internalOnly && !model.externalDisplayAvailable)
                        .accessibilityAddTraits(mode == option ? .isSelected : [])
                    }
                }

                if mode != .internalOnly {
                    Section("iPad External") {
                        if resolutions.isEmpty {
                            Text("No compatible resolutions").foregroundStyle(.secondary)
                        } else {
                            Picker("Resolution", selection: $resolution) {
                                ForEach(resolutions) { value in
                                    Text(value.label + (value == capabilities.preferredResolution ? " (Recommended)" : ""))
                                        .tag(value)
                                }
                            }
                        }
                        Picker("Frame rate", selection: $frameRate) {
                            ForEach(frameRates, id: \.self) { fps in
                                Text("\(fps) fps").tag(fps)
                            }
                        }
                    }
                }
                if model.isChangingDisplays {
                    HStack(spacing: 12) {
                        ProgressView()
                        Text("Switching displays…")
                    }
                }
                if let error = model.displayConfigurationError {
                    Text(error).foregroundStyle(.red)
                }
            }
            .navigationTitle("Displays")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Close") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Apply") {
                        if model.selectDisplayMode(mode, externalResolution: resolution, externalFrameRate: frameRate) {
                            dismiss()
                        }
                    }
                    .disabled(!model.isConnected || (mode != .internalOnly &&
                        (!model.externalDisplayAvailable || !resolutions.contains(resolution) || !frameRates.contains(frameRate))))
                }
            }
            .onAppear { normalizeChoices() }
            .onChange(of: model.externalDisplayCapabilities) { _ in normalizeChoices() }
            .onChange(of: model.externalDisplayAvailable) { available in
                if !available { mode = .internalOnly }
            }
        }
    }

    private func normalizeChoices() {
        if !resolutions.contains(resolution) {
            if let preferred = capabilities.preferredResolution, resolutions.contains(preferred) {
                resolution = preferred
            } else if let fallback = resolutions.last {
                resolution = fallback
            }
        }
        if !frameRates.contains(frameRate), let fallback = frameRates.last { frameRate = fallback }
    }
}

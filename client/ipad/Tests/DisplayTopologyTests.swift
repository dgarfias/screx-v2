import Foundation

// swiftc Screx/DeviceDisplayIdentity.swift Screx/MediaPacket.swift Screx/CapabilityNegotiation.swift Screx/DisplayTopology.swift Tests/DisplayTopologyTests.swift -o /tmp/screx-topology-tests
@main
struct DisplayTopologyTests {
    static func main() {
        var routing = DisplayRoutingState()
        precondition(routing.internalVideoVisible && !routing.externalWindowNeeded)
        routing.externalAvailable = true
        precondition(!routing.externalWindowNeeded, "Plugging in a monitor must not take over mirroring")
        routing.sessionEnabled = true
        routing.committedMask = 1
        routing.requestedMode = .externalOnly
        precondition(routing.internalVideoVisible && routing.externalWindowNeeded,
                     "Retain the internal surface while the external replacement starts")
        precondition(!routing.canAcknowledge(.externalDisplay, surfaceAttached: false),
                     "Decoding without an attached display surface must not retire the old monitor")
        precondition(routing.canAcknowledge(.externalDisplay, surfaceAttached: true))
        routing.committedMask = 2
        precondition(!routing.internalVideoVisible && routing.externalWindowNeeded)
        routing.requestedMode = .internalOnly
        precondition(routing.internalVideoVisible && routing.externalWindowNeeded,
                     "Retain the external surface until internal video is committed")
        routing.committedMask = 1
        precondition(routing.internalVideoVisible && !routing.externalWindowNeeded,
                     "Internal only releases the app-owned external window")
        routing.requestedMode = .both
        routing.committedMask = 3
        routing.externalAvailable = false
        precondition(routing.internalVideoVisible && !routing.externalWindowNeeded)
        precondition(!routing.canAcknowledge(.externalDisplay, surfaceAttached: true),
                     "An unplugged display must never acknowledge an in-flight replacement")
        routing.sessionEnabled = false
        precondition(routing.internalVideoVisible && !routing.externalWindowNeeded)
        precondition(!routing.canAcknowledge(.internalDisplay, surfaceAttached: true))

        var settings = StreamSettings(width: 1920, height: 1080, fps: 60, codecId: 1, bitrateBps: 15_000_000)
        settings.enabledDisplayMask = 3
        settings.externalWidth = 3840
        settings.externalHeight = 2160
        settings.externalFps = 30
        settings.revision = 7
        settings.internalDisplayName = "iPad A16 11\""
        let encoded = settings.encode()
        precondition(encoded.prefix(20) == Data([7, 0x80, 4, 0x38, 60, 1, 0, 0xe4, 0xe1, 0xc0,
                                                3, 0x0f, 0, 8, 0x70, 0, 0, 0, 7, 30]))
        precondition(encoded.suffix(13) == Data("iPad A16 11\" ".utf8))
        let defaults = StreamSettings().encode()
        precondition(defaults.count == 33 && defaults[5] == 0xff && defaults[10] == 1 && defaults[18] == 1 && defaults[19] == 0)
        let caps = DaemonCapabilities(camera: false, microphone: false, speaker: false, gamepadMaxControllers: nil,
                                      codecs: [0, 1], maxWidth: 3840, maxHeight: 2160, maxFps: 60,
                                      bitrateMinBps: 500_000, bitrateMaxBps: 20_000_000, hostname: "test")
        precondition(caps.validate(settings).isEmpty)
        settings.externalWidth = 4096
        precondition(caps.validate(settings).count == 1)
        settings.enabledDisplayMask = 1
        precondition(caps.validate(settings).isEmpty, "A disabled external resolution must not prevent internal streaming")
        settings.fps = 120
        precondition(caps.validate(settings).count == 1)
        settings.fps = 60
        settings.externalWidth = 3840
        settings.enabledDisplayMask = 3
        settings.externalFps = 120
        precondition(caps.validate(settings).count == 1, "External FPS has its own validation")
        precondition(settings.fps == 60, "Editing external FPS leaves internal FPS unchanged")

        let hd = DisplayResolution(width: 1920, height: 1080)
        let uhd = DisplayResolution(width: 3840, height: 2160)
        let displayCaps = ExternalDisplayCapabilities(resolutions: [hd, hd, uhd], preferredResolution: uhd,
                                                     maximumFramesPerSecond: 60)
        let choices = displayCaps.resolutionChoices(maxWidth: 3840, maxHeight: 2160)
        precondition(choices.filter { $0 == hd }.count == 1)
        precondition(choices.contains(uhd) && displayCaps.preferredResolution == uhd)
        precondition(!displayCaps.resolutionChoices(maxWidth: 1920, maxHeight: 1080).contains(uhd))
        precondition(choices == [hd, uhd], "Only modes supplied by iPadOS may appear")
        precondition(!choices.contains(DisplayResolution(width: 1280, height: 720)), "No synthetic fallback modes")
        precondition(ExternalDisplayCapabilities(resolutions: [], preferredResolution: nil, maximumFramesPerSecond: 60)
            .resolutionChoices(maxWidth: 3840, maxHeight: 2160).isEmpty)
        precondition(displayCaps.frameRateChoices(daemonMaximum: 120, selected: 120).last == 60)
        precondition(displayCaps.frameRateChoices(daemonMaximum: 30, selected: 60).last == 30)

        // Remembered per-host choices.
        precondition(DisplayPreferenceRules.mode(storedMode: nil, externalAvailable: true) == .internalOnly)
        precondition(DisplayPreferenceRules.mode(storedMode: 3, externalAvailable: true) == .both)
        precondition(DisplayPreferenceRules.mode(storedMode: 2, externalAvailable: true) == .externalOnly)
        precondition(DisplayPreferenceRules.mode(storedMode: 3, externalAvailable: false) == .internalOnly,
                     "A remembered external choice waits until a monitor is attached")
        precondition(DisplayPreferenceRules.mode(storedMode: 9, externalAvailable: true) == .internalOnly,
                     "Corrupt or unknown stored modes fall back safely")

        let rememberedHD = HostDisplayPreferences(mode: 3, externalWidth: 1920, externalHeight: 1080, externalFps: 30)
        precondition(DisplayPreferenceRules.resolution(stored: rememberedHD, available: choices,
                                                       recommended: uhd) == hd,
                     "The remembered resolution wins while the display still offers it")
        let remembered1440 = HostDisplayPreferences(mode: 3, externalWidth: 2560, externalHeight: 1440,
                                                   externalFps: 60)
        precondition(DisplayPreferenceRules.resolution(stored: remembered1440, available: choices,
                                                       recommended: uhd) == uhd,
                     "An unavailable remembered resolution falls back to Recommended")
        precondition(DisplayPreferenceRules.resolution(stored: nil, available: choices,
                                                       recommended: uhd) == uhd,
                     "With nothing remembered, start from Recommended")
        precondition(DisplayPreferenceRules.resolution(stored: nil, available: [hd],
                                                       recommended: uhd) == hd,
                     "Never select a mode the current display does not offer")
        precondition(DisplayPreferenceRules.resolution(stored: nil, available: [],
                                                       recommended: nil) == nil)
        let roundTrip = try! JSONDecoder().decode(HostDisplayPreferences.self,
                                                  from: try! JSONEncoder().encode(rememberedHD))
        precondition(roundTrip == rememberedHD)

        var statusBody = Data([0, 0, 0, 7, 1, 0, 1, 1, 2, 3, 4, 0x0f, 0, 8, 0x70])
        let status = DisplayStatus.parse(statusBody)!
        precondition(status.revision == 7 && status.display == .externalDisplay && status.phase == .streaming)
        precondition(status.activeMask == 1 && status.generation == 0x01020304)
        precondition(status.width == 3840 && status.height == 2160 && status.detail.isEmpty)
        precondition(DisplayStatus.parse(statusBody.prefix(14)) == nil)
        statusBody[5] = 2
        statusBody.append(contentsOf: "Encoder unavailable".utf8)
        precondition(DisplayStatus.parse(statusBody)!.phase == .failed)
        precondition(DisplayStatus.parse(statusBody)!.detail == "Encoder unavailable")
        statusBody[4] = 2
        precondition(DisplayStatus.parse(statusBody) == nil)
        print("Display topology wire-format and validation tests passed")
    }
}

import SwiftUI
import Combine
import AVFoundation
import CryptoKit
import Network
import GameController
import UIKit
import os

@main
final class AppDelegate: UIResponder, UIApplicationDelegate {
    let model = StreamViewModel()
    lazy var externalDisplay = ExternalDisplayCoordinator(model: model)

    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil
    ) -> Bool {
        model.configureAudioSession()
        _ = externalDisplay
        return true
    }

    func application(
        _ application: UIApplication,
        configurationForConnecting connectingSceneSession: UISceneSession,
        options: UIScene.ConnectionOptions
    ) -> UISceneConfiguration {
        let external = connectingSceneSession.role == .windowExternalDisplayNonInteractive
        let config = UISceneConfiguration(name: external ? "External Display" : "Default Configuration", sessionRole: connectingSceneSession.role)
        config.delegateClass = external ? ExternalDisplaySceneDelegate.self : SceneDelegate.self
        return config
    }
}

final class SceneDelegate: UIResponder, UIWindowSceneDelegate {
    var window: UIWindow?

    private var model: StreamViewModel? {
        (UIApplication.shared.delegate as? AppDelegate)?.model
    }

    func scene(
        _ scene: UIScene,
        willConnectTo session: UISceneSession,
        options connectionOptions: UIScene.ConnectionOptions
    ) {
        guard let windowScene = scene as? UIWindowScene else { return }
        guard let appDelegate = UIApplication.shared.delegate as? AppDelegate else { return }

        let window = UIWindow(windowScene: windowScene)
        window.rootViewController = ScrexRootViewController(model: appDelegate.model)
        self.window = window
        window.makeKeyAndVisible()
    }

    func sceneDidEnterBackground(_ scene: UIScene) {
        guard scene.session.role == .windowApplication else { return }
        model?.handleAppDidEnterBackground()
    }
}

final class MouseCaptureRootView: UIView {
    override var canBecomeFirstResponder: Bool {
        true
    }
}

let defaultDaemonPort: UInt16 = 9000

func formatEndpointInput(host: String, port: UInt16) -> String {
    let formattedHost = host.contains(":") ? "[\(host)]" : host
    return port == defaultDaemonPort ? formattedHost : "\(formattedHost):\(port)"
}

private func formatByteRate(_ bytesPerSecond: Double) -> String {
    if bytesPerSecond <= 0 {
        return "0 bytes/s"
    }
    let formatter = ByteCountFormatter()
    formatter.allowedUnits = [.useBytes, .useKB, .useMB]
    formatter.countStyle = .file
    formatter.includesUnit = true
    formatter.isAdaptive = true
    return "\(formatter.string(fromByteCount: Int64(bytesPerSecond.rounded())))/s"
}

private let networkCameraCompressionQuality: CGFloat = 0.4

struct RecentConnection: Codable, Identifiable, Equatable {
    let host: String
    let port: UInt16
    let name: String
    let lastConnectedAt: Date
    let isPinned: Bool

    private enum CodingKeys: String, CodingKey {
        case host
        case port
        case name
        case lastConnectedAt
        case isPinned
    }

    init(host: String, port: UInt16, name: String, lastConnectedAt: Date, isPinned: Bool = false) {
        self.host = host
        self.port = port
        self.name = name
        self.lastConnectedAt = lastConnectedAt
        self.isPinned = isPinned
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        host = try container.decode(String.self, forKey: .host)
        port = try container.decode(UInt16.self, forKey: .port)
        name = try container.decode(String.self, forKey: .name)
        lastConnectedAt = try container.decode(Date.self, forKey: .lastConnectedAt)
        isPinned = try container.decodeIfPresent(Bool.self, forKey: .isPinned) ?? false
    }

    var id: String { "\(host):\(port)" }
    var displayName: String { name.isEmpty ? host : name }
    var endpointLabel: String {
        formatEndpointInput(host: host, port: port)
    }
}

enum ConnectionTransport: String {
    case none = ""
    case network = "Network"
}

enum ConnectionHealthState: String {
    case idle = "Idle"
    case connecting = "Connecting"
    case pairing = "Pairing"
    case waitingForVideo = "Waiting for video"
    case streaming = "Streaming"
    case busy = "Busy"
    case connectionRefused = "Connection refused"
    case timedOut = "Timed out"
    case sessionStale = "Session stale, try again"
    case connectionError = "Connection error"

    var isConnected: Bool {
        switch self {
        case .streaming:
            return true
        default:
            return false
        }
    }

    var isConnecting: Bool {
        switch self {
        case .connecting, .pairing, .waitingForVideo:
            return true
        default:
            return false
        }
    }

    var isTerminalFailure: Bool {
        switch self {
        case .busy, .connectionRefused, .timedOut, .sessionStale, .connectionError:
            return true
        default:
            return false
        }
    }
}

/// User-facing resolution presets offered in the Stream Settings sheet. `daemonDefault`
/// means "send 0 for the resolution in `SETTINGS`" (let the daemon pick). `native`
/// ("This iPad") is this device's own panel resolution, detected from `UIScreen`; it is
/// only offered in the picker when no listed preset already matches the panel (see
/// `pickerCases`), so current models don't see a duplicate entry but a future unlisted
/// iPad still defaults to its exact panel resolution.
enum StreamResolutionPreset: String, CaseIterable, Identifiable {
    case daemonDefault = "default"
    case native = "native"
    case p720 = "720p"
    case p1080 = "1080p"
    case p1440 = "1440p"
    case uhd4k = "4k"
    case wxga1610 = "1280x800"
    case wuxga = "1920x1200"
    case wqxga = "2560x1600"
    case ipadMini = "2266x1488"
    case ipadBase = "2160x1620"
    case ipadA16 = "2360x1640"
    case ipadPro11 = "2388x1668"
    case ipadPro11M4 = "2420x1668"
    case ipadPro129 = "2732x2048"
    case ipadPro13M4 = "2752x2064"

    var id: String { rawValue }

    /// This device's native panel resolution in landscape orientation (width >= height),
    /// derived once from `UIScreen.main.nativeBounds` (a pixel size, not points).
    /// Cached because `UIScreen.main` access and the width/height normalization never
    /// change for the lifetime of the process.
    private static let nativeResolution: (width: Int, height: Int) = {
        let bounds = UIScreen.main.nativeBounds
        let long = Int(max(bounds.width, bounds.height))
        let short = Int(min(bounds.width, bounds.height))
        return (long, short)
    }()

    /// The listed preset matching this device's native panel resolution, if the list has
    /// one (it does for every iPad model the list was built from). `.native` itself is
    /// excluded from the search — it always matches by construction, and the point is to
    /// find a *listed* duplicate of it.
    static let deviceNative: StreamResolutionPreset? = allCases.first { preset in
        guard preset != .native, let resolution = preset.resolution else { return false }
        return resolution == nativeResolution
    }

    /// The cases offered in the Stream Settings picker: `.native` ("This iPad") is
    /// included only when no listed preset matches this device's panel, so it never
    /// shows up as a duplicate of a listed value.
    static let pickerCases: [StreamResolutionPreset] =
        deviceNative == nil ? allCases : allCases.filter { $0 != .native }

    var label: String {
        switch self {
        case .daemonDefault: return "Daemon default"
        case .native:
            let native = Self.nativeResolution
            return "This iPad (\(native.width)×\(native.height))"
        case .p720: return "720p (1280×720)"
        case .p1080: return "1080p (1920×1080)"
        case .p1440: return "1440p (2560×1440)"
        case .uhd4k: return "4K (3840×2160)"
        case .wxga1610: return "1280×800 (16:10)"
        case .wuxga: return "1920×1200 (16:10)"
        case .wqxga: return "2560×1600 (16:10)"
        case .ipadMini: return "2266×1488 (iPad mini)"
        case .ipadBase: return "2160×1620 (iPad 7th–9th gen)"
        case .ipadA16: return "2360×1640 (iPad 10th/11th gen, Air 11″)"
        case .ipadPro11: return "2388×1668 (iPad Pro 11″)"
        case .ipadPro11M4: return "2420×1668 (iPad Pro 11″ M4)"
        case .ipadPro129: return "2732×2048 (iPad Pro 12.9″, Air 13″)"
        case .ipadPro13M4: return "2752×2064 (iPad Pro 13″ M4)"
        }
    }

    var resolution: (width: Int, height: Int)? {
        switch self {
        case .daemonDefault: return nil
        case .native: return Self.nativeResolution
        case .p720: return (1280, 720)
        case .p1080: return (1920, 1080)
        case .p1440: return (2560, 1440)
        case .uhd4k: return (3840, 2160)
        case .wxga1610: return (1280, 800)
        case .wuxga: return (1920, 1200)
        case .wqxga: return (2560, 1600)
        case .ipadMini: return (2266, 1488)
        case .ipadBase: return (2160, 1620)
        case .ipadA16: return (2360, 1640)
        case .ipadPro11: return (2388, 1668)
        case .ipadPro11M4: return (2420, 1668)
        case .ipadPro129: return (2732, 2048)
        case .ipadPro13M4: return (2752, 2064)
        }
    }
}

/// User-facing framerate presets offered in the Stream Settings sheet.
enum StreamFrameratePreset: String, CaseIterable, Identifiable {
    case daemonDefault = "default"
    case fps30 = "30"
    case fps60 = "60"

    var id: String { rawValue }

    var label: String {
        switch self {
        case .daemonDefault: return "Daemon default"
        case .fps30: return "30 fps"
        case .fps60: return "60 fps"
        }
    }

    var fps: Int? {
        switch self {
        case .daemonDefault: return nil
        case .fps30: return 30
        case .fps60: return 60
        }
    }
}

/// User-facing codec presets offered in the Stream Settings sheet.
enum StreamCodecPreset: String, CaseIterable, Identifiable {
    case daemonDefault = "default"
    case h264 = "h264"
    case h265 = "h265"

    var id: String { rawValue }

    var label: String {
        switch self {
        case .daemonDefault: return "Daemon default"
        case .h264: return "H.264"
        case .h265: return "H.265"
        }
    }

    var codecId: UInt8? {
        switch self {
        case .daemonDefault: return nil
        case .h264: return DaemonCapabilities.codecH264
        case .h265: return DaemonCapabilities.codecH265
        }
    }
}

/// User-facing bitrate presets offered in the Stream Settings sheet. `.custom` has no
/// fixed `bitrateBps` of its own — its effective value comes from
/// `StreamViewModel.customBitrateMbps` instead (see `StreamViewModel.resolvedBitrateBps`).
enum StreamBitratePreset: String, CaseIterable, Identifiable {
    case daemonDefault = "default"
    case low = "low"
    case medium = "medium"
    case high = "high"
    case veryHigh = "veryhigh"
    case custom = "custom"

    var id: String { rawValue }

    var label: String {
        switch self {
        case .daemonDefault: return "Daemon default"
        case .low: return "Low (3 Mbps)"
        case .medium: return "Medium (8 Mbps)"
        case .high: return "High (15 Mbps)"
        case .veryHigh: return "Very high (20 Mbps)"
        case .custom: return "Custom…"
        }
    }

    var bitrateBps: UInt32? {
        switch self {
        case .daemonDefault: return nil
        case .low: return 3_000_000
        case .medium: return 8_000_000
        case .high: return 15_000_000
        case .veryHigh: return 20_000_000
        case .custom: return nil
        }
    }
}

final class ScrexRootViewController: GCEventViewController {
    private let model: StreamViewModel
    private let hostingController: UIHostingController<AnyView>
    private var cancellables = Set<AnyCancellable>()
    private let captureView = MouseCaptureRootView()
    private var externalSceneRegistration: AnyObject?

    init(model: StreamViewModel) {
        self.model = model
        self.hostingController = UIHostingController(
            rootView: AnyView(
                ContentView()
                    .environmentObject(model)
                    .onAppear { model.startServices() }
            )
        )
        super.init(nibName: nil, bundle: nil)
        controllerUserInteractionEnabled = !model.physicalMouseConnected
        observeModel()
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    override func loadView() {
        view = captureView
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        if #available(iOS 27.0, *) {
            let configuration = UISceneConfiguration(name: "External Display", sessionRole: .windowExternalDisplayNonInteractive)
            configuration.delegateClass = ExternalDisplaySceneDelegate.self
            // Keep registration while the main controller lives, including Internal only,
            // so a monitor can be detected without taking over ordinary system mirroring.
            externalSceneRegistration = registerSceneAccessory(.externalNonInteractive(sceneConfiguration: configuration))
        }
        addChild(hostingController)
        hostingController.view.translatesAutoresizingMaskIntoConstraints = false
        hostingController.view.backgroundColor = .clear
        view.addSubview(hostingController.view)
        NSLayoutConstraint.activate([
            hostingController.view.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            hostingController.view.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            hostingController.view.topAnchor.constraint(equalTo: view.topAnchor),
            hostingController.view.bottomAnchor.constraint(equalTo: view.bottomAnchor),
        ])
        hostingController.didMove(toParent: self)
    }

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        captureView.becomeFirstResponder()
        updatePhysicalMouseCapture()
        setNeedsStatusBarAppearanceUpdate()
        setNeedsUpdateOfHomeIndicatorAutoHidden()
    }

    override var prefersPointerLocked: Bool {
        model.physicalMouseConnected || model.physicalControllerConnectedCount > 0
    }

    override var prefersStatusBarHidden: Bool {
        true
    }

    override var prefersHomeIndicatorAutoHidden: Bool {
        true
    }

    override var preferredScreenEdgesDeferringSystemGestures: UIRectEdge {
        .all
    }

    private func observeModel() {
        model.$physicalMouseConnected
            .removeDuplicates()
            .sink { [weak self] _ in
                self?.updatePhysicalMouseCapture()
            }
            .store(in: &cancellables)

        model.$physicalControllerConnectedCount
            .removeDuplicates()
            .sink { [weak self] _ in
                self?.updatePhysicalMouseCapture()
            }
            .store(in: &cancellables)
    }

    private func updatePhysicalMouseCapture() {
        let captureActive = model.physicalMouseConnected || model.physicalControllerConnectedCount > 0
        controllerUserInteractionEnabled = !captureActive
        if captureActive {
            _ = captureView.becomeFirstResponder()
        }
        if #available(iOS 14.0, *) {
            setNeedsUpdateOfPrefersPointerLocked()
        }
        setNeedsStatusBarAppearanceUpdate()
        setNeedsUpdateOfHomeIndicatorAutoHidden()
    }
}

/// Lock-free (when atomics are available) or low-contention traffic counter.
/// Uses OSAllocatedUnfairLock as a portable fallback so we don't need to add
/// swift-atomics to the Xcode project.
final class TrafficCounter: @unchecked Sendable {
    private let lock = OSAllocatedUnfairLock()
    private var rx: UInt64 = 0
    private var tx: UInt64 = 0

    func add(rx: UInt64, tx: UInt64) {
        lock.withLock {
            self.rx &+= rx
            self.tx &+= tx
        }
    }

    func readAndReset() -> (rx: UInt64, tx: UInt64) {
        lock.withLock {
            let result = (rx, tx)
            rx = 0
            tx = 0
            return result
        }
    }
}

@MainActor
final class StreamViewModel: ObservableObject {
    @Published var status: String = "Enter a daemon host or IP to connect."
    @Published private(set) var connectionHealth: ConnectionHealthState = .idle
    @Published var isConnected = false
    @Published var manualHost: String = ""
    @Published var transport: String = ""
    @Published var showPinEntry = false
    @Published var pinInput: String = ""
    @Published var pairingStatus: String = ""
    @Published var recentConnections: [RecentConnection] = StreamViewModel.loadRecentConnections()
    @Published private(set) var sessionDisplayName: String = ""
    /// Per-second diagnostics, observed only by the info views. Kept out of the view model's own
    /// @Published state so their updates don't recompute the entire session UI twice a second.
    let stats = StreamStats()
    /// Internal stream size for touch mapping; changes only when the decoded format changes.
    @Published private(set) var internalVideoSize = CGSize(width: 1920, height: 1080)

    /// What the currently-connected daemon advertised via `CAPS`. Nil until a `CAPS`
    /// message arrives (or the 2s timeout fallback fires, see `beginCapabilityNegotiation`).
    @Published private(set) var daemonCapabilities: DaemonCapabilities?
    @Published private(set) var displayConfigurationError: String?
    private var currentDisplaySettings: StreamSettings?
    private var committedDisplaySettings: StreamSettings?
    @Published private(set) var externalDisplayCapabilities: ExternalDisplayCapabilities?
    private var readyDisplayGenerations: [StreamDisplayID: UInt32] = [:]
    @Published private(set) var displayRouting = DisplayRoutingState()
    @Published private(set) var isChangingDisplays = false
    @Published private(set) var inputLayoutRevision: UInt32 = 0
    weak var externalDisplayCoordinator: ExternalDisplayCoordinator?
    private var attachedDisplaySurfaces = Set<StreamDisplayID>()
    private var trackpadBeforeExternalOnly: Bool?

    var selectedDisplayMode: DisplayOutputMode { displayRouting.requestedMode }
    var externalDisplayAvailable: Bool { displayRouting.externalAvailable }
    var currentDisplayConfiguration: StreamSettings? { currentDisplaySettings }
    func clearDisplayConfigurationError() { displayConfigurationError = nil }
    var canForwardInternalTouch: Bool {
        // During a transition the old internal desktop is still on screen and still valid input
        // space, so only its visibility matters (the daemon sends its generation-scoped stream).
        displayRouting.internalVideoVisible && attachedDisplaySurfaces.contains(.internalDisplay)
    }
    /// Opt-in for External only; always starts off, and resets whenever External only ends.
    @Published private(set) var isLaptopMode = false
    var isExternalOnlyActive: Bool {
        isConnected && !displayRouting.internalVideoVisible
            && displayRouting.committedMask == DisplayOutputMode.externalOnly.rawValue
    }
    /// Laptop mode replaces the black placeholder, the floating pill and the iOS keyboard.
    var isExternalControlPanelVisible: Bool { isExternalOnlyActive && isLaptopMode }

    func setLaptopMode(_ enabled: Bool) {
        isLaptopMode = enabled && isExternalOnlyActive
    }

    /// Stream-settings preferences the user picked in the Stream Settings sheet,
    /// persisted in UserDefaults and re-sent unmodified as `SETTINGS` on every connect —
    /// validated against the daemon's advertised bounds first (see
    /// `handleDaemonCapabilities`); a setting that doesn't fit fails the connection
    /// instead of being silently clamped.
    @Published var preferredResolution: StreamResolutionPreset = StreamViewModel.loadPreferredResolution()
    @Published var preferredFramerate: StreamFrameratePreset = StreamViewModel.loadPreferredFramerate()
    @Published var preferredCodec: StreamCodecPreset = StreamViewModel.loadPreferredCodec()
    @Published var preferredBitrate: StreamBitratePreset = StreamViewModel.loadPreferredBitrate()
    /// Effective bitrate (in Mbps) when `preferredBitrate == .custom`. Ignored otherwise.
    @Published var customBitrateMbps: Double = StreamViewModel.loadCustomBitrateMbps()

    let decoder = VideoDecoder()
    let externalDecoder = VideoDecoder()
    let avSync = AVSyncState()
    let audioPlayer: AudioPlayer
    let cameraCapture = CameraCapture()
    let micCapture = MicCapture()

    private var stream: StreamClient?
    private var networkControl: NetworkControlClient?
    private var pairingService: PairingService?
    private var pendingPinCompletion: ((String) -> Void)?
    private var sessionKey: SymmetricKey?

    nonisolated init() {
        self.audioPlayer = AudioPlayer(avSync: avSync)
    }
    private var servicesStarted = false
    private var camFrameId: UInt32 = 0

    private var lastNetEndpoint: NWEndpoint?
    private var lastNetName: String?
    /// Daemon hostname (or connection name) used to key remembered display choices.
    @Published private(set) var currentHostKey: String?
    private static let hostDisplayPreferencesKey = "screx_host_display_preferences"
    private var micSeq: UInt32 = 0
    @Published private(set) var isConnecting = false
    private var activeTransport: ConnectionTransport = .none
    private var trafficTimer: DispatchSourceTimer?
    private nonisolated let trafficCounter = TrafficCounter()

    /// Fails the connection if no `CAPS` message arrives within `capsTimeoutInterval`
    /// of the session coming up. Cancelled when `CAPS` arrives.
    private var capsTimeoutWorkItem: DispatchWorkItem?
    private static let capsTimeoutInterval: TimeInterval = 5.0

    @Published var physicalMouseConnected = false
    @Published private(set) var isTrackpadEnabled = false
    @Published var physicalKeyboardConnected = false
    @Published var physicalControllerConnectedCount = 0
    @Published private(set) var isControllerPassthroughEnabled = true
    private var mouseObservers: [Any] = []
    private var connectedMice: [ObjectIdentifier: GCMouse] = [:]
    private var keyboardObservers: [Any] = []
    private var controllerObservers: [Any] = []
    private var controllerSlots: [ObjectIdentifier: UInt8] = [:]
    private var physicalMouseButtonMask: UInt8 = 0
    private var physicalMouseScrollAccumulator: Float = 0
    private var trackpadButtonMask: UInt8 = 0
    private var trackpadMotionRemainder: CGSize = .zero
    private var trackpadScrollRemainder: CGFloat = 0

    private static let recentConnectionsKey = "screx_recent_connections"
    private static let maxRecentConnections = 5
    private static let maxPinnedConnections = 10

    private func log(_ message: String) {
        print("[app] \(message)")
    }

    private static func formatLatency(_ ms: Double) -> String {
        String(format: "%.0f ms", ms)
    }

    /// Configures the shared AVAudioSession based on whether the in-app microphone is active.
    ///
    /// Playback-only sessions allow normal HDMI/AirPlay/A2DP output selection. Enable duplex
    /// routing only while recording; forcing a recording category can exclude output-only routes.
    func configureAudioSession() {
        configureAudioSession(micActive: micCapture.isRunning)
    }

    private func configureAudioSession(micActive: Bool) {
        let session = AVAudioSession.sharedInstance()
        do {
            let options: AVAudioSession.CategoryOptions = micActive
                ? [.defaultToSpeaker, .mixWithOthers, .allowBluetooth]
                : [.mixWithOthers]
            try session.setCategory(micActive ? .playAndRecord : .playback, mode: .default, options: options)
            try session.setPreferredSampleRate(48000)
            try session.setPreferredIOBufferDuration(0.01)
            try session.setActive(true)
            print("[app] audio session configured: mic=\(micActive), options=\(options), sampleRate=\(session.sampleRate), ioBufferDuration=\(session.ioBufferDuration)")
            print("[app] audio output: \(session.currentRoute.outputs.map { "\($0.portType.rawValue):\($0.portName)" }.joined(separator: ", "))")
        } catch {
            print("[app] audio session configuration failed: \(error)")
        }
    }

    var pinnedConnections: [RecentConnection] {
        recentConnections.filter(\.isPinned)
    }

    var unpinnedRecentConnections: [RecentConnection] {
        recentConnections.filter { !$0.isPinned }
    }

    private static func normalizeConnections(_ connections: [RecentConnection]) -> [RecentConnection] {
        let pinned = connections
            .filter(\.isPinned)
            .sorted { $0.lastConnectedAt > $1.lastConnectedAt }
        let recents = connections
            .filter { !$0.isPinned }
            .sorted { $0.lastConnectedAt > $1.lastConnectedAt }

        return Array(pinned.prefix(Self.maxPinnedConnections))
            + Array(recents.prefix(Self.maxRecentConnections))
    }

    private static func loadRecentConnections() -> [RecentConnection] {
        guard let data = UserDefaults.standard.data(forKey: recentConnectionsKey) else {
            return []
        }
        do {
            let decoded = try JSONDecoder().decode([RecentConnection].self, from: data)
            return normalizeConnections(decoded)
        } catch {
            print("[app] failed to load recent connections: \(error)")
            return []
        }
    }

    private func persistRecentConnections() {
        do {
            let data = try JSONEncoder().encode(recentConnections)
            UserDefaults.standard.set(data, forKey: Self.recentConnectionsKey)
        } catch {
            log("failed to persist recent connections: \(error)")
        }
    }

    // MARK: - Stream settings preferences

    private static let preferredResolutionKey = "screx_pref_resolution"
    private static let preferredFramerateKey = "screx_pref_framerate"
    private static let preferredCodecKey = "screx_pref_codec"
    private static let preferredBitrateKey = "screx_pref_bitrate"
    private static let customBitrateMbpsKey = "screx_pref_bitrate_custom_mbps"
    private static let defaultCustomBitrateMbps: Double = 10.0

    /// Falls back to this device's own panel resolution — the matching listed preset
    /// when there is one, `.native` ("This iPad") otherwise — both when no preference
    /// has ever been stored and when a stored rawValue is unrecognized (e.g. a preset
    /// removed in a later version). A stored `.native` is normalized to the matching
    /// listed preset when one exists, since `.native` isn't offered in the picker then.
    private static func loadPreferredResolution() -> StreamResolutionPreset {
        let fallback = StreamResolutionPreset.deviceNative ?? .native
        guard let raw = UserDefaults.standard.string(forKey: preferredResolutionKey),
              let preset = StreamResolutionPreset(rawValue: raw) else {
            return fallback
        }
        if preset == .native, let listed = StreamResolutionPreset.deviceNative {
            return listed
        }
        return preset
    }

    private static func loadPreferredFramerate() -> StreamFrameratePreset {
        guard let raw = UserDefaults.standard.string(forKey: preferredFramerateKey) else {
            return .fps60
        }
        return StreamFrameratePreset(rawValue: raw) ?? .fps60
    }

    private static func loadPreferredCodec() -> StreamCodecPreset {
        let raw = UserDefaults.standard.string(forKey: preferredCodecKey) ?? StreamCodecPreset.daemonDefault.rawValue
        return StreamCodecPreset(rawValue: raw) ?? .daemonDefault
    }

    /// Falls back to `.medium` (8 Mbps) both when no preference has ever been stored and
    /// when a stored rawValue is unrecognized (e.g. a preset removed in a later version).
    private static func loadPreferredBitrate() -> StreamBitratePreset {
        guard let raw = UserDefaults.standard.string(forKey: preferredBitrateKey) else {
            return .medium
        }
        return StreamBitratePreset(rawValue: raw) ?? .medium
    }

    private static func loadCustomBitrateMbps() -> Double {
        let defaults = UserDefaults.standard
        guard defaults.object(forKey: customBitrateMbpsKey) != nil else { return defaultCustomBitrateMbps }
        let value = defaults.double(forKey: customBitrateMbpsKey)
        return value.isFinite && value > 0 ? value : defaultCustomBitrateMbps
    }

    /// Called when the Stream Settings sheet is dismissed to persist whatever the user
    /// picked. These persisted values are what feed the `SETTINGS` validated and sent on every
    /// subsequent connect (see `handleDaemonCapabilities`).
    func persistStreamSettingsPreferences() {
        let defaults = UserDefaults.standard
        defaults.set(preferredResolution.rawValue, forKey: Self.preferredResolutionKey)
        defaults.set(preferredFramerate.rawValue, forKey: Self.preferredFramerateKey)
        defaults.set(preferredCodec.rawValue, forKey: Self.preferredCodecKey)
        defaults.set(preferredBitrate.rawValue, forKey: Self.preferredBitrateKey)
        defaults.set(customBitrateMbps, forKey: Self.customBitrateMbpsKey)
    }

    /// The user's configured stream settings, exactly as picked in the Stream Settings
    /// sheet — no clamping or snapping against any daemon's advertised bounds. Sent to
    /// the daemon as-is (see `handleDaemonCapabilities`) once `DaemonCapabilities.validate`
    /// confirms it fits; otherwise the connection is failed instead of adjusting this.
    private var preferredStreamSettings: StreamSettings {
        let resolution = preferredResolution.resolution
        return StreamSettings(
            width: resolution?.width,
            height: resolution?.height,
            fps: preferredFramerate.fps,
            codecId: preferredCodec.codecId,
            bitrateBps: resolvedBitrateBps
        )
    }

    /// Resolves `preferredBitrate` (and, for `.custom`, `customBitrateMbps`) to a concrete
    /// bps value to put on the wire, unmodified. Non-finite or non-positive custom input
    /// is treated as "no preference" (nil), same as daemon default; a finite custom value
    /// is capped only against `UInt32.max` to guard the `Double` -> `UInt32` conversion
    /// from overflowing, never against the daemon's advertised range — that's
    /// `DaemonCapabilities.validate`'s job, and it fails the connection rather than
    /// snapping this value.
    private var resolvedBitrateBps: UInt32? {
        guard preferredBitrate == .custom else {
            return preferredBitrate.bitrateBps
        }
        guard customBitrateMbps.isFinite, customBitrateMbps > 0 else { return nil }
        let rawBps = (customBitrateMbps * 1_000_000).rounded()
        guard rawBps.isFinite, rawBps > 0 else { return nil }
        let capped = min(rawBps, Double(UInt32.max))
        return UInt32(capped)
    }

    private func rememberRecentConnection(name: String, host: String, port: UInt16) {
        let displayName = name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? host : name
        let existing = recentConnections.first { $0.host == host && $0.port == port }
        var updated = recentConnections.filter { $0.host != host || $0.port != port }
        updated.insert(
            RecentConnection(
                host: host,
                port: port,
                name: displayName,
                lastConnectedAt: Date(),
                isPinned: existing?.isPinned ?? false
            ),
            at: 0
        )
        recentConnections = Self.normalizeConnections(updated)
        persistRecentConnections()
    }

    private func makeEndpoint(host: String, port: UInt16) -> NWEndpoint {
        NWEndpoint.hostPort(
            host: NWEndpoint.Host(host),
            port: NWEndpoint.Port(integerLiteral: port)
        )
    }

    private func parseManualEndpoint(_ rawInput: String) -> (host: String, port: UInt16)? {
        let input = rawInput.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !input.isEmpty else { return nil }

        if input.hasPrefix("[") {
            guard let closeBracket = input.firstIndex(of: "]") else {
                applyConnectionHealth(.idle, detail: "Invalid address. Use host, host:port, or [ipv6]:port.")
                return nil
            }

            let host = String(input[input.index(after: input.startIndex)..<closeBracket])
            let suffix = String(input[input.index(after: closeBracket)...])
            guard !host.isEmpty else {
                applyConnectionHealth(.idle, detail: "Host cannot be empty.")
                return nil
            }
            if suffix.isEmpty {
                return (host, defaultDaemonPort)
            }
            guard suffix.hasPrefix(":"), let port = UInt16(suffix.dropFirst()), port > 0 else {
                applyConnectionHealth(.idle, detail: "Invalid port. Use a value from 1 to 65535.")
                return nil
            }
            return (host, port)
        }

        let colonCount = input.reduce(into: 0) { count, char in
            if char == ":" { count += 1 }
        }

        if colonCount == 1, let colon = input.lastIndex(of: ":") {
            let host = String(input[..<colon])
            let portPart = String(input[input.index(after: colon)...])
            guard !host.isEmpty else {
                    applyConnectionHealth(.idle, detail: "Host cannot be empty.")
                return nil
            }
            guard let port = UInt16(portPart), port > 0 else {
                    applyConnectionHealth(.idle, detail: "Invalid port. Use a value from 1 to 65535.")
                return nil
            }
            return (host, port)
        }

        return (input, defaultDaemonPort)
    }

    private func endpointHostAndPort(_ endpoint: NWEndpoint, fallbackHost: String) -> (host: String, port: UInt16) {
        switch endpoint {
        case .hostPort(let host, let port):
            return ("\(host)", port.rawValue)
        default:
            return (fallbackHost, defaultDaemonPort)
        }
    }

    private func disconnectedPrompt() -> String {
        recentConnections.isEmpty
            ? "Enter a daemon host or IP[:port] to connect."
            : "Choose a pinned or recent daemon, or enter a host or IP[:port] to connect."
    }

    var connectionHealthTitle: String {
        connectionHealth.rawValue
    }

    private func applyConnectionHealth(
        _ state: ConnectionHealthState,
        detail: String? = nil,
        transport: ConnectionTransport = .none
    ) {
        connectionHealth = state
        activeTransport = transport
        self.transport = transport.rawValue
        syncCameraCompressionQuality()
        status = detail ?? defaultDetail(for: state)
        isConnected = state.isConnected
        isConnecting = state.isConnecting
        if transport == .none && !state.isConnected {
            stats.reset()
        }
    }

    private func defaultDetail(for state: ConnectionHealthState) -> String {
        switch state {
        case .idle:
            return disconnectedPrompt()
        case .connecting:
            return "Opening a connection to the daemon."
        case .pairing:
            return "Negotiating a secure session."
        case .waitingForVideo:
            return "Connected, waiting for the first video frame."
        case .streaming:
            return ""
        case .busy:
            return "The daemon is already in use by another client."
        case .connectionRefused:
            return "The daemon refused the connection."
        case .timedOut:
            return "The daemon stopped responding in time."
        case .sessionStale:
            return "The saved session looks stale. Try again or pair again."
        case .connectionError:
            return "The connection failed unexpectedly."
        }
    }

    private func applyConnectionFailure(_ message: String, transport: ConnectionTransport = .none) {
        let normalized = message.lowercased()
        if normalized.contains("busy") {
            applyConnectionHealth(.busy, detail: "The daemon is already in use by another client.", transport: transport)
        } else if normalized.contains("refused") {
            applyConnectionHealth(.connectionRefused, detail: "The daemon refused the connection.", transport: transport)
        } else if normalized.contains("timed out") || normalized.contains("timeout") {
            applyConnectionHealth(.timedOut, detail: "The daemon stopped responding in time.", transport: transport)
        } else if normalized.contains("not recognized by daemon")
            || normalized.contains("pair again")
            || normalized.contains("stale")
        {
            applyConnectionHealth(.sessionStale, detail: "The saved session looks stale. Try again.", transport: transport)
        } else {
            applyConnectionHealth(.connectionError, detail: message, transport: transport)
        }
    }

    private func sendSpeakerTransportState(isEnabled: Bool) {
        guard activeTransport != .none else { return }
        networkControl?.sendSpeakerState(isEnabled: isEnabled)
    }

    private func syncSpeakerPassthroughState() {
        sendSpeakerTransportState(isEnabled: audioPlayer.isOutputEnabled)
    }

    // MARK: - Capability negotiation

    /// Called once a session is up (network `.sessionEstablished`).
    /// Clears any capabilities left over from a previous session and starts the `CAPS`
    /// timeout; `handleDaemonCapabilities` cancels it when `CAPS` arrives.
    private func beginCapabilityNegotiation(transport: ConnectionTransport) {
        log("beginCapabilityNegotiation(transport: \(transport.rawValue))")
        capsTimeoutWorkItem?.cancel()
        daemonCapabilities = nil

        let workItem = DispatchWorkItem { [weak self] in
            guard let self else { return }
            self.log("CAPS timeout")
            self.failConnection(detail: "Failed to connect: the daemon did not send its capabilities.")
        }
        capsTimeoutWorkItem = workItem
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.capsTimeoutInterval, execute: workItem)
    }

    /// Called when a real `CAPS` message arrives from the daemon. Cancels the timeout
    /// fallback, publishes the capabilities, then validates the user's persisted stream
    /// settings against them: if everything fits, sends the settings on to the daemon
    /// exactly as configured (no clamping); if anything doesn't fit, fails the connection
    /// instead of silently adjusting the user's preference — see
    /// `DaemonCapabilities.validate` and `failConnection`.
    private func handleDaemonCapabilities(_ capabilities: DaemonCapabilities) {
        capsTimeoutWorkItem?.cancel()
        capsTimeoutWorkItem = nil
        daemonCapabilities = capabilities
        updateSessionHostname(capabilities.hostname)
        currentHostKey = capabilities.hostname.isEmpty
            ? (lastNetName ?? "")
            : capabilities.hostname

        // Internal resolution/FPS/codec/bitrate come from Stream Settings. The display mode and
        // the external output's resolution/FPS come from this host's remembered choice, falling
        // back to internal-only and the display's recommended resolution.
        var settings = preferredStreamSettings
        let remembered = displayPreferences(forHost: currentHostKey)
        settings.enabledDisplayMask = DisplayPreferenceRules.mode(
            storedMode: remembered?.mode,
            externalAvailable: externalDisplayAvailable
        ).rawValue
        if settings.enabledDisplayMask & 2 != 0 {
            if let resolution = externalResolution(stored: remembered) {
                settings.externalWidth = resolution.width
                settings.externalHeight = resolution.height
            }
            settings.externalFps = remembered?.externalFps
        }

        let violations = capabilities.validate(settings)
        guard violations.isEmpty else {
            log("CAPS received: \(capabilities); settings \(settings) violate capabilities: \(violations)")
            failConnection(detail: "Failed to connect: " + violations.joined(separator: "; "))
            return
        }

        log("CAPS received: \(capabilities); sending SETTINGS: \(settings)")
        sendStreamSettingsTransportState(settings)
    }

    /// Fails the in-progress connection because the daemon's advertised capabilities
    /// don't satisfy the user's configured stream settings. Tears down the session
    /// (stream, network control, decoder/audio/camera/mic/peripheral state, and
    /// capability-negotiation state), then reports the failure through the same
    /// `applyConnectionHealth(.connectionError, ...)` surface every other connection
    /// failure uses, so the UI shows it identically. Safe to call when no session is
    /// active: every teardown call here is nil-safe.
    private func failConnection(detail: String) {
        log("failConnection: \(detail)")
        stream?.onEvent = nil
        stream?.onDisconnect = nil
        stream?.disconnect()
        stream = nil
        closeNetworkControl(gracefully: false)
        pairingService?.cancel()
        pairingService = nil
        decoder.setSuspended(false)
        audioPlayer.stop()
        micCapture.stop()
        configureAudioSession()
        cameraCapture.stop()
        stopPeripheralMonitoring()
        resetCapabilityNegotiation()
        applyConnectionHealth(.connectionError, detail: detail, transport: .none)
    }

    private func sendStreamSettingsTransportState(_ settings: StreamSettings) {
        currentDisplaySettings = settings
        displayRouting.sessionEnabled = true
        displayRouting.requestedMode = DisplayOutputMode(rawValue: settings.enabledDisplayMask) ?? .internalOnly
        isChangingDisplays = true
        externalDisplayCoordinator?.reconcile()
        networkControl?.sendStreamSettings(settings)
    }

    func setExternalDisplayAvailable(_ available: Bool) {
        guard available != displayRouting.externalAvailable else { return }
        displayRouting.externalAvailable = available
        if available {
            applyRememberedDisplayModeIfNeeded()
        } else if let settings = currentDisplaySettings,
                  (settings.enabledDisplayMask | displayRouting.committedMask) & 2 != 0 {
            var fallback = settings
            fallback.enabledDisplayMask = DisplayOutputMode.internalOnly.rawValue
            _ = requestDisplaySettings(fallback)
        }
    }

    func setExternalDisplayCapabilities(_ capabilities: ExternalDisplayCapabilities?) {
        externalDisplayCapabilities = capabilities
    }

    // MARK: - Remembered display choices

    var availableExternalResolutions: [DisplayResolution] {
        externalDisplayCapabilities?.resolutionChoices(
            maxWidth: daemonCapabilities?.maxWidth ?? 3840,
            maxHeight: daemonCapabilities?.maxHeight ?? 2160
        ) ?? []
    }

    var recommendedExternalResolution: DisplayResolution? {
        guard let capabilities = externalDisplayCapabilities else { return nil }
        let available = availableExternalResolutions
        if let preferred = capabilities.preferredResolution, available.contains(preferred) {
            return preferred
        }
        return available.last
    }

    func displayPreferences(forHost host: String?) -> HostDisplayPreferences? {
        guard let host, !host.isEmpty else { return nil }
        return hostDisplayPreferences[host]
    }

    private var hostDisplayPreferences: [String: HostDisplayPreferences] {
        get {
            guard let data = UserDefaults.standard.data(forKey: Self.hostDisplayPreferencesKey),
                  let decoded = try? JSONDecoder().decode([String: HostDisplayPreferences].self, from: data)
            else { return [:] }
            return decoded
        }
        set {
            guard let data = try? JSONEncoder().encode(newValue) else { return }
            UserDefaults.standard.set(data, forKey: Self.hostDisplayPreferencesKey)
        }
    }

    private func rememberDisplaySelection(_ mode: DisplayOutputMode,
                                          resolution: DisplayResolution, frameRate: Int) {
        guard let host = currentHostKey, !host.isEmpty else { return }
        var all = hostDisplayPreferences
        all[host] = HostDisplayPreferences(mode: mode.rawValue,
                                           externalWidth: resolution.width,
                                           externalHeight: resolution.height,
                                           externalFps: frameRate)
        hostDisplayPreferences = all
    }

    private func externalResolution(stored: HostDisplayPreferences?) -> DisplayResolution? {
        DisplayPreferenceRules.resolution(stored: stored,
                                          available: availableExternalResolutions,
                                          recommended: recommendedExternalResolution)
    }

    /// A monitor appearing after connect applies this host's remembered external choice once.
    private func applyRememberedDisplayModeIfNeeded() {
        guard isConnected, externalDisplayAvailable,
              let stored = displayPreferences(forHost: currentHostKey), stored.mode & 2 != 0,
              var settings = currentDisplaySettings,
              settings.enabledDisplayMask != stored.mode else { return }
        settings.enabledDisplayMask = stored.mode
        if let resolution = externalResolution(stored: stored) {
            settings.externalWidth = resolution.width
            settings.externalHeight = resolution.height
        }
        settings.externalFps = stored.externalFps
        _ = requestDisplaySettings(settings)
    }

    func setDisplaySurfaceAttached(_ display: StreamDisplayID, attached: Bool) {
        if attached {
            guard attachedDisplaySurfaces.insert(display).inserted else { return }
            if let generation = readyDisplayGenerations[display] {
                acknowledgeReadyDisplay(display, generation: generation)
            }
        } else {
            attachedDisplaySurfaces.remove(display)
        }
    }

    @discardableResult
    func selectDisplayMode(_ mode: DisplayOutputMode, externalResolution: DisplayResolution,
                           externalFrameRate: Int) -> Bool {
        guard var settings = currentDisplaySettings else { return false }
        guard mode == .internalOnly || externalDisplayAvailable else {
            displayConfigurationError = "Connect an external display first."
            return false
        }
        settings.enabledDisplayMask = mode.rawValue
        if mode != .internalOnly {
            settings.externalWidth = externalResolution.width
            settings.externalHeight = externalResolution.height
            settings.externalFps = externalFrameRate
        }
        let applied = requestDisplaySettings(settings)
        if applied {
            rememberDisplaySelection(mode, resolution: externalResolution, frameRate: externalFrameRate)
        }
        return applied
    }

    private func releaseInputForDisplayChange() {
        let previousMask = physicalMouseButtonMask | trackpadButtonMask
        physicalMouseButtonMask = 0
        trackpadButtonMask = 0
        sendMouseButtonChanges(from: previousMask)
        // Release hardware keys even if a cable was pulled in the middle of a chord.
        sendPeripheral(Data([0x02, 0x00]))
        if physicalKeyboardConnected { sendPeripheral(Data([0x02, 0x01])) }
        trackpadMotionRemainder = .zero
        trackpadScrollRemainder = 0
        inputLayoutRevision &+= 1
    }

    /// Runtime topology requests use one monotonically increasing revision for both outputs.
    @discardableResult
    func requestDisplaySettings(_ settings: StreamSettings) -> Bool {
        guard let capabilities = daemonCapabilities, let current = currentDisplaySettings,
              networkControl != nil, current.revision < UInt32.max else { return false }
        let violations = capabilities.validate(settings)
        guard violations.isEmpty, (1...3).contains(settings.enabledDisplayMask) else {
            displayConfigurationError = violations.first ?? "Select at least one display."
            return false
        }
        var next = settings
        next.revision = current.revision + 1
        releaseInputForDisplayChange()
        displayConfigurationError = nil
        sendStreamSettingsTransportState(next)
        return true
    }

    private func acknowledgeReadyDisplay(_ display: StreamDisplayID, generation: UInt32) {
        readyDisplayGenerations[display] = generation
        guard let settings = currentDisplaySettings,
              settings.enabledDisplayMask & (1 << display.rawValue) != 0,
              displayRouting.canAcknowledge(display, surfaceAttached: attachedDisplaySurfaces.contains(display)) else { return }
        networkControl?.sendDisplayPresented(revision: settings.revision, display: display, generation: generation)
    }

    private func handleDisplayStatus(_ update: DisplayStatus) {
        guard update.revision == currentDisplaySettings?.revision else { return }
        if update.phase == .failed {
            if update.activeMask == 0 {
                failConnection(detail: "Could not start the display: \(update.detail)")
                return
            }
            displayConfigurationError = update.detail
            currentDisplaySettings?.enabledDisplayMask = update.activeMask
            currentDisplaySettings?.externalFps = committedDisplaySettings?.externalFps
            displayRouting.requestedMode = DisplayOutputMode(rawValue: update.activeMask) ?? .internalOnly
            displayRouting.committedMask = update.activeMask
            isChangingDisplays = false
            if update.generation > 0, update.width > 0, update.height > 0 {
                if update.display == .internalDisplay {
                    currentDisplaySettings?.width = update.width
                    currentDisplaySettings?.height = update.height
                } else {
                    currentDisplaySettings?.externalWidth = update.width
                    currentDisplaySettings?.externalHeight = update.height
                }
            }
            externalDisplayCoordinator?.reconcile()
            restoreTrackpadAfterExternalOnly()
        } else if update.phase == .committed || update.phase == .removed {
            committedDisplaySettings = currentDisplaySettings
            displayRouting.committedMask = update.activeMask
            isChangingDisplays = false
            if update.activeMask == DisplayOutputMode.externalOnly.rawValue {
                if trackpadBeforeExternalOnly == nil { trackpadBeforeExternalOnly = isTrackpadEnabled }
                setTrackpadEnabled(true)
            } else {
                restoreTrackpadAfterExternalOnly()
            }
            externalDisplayCoordinator?.reconcile()
            // A monitor can disappear between the final ACK and the committed status.
            if !externalDisplayAvailable, update.activeMask & 2 != 0,
               currentDisplaySettings?.enabledDisplayMask != DisplayOutputMode.internalOnly.rawValue,
               let settings = currentDisplaySettings {
                var fallback = settings
                fallback.enabledDisplayMask = DisplayOutputMode.internalOnly.rawValue
                _ = requestDisplaySettings(fallback)
            }
        } else if update.phase == .streaming, readyDisplayGenerations[update.display] == update.generation {
            acknowledgeReadyDisplay(update.display, generation: update.generation)
        }
    }

    private func restoreTrackpadAfterExternalOnly() {
        if displayRouting.committedMask != DisplayOutputMode.externalOnly.rawValue { isLaptopMode = false }
        guard displayRouting.committedMask != DisplayOutputMode.externalOnly.rawValue,
              let saved = trackpadBeforeExternalOnly else { return }
        trackpadBeforeExternalOnly = nil
        setTrackpadEnabled(saved)
    }

    /// Clears capability-negotiation state on disconnect so a fresh connection starts
    /// from a clean slate (pre-CAPS "everything available" state) rather than carrying
    /// over the previous session's daemon capabilities.
    private func resetCapabilityNegotiation() {
        capsTimeoutWorkItem?.cancel()
        capsTimeoutWorkItem = nil
        daemonCapabilities = nil
        currentDisplaySettings = nil
        committedDisplaySettings = nil
        readyDisplayGenerations.removeAll()
        displayConfigurationError = nil
        displayRouting.sessionEnabled = false
        displayRouting.requestedMode = .internalOnly
        displayRouting.committedMask = 0
        isChangingDisplays = false
        trackpadBeforeExternalOnly = nil
        isLaptopMode = false
        externalDisplayCoordinator?.reconcile()
    }

    func handleAppDidEnterBackground() {
        log("sceneDidEnterBackground")
        guard activeTransport != .none || isConnecting || pairingService != nil else { return }
        disconnect(detail: "Screx disconnected when the app entered the background. Reconnect to continue.")
    }

    func startServices() {
        guard !servicesStarted else { return }
        servicesStarted = true
        log("startServices()")
        log("local display: \(DeviceDisplayIdentity.current.name), EDID: \(DeviceDisplayIdentity.current.edidName)")
        startTrafficMonitoring()
        syncCameraCompressionQuality()
        if !isConnected && !isConnecting {
            applyConnectionHealth(.idle, detail: disconnectedPrompt())
        }
    }

    private func startTrafficMonitoring() {
        trafficTimer?.cancel()
        let timer = DispatchSource.makeTimerSource(queue: .main)
        timer.schedule(deadline: .now() + 1.0, repeating: 1.0)
        timer.setEventHandler { [weak self] in
            guard let self else { return }
            let (rxDelta, txDelta) = self.trafficCounter.readAndReset()
            self.stats.setRates(receive: formatByteRate(Double(rxDelta)), send: formatByteRate(Double(txDelta)))
        }
        timer.resume()
        trafficTimer = timer
    }

    private nonisolated func recordTraffic(rxBytes: Int = 0, txBytes: Int = 0) {
        if rxBytes > 0 || txBytes > 0 {
            trafficCounter.add(rx: UInt64(rxBytes), tx: UInt64(txBytes))
        }
    }

    private func updateSessionHostname(_ hostname: String) {
        guard !hostname.isEmpty else { return }
        sessionDisplayName = hostname
    }

    private func syncCameraCompressionQuality() {
        cameraCapture.compressionQuality = networkCameraCompressionQuality
    }

    func connectManual() {
        guard !isConnecting else {
            log("connectManual() ignored while already connecting")
            return
        }
        guard let target = parseManualEndpoint(manualHost) else { return }
        let endpoint = makeEndpoint(host: target.host, port: target.port)
        lastNetEndpoint = endpoint
        lastNetName = formatEndpointInput(host: target.host, port: target.port)
        connectToEndpoint(endpoint, name: lastNetName ?? target.host)
    }

    func connectRecent(_ recent: RecentConnection) {
        guard !isConnecting else {
            log("connectRecent() ignored while already connecting")
            return
        }
        manualHost = formatEndpointInput(host: recent.host, port: recent.port)
        let endpoint = makeEndpoint(host: recent.host, port: recent.port)
        lastNetEndpoint = endpoint
        lastNetName = recent.displayName
        connectToEndpoint(endpoint, name: recent.displayName)
    }

    func clearRecentConnections() {
        recentConnections = pinnedConnections
        persistRecentConnections()
        if !isConnected && !isConnecting {
            applyConnectionHealth(.idle, detail: disconnectedPrompt())
        }
    }

    func deleteConnection(_ connection: RecentConnection) {
        recentConnections.removeAll { $0.id == connection.id }
        persistRecentConnections()
        if lastNetEndpoint.map({ endpointHostAndPort($0, fallbackHost: lastNetName ?? "").host == connection.host && endpointHostAndPort($0, fallbackHost: lastNetName ?? "").port == connection.port }) == true {
            lastNetEndpoint = nil
            lastNetName = nil
        }
        if !isConnected && !isConnecting {
            applyConnectionHealth(.idle, detail: disconnectedPrompt())
        }
    }

    func togglePinned(_ connection: RecentConnection) {
        if !connection.isPinned && pinnedConnections.count >= Self.maxPinnedConnections {
            if isConnected || isConnecting {
                status = "Pinned connections are limited to 10."
            } else {
                applyConnectionHealth(.idle, detail: "Pinned connections are limited to 10.")
            }
            return
        }

        recentConnections = Self.normalizeConnections(
            recentConnections.map { existing in
                guard existing.id == connection.id else { return existing }
                return RecentConnection(
                    host: existing.host,
                    port: existing.port,
                    name: existing.name,
                    lastConnectedAt: existing.lastConnectedAt,
                    isPinned: !existing.isPinned
                )
            }
        )
        persistRecentConnections()

        if !isConnected && !isConnecting {
            applyConnectionHealth(.idle, detail: disconnectedPrompt())
        }
    }

    func connectToEndpoint(_ endpoint: NWEndpoint, name: String) {
        log("connectToEndpoint(name=\(name), endpoint=\(endpoint)) start; isConnected=\(isConnected) isConnecting=\(isConnecting)")
        // Detach old stream's callbacks so stale async events can't interfere
        stream?.onEvent = nil
        stream?.onDisconnect = nil
        stream?.disconnect()
        closeNetworkControl(gracefully: true)
        pairingService?.cancel()

        applyConnectionHealth(.connecting, detail: "Connecting to \(name).", transport: .network)
        sessionDisplayName = name
        _ = trafficCounter.readAndReset()

        // Extract host string from endpoint
        let host: String
        switch endpoint {
        case .hostPort(let h, _):
            host = "\(h)"
        default:
            host = name
        }

        let port: UInt16
        switch endpoint {
        case .hostPort(_, let p):
            port = p.rawValue
        default:
            port = defaultDaemonPort
        }

        // Step 1: TCP handshake for pairing/session key exchange
        let ps = PairingService()
        self.pairingService = ps
        log("starting PairingService for host=\(host) port=\(port)")

        ps.onResult = { [weak self] result in
            guard let self else { return }
            switch result {
            case .sessionEstablished(let key, let connection, let initialData):
                self.log("PairingService result: session established")
                self.sessionKey = key
                self.pairingService = nil

                let control = NetworkControlClient(connection: connection, sessionKey: key, initialData: initialData)
                control.onDisconnect = { [weak self, weak control] in
                    Task { @MainActor in
                        guard let self, let control else { return }
                        guard self.networkControl === control else { return }
                        self.log("NetworkControlClient onDisconnect")
                        self.networkControl = nil
                        self.handleStreamLost()
                    }
                }
                self.networkControl = control
                control.onTraffic = { [weak self] rxBytes, txBytes in
                    self?.recordTraffic(rxBytes: rxBytes, txBytes: txBytes)
                }
                control.onCapabilities = { [weak self, weak control] capabilities in
                    Task { @MainActor in
                        guard let self, let control else { return }
                        guard self.networkControl === control else { return }
                        self.handleDaemonCapabilities(capabilities)
                    }
                }
                control.onDisplayStatus = { [weak self, weak control] update in
                    Task { @MainActor in
                        guard let self, let control, self.networkControl === control else { return }
                        self.handleDisplayStatus(update)
                    }
                }
                control.onLatency = { [weak self, weak control] rttMs, jitterMs in
                    Task { @MainActor in
                        guard let self, let control else { return }
                        guard self.networkControl === control else { return }
                        self.stats.setLatency(rtt: Self.formatLatency(rttMs), jitter: Self.formatLatency(jitterMs))
                    }
                }
                self.beginCapabilityNegotiation(transport: .network)
                control.start()
                self.decoder.setSuspended(false)
                self.syncSpeakerPassthroughState()

                self.startEncryptedStream(endpoint: endpoint, name: name, sessionKey: key, controlClient: control)

            case .pinRequired(let completion):
                self.log("PairingService result: PIN required")
                self.pendingPinCompletion = completion
                self.pinInput = ""
                self.showPinEntry = true
                self.applyConnectionHealth(.pairing, detail: "Enter the PIN shown on the daemon.", transport: .network)
                self.pairingStatus = "Enter the PIN shown on the daemon"

            case .rejected(let reason):
                self.log("PairingService result: rejected (\(reason))")
                self.pairingService = nil
                self.applyConnectionFailure("Pairing rejected: \(reason)", transport: .none)

            case .error(let msg):
                self.log("PairingService result: error (\(msg))")
                self.pairingService = nil
                self.applyConnectionFailure("Pairing error: \(msg)", transport: .none)
            }
        }

        ps.pair(host: host, port: port)
    }

    func submitPin() {
        guard let completion = pendingPinCompletion else { return }
        let pin = pinInput.trimmingCharacters(in: .whitespacesAndNewlines)
        guard pin.count == 6, pin.allSatisfy({ $0.isNumber }) else {
            pairingStatus = "PIN must be exactly 6 digits"
            return
        }
        log("submitPin()")
        showPinEntry = false
        applyConnectionHealth(.pairing, detail: "Verifying the pairing PIN.", transport: .network)
        completion(pin)
        pendingPinCompletion = nil
    }

    func cancelPin() {
        log("cancelPin()")
        showPinEntry = false
        pendingPinCompletion = nil
        pairingService?.cancel()
        pairingService = nil
        applyConnectionHealth(.idle, detail: "Pairing cancelled. \(disconnectedPrompt())", transport: .none)
    }

    private func startEncryptedStream(endpoint: NWEndpoint, name: String, sessionKey: SymmetricKey, controlClient: NetworkControlClient) {
        log("startEncryptedStream(name=\(name), endpoint=\(endpoint))")
        applyConnectionHealth(.waitingForVideo, detail: "Waiting for video from \(name).", transport: .network)


        decoder.onVideoSizeChanged = { [weak self] width, height in
            Task { @MainActor in self?.internalVideoSize = CGSize(width: width, height: height) }
        }
        let client = StreamClient(endpoint: endpoint, decoder: decoder, externalDecoder: externalDecoder, audioPlayer: audioPlayer, avSync: avSync)
        client.sessionKey = sessionKey
        client.sendPliRequest = { [weak controlClient] display, generation in
            controlClient?.sendPli(display: display, generation: generation)
        }
        self.stream = client
        client.onVideoFrameReady = { [weak self, weak client] display, generation in
            Task { @MainActor in
                guard let self, let client, self.stream === client else { return }
                self.acknowledgeReadyDisplay(display, generation: generation)
            }
        }
        client.onTraffic = { [weak self] rxBytes, txBytes in
            self?.recordTraffic(rxBytes: rxBytes, txBytes: txBytes)
        }

        client.onEvent = { [weak self, weak client] event in
            Task { @MainActor in
                guard let self, let client else { return }
                guard self.stream === client else { return }
                self.log("StreamClient event: \(String(describing: event))")
                switch event {
                case .readyToRegister:
                    self.applyConnectionHealth(.waitingForVideo, detail: "Connected. Registering for network video.", transport: .network)
                case .waiting(let message):
                    let normalized = message.lowercased()
                    if normalized.contains("refused") || normalized.contains("timed out") || normalized.contains("timeout") {
                        self.applyConnectionFailure("Connection failed: \(message)", transport: .none)
                    } else {
                        self.applyConnectionHealth(.waitingForVideo, detail: "Waiting for video. \(message)", transport: .network)
                    }
                case .connectionFailed(let message):
                    self.applyConnectionFailure("Connection failed: \(message)", transport: .none)
                case .receiveError(let message):
                    self.applyConnectionFailure("Receive error: \(message)", transport: .none)
                case .timedOut:
                    self.applyConnectionHealth(.timedOut, detail: "Timed out waiting for the daemon's media stream.", transport: .none)
                case .firstFrame:
                    let target = self.endpointHostAndPort(endpoint, fallbackHost: name)
                    self.applyConnectionHealth(.streaming, transport: .network)
                    self.manualHost = formatEndpointInput(host: target.host, port: target.port)
                    let displayName = self.sessionDisplayName.isEmpty ? name : self.sessionDisplayName
                    self.rememberRecentConnection(name: displayName, host: target.host, port: target.port)
                    self.audioPlayer.start()
                    self.startPeripheralMonitoring()
                }
            }
        }
        client.onDisconnect = { [weak self, weak client] in
            Task { @MainActor in
                guard let self, let client else { return }
                guard self.stream === client else { return }
                self.log("StreamClient onDisconnect")
                self.stream = nil
                self.handleStreamLost()
            }
        }
        client.connect()
    }

    /// Called when we've lost all streams and should return to idle state.
    private func handleStreamLost() {
        log("handleStreamLost() lastNetEndpoint=\(String(describing: lastNetEndpoint)) lastNetName=\(String(describing: lastNetName))")
        let preservedHealth = connectionHealth
        let preservedStatus = status
        stream?.onEvent = nil
        stream?.onDisconnect = nil
        stream?.disconnect()
        stream = nil
        closeNetworkControl(gracefully: true)
        decoder.setSuspended(false)
        audioPlayer.stop()
        micCapture.stop()
        configureAudioSession()
        cameraCapture.stop()
        stopPeripheralMonitoring()
        resetCapabilityNegotiation()
        if preservedHealth.isTerminalFailure {
            applyConnectionHealth(preservedHealth, detail: preservedStatus, transport: .none)
        } else {
            applyConnectionHealth(.timedOut, detail: "The session ended. \(disconnectedPrompt())", transport: .none)
        }
    }

    func disconnect(detail: String? = nil) {
        // Tell the daemon to tear down virtual devices if active
        if cameraCapture.isRunning {
            sendCameraDisable()
        }
        if micCapture.isRunning {
            sendMicDisable()
        }
        // Speaker disable — the daemon will also clean up on disconnect,
        // but sending it explicitly ensures a clean teardown path.
        if audioPlayer.isOutputEnabled {
            sendSpeakerTransportState(isEnabled: false)
        }
        stream?.onEvent = nil
        stream?.onDisconnect = nil
        stream?.disconnect()
        stream = nil
        closeNetworkControl(gracefully: true)
        pairingService?.cancel()
        pairingService = nil
        decoder.setSuspended(false)
        audioPlayer.stop()
        micCapture.stop()
        configureAudioSession()
        cameraCapture.stop()
        stopPeripheralMonitoring()
        resetCapabilityNegotiation()
        lastNetEndpoint = nil
        lastNetName = nil
        sessionDisplayName = ""
        applyConnectionHealth(.idle, detail: detail ?? disconnectedPrompt(), transport: .none)
    }

    var displayLayer: AVSampleBufferDisplayLayer? {
        decoder.displayLayer
    }

    private func closeNetworkControl(gracefully: Bool) {
        let control = networkControl
        control?.onDisconnect = nil
        networkControl = nil
        if gracefully {
            control?.disconnectGracefully()
        } else {
            control?.disconnect()
        }
    }

    func sendTouch(_ data: Data) {
        networkControl?.sendTouch(data)
    }

    func sendKey(_ keyData: Data) {
        networkControl?.sendKey(keyData)
    }

    /// Builds a "text insert" key packet: type 0x01 + UTF-8 bytes
    func sendTextInsert(_ text: String) {
        var data = Data([0x01])
        data.append(Data(text.utf8))
        sendKey(data)
    }

    /// Builds a "special key" packet: type 0x02 + key code
    func sendSpecialKey(_ code: UInt8) {
        sendKey(Data([0x02, code]))
    }

    /// Sends a modifier combo with text: type 0x04 + mods(1) + inner_type 0x01 + UTF-8
    func sendComboText(mods: UInt8, text: String) {
        var data = Data([0x04, mods, 0x01])
        data.append(Data(text.utf8))
        sendKey(data)
    }

    /// Sends a modifier combo with special key: type 0x04 + mods(1) + inner_type 0x02 + code(1)
    func sendComboSpecial(mods: UInt8, code: UInt8) {
        sendKey(Data([0x04, mods, 0x02, code]))
    }

    // MARK: - Physical Mouse/Keyboard

    func sendMouse(_ mouseData: Data) {
        networkControl?.sendMouse(mouseData)
    }

    func sendRawKey(_ keyData: Data) {
        networkControl?.sendRawKey(keyData)
    }

    func sendPeripheral(_ periphData: Data) {
        networkControl?.sendPeripheral(periphData)
    }

    func sendGamepad(_ gamepadData: Data) {
        networkControl?.sendGamepad(gamepadData)
    }

    // MARK: - Peripheral Monitoring (GCMouse / GCKeyboard)

    func startPeripheralMonitoring() {
        stopPeripheralMonitoring()

        // iPadOS can report several pointing devices (an external mouse, a keyboard trackpad,
        // a Bluetooth mouse reconnecting after sleep). Drive the desktop from all of them.
        for mouse in GCMouse.mice() {
            attachMouse(mouse)
        }
        if let kb = GCKeyboard.coalesced {
            attachKeyboard(kb)
        }
        refreshControllerPassthrough()

        let mc = NotificationCenter.default.addObserver(
            forName: .GCMouseDidConnect, object: nil, queue: .main
        ) { [weak self] note in
            guard let self, let mouse = note.object as? GCMouse else { return }
            Task { @MainActor in self.attachMouse(mouse) }
        }
        let md = NotificationCenter.default.addObserver(
            forName: .GCMouseDidDisconnect, object: nil, queue: .main
        ) { [weak self] note in
            guard let self, let mouse = note.object as? GCMouse else { return }
            Task { @MainActor in self.detachMouse(mouse) }
        }
        let kc = NotificationCenter.default.addObserver(
            forName: .GCKeyboardDidConnect, object: nil, queue: .main
        ) { [weak self] note in
            guard let self, let kb = note.object as? GCKeyboard else { return }
            Task { @MainActor in self.attachKeyboard(kb) }
        }
        let kd = NotificationCenter.default.addObserver(
            forName: .GCKeyboardDidDisconnect, object: nil, queue: .main
        ) { [weak self] note in
            guard let self else { return }
            Task { @MainActor in self.detachKeyboard() }
        }
        let cc = NotificationCenter.default.addObserver(
            forName: .GCControllerDidConnect, object: nil, queue: .main
        ) { [weak self] note in
            guard let self, let controller = note.object as? GCController else { return }
            Task { @MainActor in self.attachController(controller) }
        }
        let cd = NotificationCenter.default.addObserver(
            forName: .GCControllerDidDisconnect, object: nil, queue: .main
        ) { [weak self] note in
            guard let self, let controller = note.object as? GCController else { return }
            Task { @MainActor in self.detachController(controller) }
        }
        mouseObservers = [mc, md]
        keyboardObservers = [kc, kd]
        controllerObservers = [cc, cd]
    }

    func stopPeripheralMonitoring() {
        setTrackpadEnabled(false)
        for obs in mouseObservers { NotificationCenter.default.removeObserver(obs) }
        for obs in keyboardObservers { NotificationCenter.default.removeObserver(obs) }
        for obs in controllerObservers { NotificationCenter.default.removeObserver(obs) }
        mouseObservers.removeAll()
        keyboardObservers.removeAll()
        controllerObservers.removeAll()
        for mouse in connectedMice.values { detachMouse(mouse) }
        detachKeyboard()
        detachAllControllers()
    }

    private func attachMouse(_ mouse: GCMouse) {
        let wasConnected = !connectedMice.isEmpty
        connectedMice[ObjectIdentifier(mouse)] = mouse
        if !wasConnected {
            physicalMouseConnected = true
            physicalMouseButtonMask = 0
            physicalMouseScrollAccumulator = 0
            sendPeripheral(Data([0x01, 0x01])) // PERIPH_MOUSE, ATTACHED
        }

        guard let input = mouse.mouseInput else { return }

        input.mouseMovedHandler = { [weak self] _, dx, dy in
            guard let self else { return }
            var data = Data([0x01]) // MOUSE_MOVE
            let idx = Int16(clamping: Int(dx)).bigEndian
            let idy = Int16(clamping: -Int(dy)).bigEndian
            withUnsafeBytes(of: idx) { data.append(contentsOf: $0) }
            withUnsafeBytes(of: idy) { data.append(contentsOf: $0) }
            Task { @MainActor in self.sendMouse(data) }
        }

        input.leftButton.pressedChangedHandler = { [weak self] _, _, pressed in
            guard let self else { return }
            Task { @MainActor in self.updatePhysicalMouseButton(button: 0x01, pressed: pressed) }
        }

        if let right = input.rightButton {
            right.pressedChangedHandler = { [weak self] _, _, pressed in
                guard let self else { return }
                Task { @MainActor in self.updatePhysicalMouseButton(button: 0x02, pressed: pressed) }
            }
        }

        if let middle = input.middleButton {
            middle.pressedChangedHandler = { [weak self] _, _, pressed in
                guard let self else { return }
                Task { @MainActor in self.updatePhysicalMouseButton(button: 0x04, pressed: pressed) }
            }
        }

        input.scroll.valueChangedHandler = { [weak self] _, _, deltaY in
            guard let self else { return }
            Task { @MainActor in self.handlePhysicalMouseScroll(value: deltaY) }
        }

        print("[periph] mouse attached")
    }

    /// Pointer lock and "mouse connected" state end only when the last pointing device is gone;
    /// another device disconnecting must not release the one still in use.
    private func detachMouse(_ mouse: GCMouse) {
        if let input = mouse.mouseInput {
            input.mouseMovedHandler = nil
            input.leftButton.pressedChangedHandler = nil
            input.rightButton?.pressedChangedHandler = nil
            input.middleButton?.pressedChangedHandler = nil
            input.scroll.valueChangedHandler = nil
        }
        guard connectedMice.removeValue(forKey: ObjectIdentifier(mouse)) != nil,
              connectedMice.isEmpty, physicalMouseConnected else { return }
        let previousMask = physicalMouseButtonMask | trackpadButtonMask
        physicalMouseButtonMask = 0
        physicalMouseScrollAccumulator = 0
        physicalMouseConnected = false
        sendMouseButtonChanges(from: previousMask)
        if !isTrackpadEnabled {
            sendPeripheral(Data([0x01, 0x00])) // PERIPH_MOUSE, DETACHED
        }
        print("[periph] mouse detached")
    }

    private func updatePhysicalMouseButton(button: UInt8, pressed: Bool) {
        let previousMask = physicalMouseButtonMask | trackpadButtonMask

        if pressed {
            physicalMouseButtonMask |= button
        } else {
            physicalMouseButtonMask &= ~button
        }

        sendMouseButtonChanges(from: previousMask)
    }

    /// The on-screen trackpad and physical mouse share the host's virtual mouse. Merge their
    /// held buttons, so releasing one source doesn't release a button held by the other.
    private func sendMouseButtonChanges(from previousMask: UInt8) {
        let currentMask = physicalMouseButtonMask | trackpadButtonMask
        for code: UInt8 in 0..<3 {
            let bit: UInt8 = 1 << code
            if (previousMask & bit) != (currentMask & bit) {
                sendMouse(Data([0x02, code, currentMask & bit != 0 ? 1 : 0]))
            }
        }
    }

    func setTrackpadEnabled(_ enabled: Bool) {
        guard enabled != isTrackpadEnabled else { return }
        if enabled {
            guard isConnected else { return }
            isTrackpadEnabled = true
            if !physicalMouseConnected {
                sendPeripheral(Data([0x01, 0x01]))
            }
        } else {
            let previousMask = physicalMouseButtonMask | trackpadButtonMask
            trackpadButtonMask = 0
            sendMouseButtonChanges(from: previousMask)
            isTrackpadEnabled = false
            if !physicalMouseConnected {
                sendPeripheral(Data([0x01, 0x00]))
            }
        }
        trackpadMotionRemainder = .zero
        trackpadScrollRemainder = 0
    }

    func moveTrackpad(dx: CGFloat, dy: CGFloat) {
        guard isTrackpadEnabled, isConnected else { return }
        trackpadMotionRemainder.width += dx
        trackpadMotionRemainder.height += dy
        let x = Int16(clamping: Int(trackpadMotionRemainder.width))
        let y = Int16(clamping: Int(trackpadMotionRemainder.height))
        trackpadMotionRemainder.width -= CGFloat(x)
        trackpadMotionRemainder.height -= CGFloat(y)
        guard x != 0 || y != 0 else { return }
        var data = Data([0x01])
        withUnsafeBytes(of: x.bigEndian) { data.append(contentsOf: $0) }
        withUnsafeBytes(of: y.bigEndian) { data.append(contentsOf: $0) }
        sendMouse(data)
    }

    func trackpadButton(_ code: UInt8, pressed: Bool) {
        guard isTrackpadEnabled, isConnected, code < 3 else { return }
        let previousMask = physicalMouseButtonMask | trackpadButtonMask
        let bit: UInt8 = 1 << code
        if pressed { trackpadButtonMask |= bit } else { trackpadButtonMask &= ~bit }
        sendMouseButtonChanges(from: previousMask)
    }

    func scrollTrackpad(deltaY: CGFloat) {
        guard isTrackpadEnabled, isConnected else { return }
        // Natural scrolling: a finger moving down moves the content down.
        trackpadScrollRemainder += deltaY / 18
        let steps = Int16(clamping: Int(trackpadScrollRemainder))
        guard steps != 0 else { return }
        trackpadScrollRemainder -= CGFloat(steps)
        var data = Data([0x03])
        withUnsafeBytes(of: steps.bigEndian) { data.append(contentsOf: $0) }
        sendMouse(data)
    }

    private func handlePhysicalMouseScroll(value: Float) {
        // GameController scroll values can be fractional; accumulate them and
        // emit whole wheel steps so Linux receives consistent REL_WHEEL events.
        physicalMouseScrollAccumulator += value

        var wholeSteps = Int(physicalMouseScrollAccumulator.rounded(.towardZero))
        guard wholeSteps != 0 else { return }

        if wholeSteps > 0 {
            physicalMouseScrollAccumulator -= Float(wholeSteps)
        } else {
            physicalMouseScrollAccumulator += Float(-wholeSteps)
        }

        wholeSteps = max(-32, min(wholeSteps, 32))
        var data = Data([0x03]) // MOUSE_SCROLL
        let dy = Int16(clamping: wholeSteps).bigEndian
        withUnsafeBytes(of: dy) { data.append(contentsOf: $0) }
        sendMouse(data)
    }

    func forwardPhysicalMouseScroll(deltaY: CGFloat) {
        handlePhysicalMouseScroll(value: Float(deltaY))
    }

    private func attachKeyboard(_ kb: GCKeyboard) {
        physicalKeyboardConnected = true
        sendPeripheral(Data([0x02, 0x01])) // PERIPH_KEYBOARD, ATTACHED

        guard let input = kb.keyboardInput else { return }

        input.keyChangedHandler = { [weak self] _, key, keyCode, pressed in
            guard let self else { return }
            var data = Data()
            let hid = UInt16(keyCode.rawValue).bigEndian
            withUnsafeBytes(of: hid) { data.append(contentsOf: $0) }
            data.append(pressed ? 1 : 0)
            Task { @MainActor in self.sendRawKey(data) }
        }

        print("[periph] keyboard attached")
    }

    private func detachKeyboard() {
        guard physicalKeyboardConnected else { return }
        physicalKeyboardConnected = false
        sendPeripheral(Data([0x02, 0x00])) // PERIPH_KEYBOARD, DETACHED
        print("[periph] keyboard detached")
    }

    private func attachController(_ controller: GCController) {
        guard isControllerPassthroughEnabled else {
            print("[gamepad] controller passthrough disabled, ignoring attach")
            return
        }
        let id = ObjectIdentifier(controller)
        guard controllerSlots[id] == nil else { return }
        guard controller.extendedGamepad != nil || controller.microGamepad != nil else {
            print("[gamepad] unsupported controller profile ignored")
            return
        }

        guard let slot = nextAvailableControllerSlot() else {
            print("[gamepad] no free virtual gamepad slot (max 4)")
            return
        }

        controllerSlots[id] = slot
        physicalControllerConnectedCount = controllerSlots.count
        sendGamepad(Data([slot, 0x01])) // ATTACHED

        if let gamepad = controller.extendedGamepad {
            gamepad.valueChangedHandler = { [weak self, weak controller] gamepad, _ in
                guard let self, let controller else { return }
                Task { @MainActor in self.sendExtendedGamepadState(controller: controller, gamepad: gamepad) }
            }
            sendExtendedGamepadState(controller: controller, gamepad: gamepad)
            print("[gamepad] controller attached in slot \(slot + 1) (extended)")
        } else if let gamepad = controller.microGamepad {
            gamepad.valueChangedHandler = { [weak self, weak controller] gamepad, _ in
                guard let self, let controller else { return }
                Task { @MainActor in self.sendMicroGamepadState(controller: controller, gamepad: gamepad) }
            }
            sendMicroGamepadState(controller: controller, gamepad: gamepad)
            print("[gamepad] controller attached in slot \(slot + 1) (micro)")
        }
    }

    private func detachController(_ controller: GCController) {
        let id = ObjectIdentifier(controller)
        guard let slot = controllerSlots.removeValue(forKey: id) else { return }

        controller.extendedGamepad?.valueChangedHandler = nil
        controller.microGamepad?.valueChangedHandler = nil

        sendGamepad(Data([slot, 0x00])) // DETACHED
        physicalControllerConnectedCount = controllerSlots.count
        print("[gamepad] controller detached from slot \(slot + 1)")
    }

    private func detachAllControllers() {
        for controller in GCController.controllers() {
            controller.extendedGamepad?.valueChangedHandler = nil
            controller.microGamepad?.valueChangedHandler = nil
        }
        for slot in controllerSlots.values {
            sendGamepad(Data([slot, 0x00]))
        }
        controllerSlots.removeAll()
        physicalControllerConnectedCount = 0
    }

    private func refreshControllerPassthrough() {
        if isControllerPassthroughEnabled {
            for controller in GCController.controllers() {
                attachController(controller)
            }
        } else {
            detachAllControllers()
        }
    }

    private func nextAvailableControllerSlot() -> UInt8? {
        let used = Set(controllerSlots.values)
        for slot in 0..<4 {
            let candidate = UInt8(slot)
            if !used.contains(candidate) {
                return candidate
            }
        }
        return nil
    }

    private func sendExtendedGamepadState(controller: GCController, gamepad: GCExtendedGamepad) {
        let id = ObjectIdentifier(controller)
        guard let slot = controllerSlots[id] else { return }

        var buttons: UInt16 = 0
        if gamepad.buttonA.isPressed { buttons |= 0x0001 }
        if gamepad.buttonB.isPressed { buttons |= 0x0002 }
        if gamepad.buttonX.isPressed { buttons |= 0x0004 }
        if gamepad.buttonY.isPressed { buttons |= 0x0008 }
        if gamepad.leftShoulder.isPressed { buttons |= 0x0010 }
        if gamepad.rightShoulder.isPressed { buttons |= 0x0020 }
        if gamepad.leftThumbstickButton?.isPressed == true { buttons |= 0x0040 }
        if gamepad.rightThumbstickButton?.isPressed == true { buttons |= 0x0080 }
        if gamepad.buttonOptions?.isPressed == true { buttons |= 0x0100 }
        if gamepad.buttonMenu.isPressed { buttons |= 0x0200 }
        if gamepad.buttonHome?.isPressed == true { buttons |= 0x0400 }

        let hatX: Int8 = gamepad.dpad.left.isPressed ? -1 : (gamepad.dpad.right.isPressed ? 1 : 0)
        let hatY: Int8 = gamepad.dpad.up.isPressed ? -1 : (gamepad.dpad.down.isPressed ? 1 : 0)

        print("[gamepad] send extended state slot=\(slot + 1) buttons=0x\(String(buttons, radix: 16)) lx=\(gamepad.leftThumbstick.xAxis.value) ly=\(gamepad.leftThumbstick.yAxis.value) rx=\(gamepad.rightThumbstick.xAxis.value) ry=\(gamepad.rightThumbstick.yAxis.value) lt=\(gamepad.leftTrigger.value) rt=\(gamepad.rightTrigger.value) hat=(\(hatX),\(hatY))")

        sendGamepadState(
            slot: slot,
            buttons: buttons,
            lx: gamepad.leftThumbstick.xAxis.value,
            ly: -gamepad.leftThumbstick.yAxis.value,
            rx: gamepad.rightThumbstick.xAxis.value,
            ry: -gamepad.rightThumbstick.yAxis.value,
            lt: gamepad.leftTrigger.value,
            rt: gamepad.rightTrigger.value,
            hatX: hatX,
            hatY: hatY
        )
    }

    private func sendMicroGamepadState(controller: GCController, gamepad: GCMicroGamepad) {
        let id = ObjectIdentifier(controller)
        guard let slot = controllerSlots[id] else { return }

        var buttons: UInt16 = 0
        if gamepad.buttonA.isPressed { buttons |= 0x0001 }
        if gamepad.buttonX.isPressed { buttons |= 0x0004 }
        if gamepad.buttonMenu.isPressed { buttons |= 0x0200 }

        let hatX: Int8 = gamepad.dpad.left.isPressed ? -1 : (gamepad.dpad.right.isPressed ? 1 : 0)
        let hatY: Int8 = gamepad.dpad.up.isPressed ? -1 : (gamepad.dpad.down.isPressed ? 1 : 0)

        print("[gamepad] send micro state slot=\(slot + 1) buttons=0x\(String(buttons, radix: 16)) hat=(\(hatX),\(hatY))")

        sendGamepadState(
            slot: slot,
            buttons: buttons,
            lx: 0,
            ly: 0,
            rx: 0,
            ry: 0,
            lt: 0,
            rt: 0,
            hatX: hatX,
            hatY: hatY
        )
    }

    private func sendGamepadState(
        slot: UInt8,
        buttons: UInt16,
        lx: Float,
        ly: Float,
        rx: Float,
        ry: Float,
        lt: Float,
        rt: Float,
        hatX: Int8,
        hatY: Int8
    ) {
        var data = Data([slot, 0x02]) // STATE
        withUnsafeBytes(of: buttons.bigEndian) { data.append(contentsOf: $0) }
        appendGamepadAxis(&data, lx)
        appendGamepadAxis(&data, ly)
        appendGamepadAxis(&data, rx)
        appendGamepadAxis(&data, ry)
        appendGamepadTrigger(&data, lt)
        appendGamepadTrigger(&data, rt)
        data.append(UInt8(bitPattern: hatX))
        data.append(UInt8(bitPattern: hatY))
        sendGamepad(data)
    }

    private func appendGamepadAxis(_ data: inout Data, _ value: Float) {
        let clamped = max(-1.0, min(1.0, value))
        let scaled = Int16(clamping: Int((clamped * 32767.0).rounded())).bigEndian
        withUnsafeBytes(of: scaled) { data.append(contentsOf: $0) }
    }

    private func appendGamepadTrigger(_ data: inout Data, _ value: Float) {
        let clamped = max(0.0, min(1.0, value))
        let scaled = UInt16((clamped * 1023.0).rounded()).bigEndian
        withUnsafeBytes(of: scaled) { data.append(contentsOf: $0) }
    }

    // MARK: - Camera

    /// True once `CAPS` reports camera support.
    var isCameraAvailable: Bool { daemonCapabilities?.camera ?? false }

    func toggleCamera() {
        guard isCameraAvailable else { return }
        if cameraCapture.isRunning {
            cameraCapture.stop()
            sendCameraDisable()
        } else {
            sendCameraEnable()
            cameraCapture.onJPEG = { [weak self] jpeg in
                guard let self else { return }
                let fid = self.camFrameId
                self.camFrameId = self.camFrameId &+ 1
                self.stream?.sendCameraFrame(jpeg, frameId: fid)
            }
            cameraCapture.start()
        }
        objectWillChange.send()
    }

    /// Tells the daemon to create the virtual webcam with our capture profile.
    private func sendCameraEnable() {
        let profile = cameraCapture.captureProfile
        let width = UInt16(profile.outputSize.width)
        let height = UInt16(profile.outputSize.height)
        let fps = UInt16(profile.targetFps)
        networkControl?.sendCameraConfig(width: width, height: height, fps: fps)
    }

    /// Tells the daemon to destroy the virtual webcam.
    private func sendCameraDisable() {
        networkControl?.sendCameraConfig(width: 0, height: 0, fps: 0)
    }

    var isCameraActive: Bool { cameraCapture.isRunning }
    var isCameraFront: Bool { cameraCapture.usingFront }

    func flipCamera() {
        cameraCapture.flipCamera()
        objectWillChange.send()
    }

    // MARK: - Microphone

    /// True once `CAPS` reports microphone support.
    var isMicAvailable: Bool { daemonCapabilities?.microphone ?? false }

    func toggleMic() {
        guard isMicAvailable else { return }
        let speakerWasEnabled = audioPlayer.isOutputEnabled
        if micCapture.isRunning {
            micCapture.stop()
            sendMicDisable()
            configureAudioSession()
            if speakerWasEnabled {
                resetSpeakerOutput()
            }
        } else {
            configureAudioSession(micActive: true)
            sendMicEnable()
            micCapture.onOpusPacket = { [weak self] opusData in
                guard let self else { return }
                let seq = self.micSeq
                self.micSeq = self.micSeq &+ 1

                self.stream?.sendMicPacket(opusData, seq: seq)
            }
            micCapture.start()
            if !micCapture.isRunning {
                sendMicDisable()
                configureAudioSession()
            }
            if speakerWasEnabled {
                resetSpeakerOutput()
            }
        }
        objectWillChange.send()
    }

    /// Re-synchronizes playback with the daemon by turning the speaker off and back on.
    /// This produces the off-on transition the daemon needs to restart its audio stream.
    private func resetSpeakerOutput() {
        guard audioPlayer.isOutputEnabled else { return }
        audioPlayer.setOutputEnabled(false)
        sendSpeakerTransportState(isEnabled: false)
        audioPlayer.setOutputEnabled(true)
        sendSpeakerTransportState(isEnabled: true)
    }

    var isMicActive: Bool { micCapture.isRunning }

    /// Tells the daemon to create the virtual microphone device.
    private func sendMicEnable() {
        networkControl?.sendMicState(isEnabled: true)
    }

    /// Tells the daemon to destroy the virtual microphone device.
    private func sendMicDisable() {
        networkControl?.sendMicState(isEnabled: false)
    }

    // MARK: - Speakers / Controllers

    /// True once `CAPS` reports speaker/system-audio support.
    var isSpeakerAvailable: Bool { daemonCapabilities?.speaker ?? false }

    func toggleSpeaker() {
        guard isSpeakerAvailable else { return }
        let isEnabled = !audioPlayer.isOutputEnabled
        audioPlayer.setOutputEnabled(isEnabled)
        sendSpeakerTransportState(isEnabled: isEnabled)
        objectWillChange.send()
    }

    var isSpeakerActive: Bool { audioPlayer.isOutputEnabled }

    /// True once `CAPS` reports a non-nil `gamepadMaxControllers` (nil there means
    /// gamepad passthrough is unsupported by this daemon).
    var isControllerAvailable: Bool { daemonCapabilities?.gamepadMaxControllers != nil }

    func toggleControllerPassthrough() {
        guard isControllerAvailable else { return }
        isControllerPassthroughEnabled.toggle()
        refreshControllerPassthrough()
    }
}

enum ToolbarOrientation: String {
    case horizontal, vertical
}

struct ContentView: View {
    @EnvironmentObject private var model: StreamViewModel
    @State private var showOverlay = true

    @State private var barPosition: CGPoint = Self.loadBarPosition()
    @State private var barOrientation: ToolbarOrientation = Self.loadBarOrientation()
    @State private var isToolbarExpanded: Bool = Self.loadToolbarExpanded()
    @State private var dragOffset: CGSize = .zero
    @State private var isDragging = false
    @State private var isKeyboardActive = false
    @State private var keyboardHeight: CGFloat = 0
    @State private var keyboardFrame: CGRect = .null
    @State private var preKeyboardY: CGFloat? = nil
    @State private var viewSize: CGSize = .zero
    @State private var pillSize: CGSize = CGSize(width: 80, height: 44)
    @State private var toolbarMessage: String? = nil
    @State private var showStreamSettings = false
    @State private var showDisplaySettings = false

    private static let btnSize: CGFloat = 32
    private static let btnSpacing: CGFloat = 6
    private static let edgeThreshold: CGFloat = 40
    private static let connectionRowHeight: CGFloat = 44

    private func connectionListHeight(for rowCount: Int) -> CGFloat {
        guard rowCount > 0 else { return 0 }
        let estimated = CGFloat(rowCount) * Self.connectionRowHeight
        return min(estimated, 220)
    }

    var body: some View {
        GeometryReader { geo in
            ZStack {
                Color.black.ignoresSafeArea()

                if model.displayRouting.internalVideoVisible, let layer = model.displayLayer {
                    VideoDisplayView(
                        layer: layer,
                        videoWidth: Int(model.internalVideoSize.width),
                        videoHeight: Int(model.internalVideoSize.height),
                        onTouch: { data in model.sendTouch(data) },
                        onScroll: { deltaY in model.forwardPhysicalMouseScroll(deltaY: deltaY) },
                        hidePointer: model.physicalMouseConnected,
                        lockPointer: model.physicalMouseConnected,
                        acceptsTouch: model.canForwardInternalTouch,
                        onWindowAttachmentChanged: { model.setDisplaySurfaceAttached(.internalDisplay, attached: $0) }
                    )
                    .ignoresSafeArea()
                } else if model.isConnected {
                    Group {
                        if model.isExternalControlPanelVisible {
                            ExternalControlPanel(
                                model: model,
                                onShowDisplays: { showDisplaySettings = true },
                                onMessage: { showToolbarMessage($0) }
                            )
                            .ignoresSafeArea(.container, edges: .bottom)
                            .onAppear { isKeyboardActive = false }
                        } else {
                            VStack(spacing: 16) {
                                Image(systemName: "display")
                                    .font(.system(size: 44))
                                    .foregroundStyle(.secondary)
                                    .accessibilityHidden(true)
                                Text("You are running only in the external display")
                                    .font(.headline)
                                Button {
                                    model.setLaptopMode(true)
                                } label: {
                                    Label("Switch to laptop mode", systemImage: "laptopcomputer")
                                }
                                .buttonStyle(.borderedProminent)
                            }
                            .foregroundStyle(.white)
                            .multilineTextAlignment(.center)
                            .padding(24)
                            .frame(maxWidth: .infinity, maxHeight: .infinity)
                        }
                    }
                    // No video view in External only, which normally owns pointer lock. Keep the
                    // iPad cursor hidden and locked so a physical mouse moves the remote pointer
                    // relatively instead of stopping at the iPad's screen edges.
                    .background(PointerLockRegion(locked: model.physicalMouseConnected).ignoresSafeArea())
                }

                // MARK: Connection screen (always visible when disconnected)
                if !model.isConnected {
                    VStack(alignment: .leading, spacing: 16) {
                        VStack(alignment: .leading, spacing: 4) {
                            Text("Screx")
                                .font(.title2.weight(.bold))
                            Text(model.connectionHealthTitle)
                                .font(.headline)
                            Text(model.status)
                                .font(.subheadline)
                                .foregroundStyle(.secondary)
                        }

                        HStack {
                            TextField("Daemon host or IP[:port]", text: $model.manualHost)
                                .textFieldStyle(.roundedBorder)
                                .autocorrectionDisabled()
                                .textInputAutocapitalization(.never)
                                .keyboardType(.URL)
                                .disabled(model.isConnecting)

                            Button(
                                model.connectionHealth == .pairing
                                    ? "Pairing…"
                                    : model.connectionHealth == .waitingForVideo && model.transport == ConnectionTransport.network.rawValue
                                        ? "Waiting…"
                                        : model.isConnecting
                                            ? "Connecting…"
                                            : "Connect"
                            ) { model.connectManual() }
                                .buttonStyle(.borderedProminent)
                                .disabled(model.isConnecting || model.manualHost.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                        }

                        Button {
                            showStreamSettings = true
                        } label: {
                            HStack(spacing: 6) {
                                Image(systemName: "slider.horizontal.3")
                                Text("Stream Settings")
                            }
                            .frame(maxWidth: .infinity)
                        }
                        .buttonStyle(.bordered)

                        if !model.pinnedConnections.isEmpty {
                            VStack(alignment: .leading, spacing: 6) {
                                Text("Pinned")
                                    .font(.caption.weight(.medium))
                                    .foregroundStyle(.secondary)
                                    .textCase(.uppercase)

                                List {
                                    ForEach(model.pinnedConnections) { connection in
                                        connectionRow(connection)
                                    }
                                }
                                .listStyle(.plain)
                                .scrollDisabled(true)
                                .environment(\.defaultMinListRowHeight, Self.connectionRowHeight)
                                .frame(height: connectionListHeight(for: model.pinnedConnections.count))
                                .clipShape(RoundedRectangle(cornerRadius: 10))
                            }
                        }

                        if !model.unpinnedRecentConnections.isEmpty {
                            VStack(alignment: .leading, spacing: 6) {
                                HStack(alignment: .center) {
                                    Text("Recent")
                                        .font(.caption.weight(.medium))
                                        .foregroundStyle(.secondary)
                                        .textCase(.uppercase)
                                    Spacer()
                                    Button {
                                        model.clearRecentConnections()
                                    } label: {
                                        Text("Clear All")
                                            .font(.caption2.weight(.semibold))
                                            .foregroundStyle(.white)
                                            .padding(.horizontal, 10)
                                            .padding(.vertical, 4)
                                            .background(Color.red, in: Capsule())
                                    }
                                    .buttonStyle(.plain)
                                }

                                List {
                                    ForEach(model.unpinnedRecentConnections) { connection in
                                        connectionRow(connection)
                                    }
                                }
                                .listStyle(.plain)
                                .scrollDisabled(true)
                                .environment(\.defaultMinListRowHeight, Self.connectionRowHeight)
                                .frame(height: connectionListHeight(for: model.unpinnedRecentConnections.count))
                                .clipShape(RoundedRectangle(cornerRadius: 10))
                            }
                        }
                    }
                    .padding(24)
                    .frame(maxWidth: 400)
                    .background(Color(.systemBackground), in: RoundedRectangle(cornerRadius: 16))
                    .shadow(color: .black.opacity(0.08), radius: 12, y: 4)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .background(Color(.systemGroupedBackground).ignoresSafeArea())
                    .transition(.opacity)
                }

                // MARK: Info overlay (toggleable when connected)
                // Laptop mode shows connection info inline in its top bar.
                if model.isConnected && showOverlay && !model.isExternalControlPanelVisible {
                    StreamInfoView(model: model)
                    .padding(.horizontal, 16)
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
                    .padding(.top, model.isExternalControlPanelVisible ? 64 : 8)
                    .transition(.move(edge: .top).combined(with: .opacity))
                }

                if let toolbarMessage {
                    Text(toolbarMessage)
                        .font(.caption.weight(.medium))
                        .padding(.horizontal, 10)
                        .padding(.vertical, 6)
                        .background(.ultraThinMaterial, in: Capsule())
                        .foregroundStyle(.white)
                        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
                        .padding(.top, showOverlay && model.isConnected ? 92 : 16)
                        .transition(.move(edge: .top).combined(with: .opacity))
                }

                KeyboardInputView(
                    isActive: $isKeyboardActive,
                    physicalKeyboardActive: model.physicalKeyboardConnected,
                    onText: { model.sendTextInsert($0) },
                    onDelete: { model.sendSpecialKey(0x01) },
                    onSpecial: { model.sendSpecialKey($0) },
                    onCombo: { mods, text in model.sendComboText(mods: mods, text: text) },
                    onModSpecial: { mods, code in model.sendComboSpecial(mods: mods, code: code) }
                )
                .frame(width: 0, height: 0)

                if model.isConnected && !model.isExternalControlPanelVisible {
                    if model.isTrackpadEnabled {
                        TrackpadOverlay(
                            containerSize: geo.size,
                            keyboardFrame: keyboardFrame.offsetBy(
                                dx: -geo.frame(in: .global).minX,
                                dy: -geo.frame(in: .global).minY
                            ),
                            onMove: { dx, dy in model.moveTrackpad(dx: dx, dy: dy) },
                            onButton: { code, pressed in model.trackpadButton(code, pressed: pressed) },
                            onScroll: { model.scrollTrackpad(deltaY: $0) }
                        )
                        .id(model.inputLayoutRevision)
                        .frame(width: geo.size.width, height: geo.size.height)
                    }
                    floatingBar(in: geo)
                }
            }
            .onAppear {
                viewSize = geo.size
                clampBarPosition(in: geo.size, persist: barPosition != .zero)
            }
            .onChange(of: geo.size) { newSize in
                viewSize = newSize
                clampBarPosition(in: newSize)
            }
            .onChange(of: pillSize) { _ in
                clampBarPosition(in: geo.size)
            }
            .onChange(of: model.externalDisplayAvailable) { available in
                if available && model.isConnected {
                    showToolbarMessage("External display connected. Open Displays to choose an output.")
                }
            }
            .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillChangeFrameNotification)) { notif in
                guard let frame = notif.userInfo?[UIResponder.keyboardFrameEndUserInfoKey] as? CGRect else { return }
                let overlap = frame.intersection(geo.frame(in: .global))
                let kbH = overlap.isEmpty ? 0 : max(0, geo.frame(in: .global).maxY - overlap.minY)
                let duration = notif.userInfo?[UIResponder.keyboardAnimationDurationUserInfoKey] as? Double ?? 0.25
                if kbH > 0 && preKeyboardY == nil { preKeyboardY = barPosition.y }
                withAnimation(.easeOut(duration: duration)) {
                    keyboardFrame = frame
                    keyboardHeight = kbH
                    if kbH == 0, let savedY = preKeyboardY {
                        barPosition = clampedBarPosition(CGPoint(x: barPosition.x, y: savedY), in: geo.size, keyboardHeight: 0)
                        preKeyboardY = nil
                    } else {
                        clampBarPosition(in: geo.size, persist: false)
                    }
                }
            }
            .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillHideNotification)) { _ in
                withAnimation(.easeOut(duration: 0.25)) {
                    keyboardFrame = .null
                    keyboardHeight = 0
                    let y = preKeyboardY ?? barPosition.y
                    barPosition = clampedBarPosition(CGPoint(x: barPosition.x, y: y), in: geo.size, keyboardHeight: 0)
                    preKeyboardY = nil
                }
            }
        }
        // Our floating controls handle keyboard occlusion themselves. Keeping the container
        // full-height avoids SwiftUI shrinking/shifting it a second time underneath them.
        .ignoresSafeArea(.keyboard)
        .sheet(isPresented: $showDisplaySettings) {
            DisplaySettingsView(model: model)
        }
        .alert("Couldn’t switch displays", isPresented: Binding(
            get: { model.displayConfigurationError != nil && !showDisplaySettings },
            set: { if !$0 { model.clearDisplayConfigurationError() } }
        )) {
            Button("OK") { model.clearDisplayConfigurationError() }
        } message: {
            Text(model.displayConfigurationError ?? "The previous display configuration was kept.")
        }
        .sheet(isPresented: $model.showPinEntry) {
            VStack(spacing: 20) {
                Text("Pairing Required")
                    .font(.title2.bold())

                Text(model.pairingStatus)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)

                TextField("000000", text: $model.pinInput)
                    .font(.system(size: 32, weight: .bold, design: .monospaced))
                    .multilineTextAlignment(.center)
                    .keyboardType(.numberPad)
                    .frame(maxWidth: 200)
                    .textFieldStyle(.roundedBorder)

                HStack(spacing: 16) {
                    Button("Cancel") { model.cancelPin() }
                        .buttonStyle(.bordered)

                    Button("Pair") { model.submitPin() }
                        .buttonStyle(.borderedProminent)
                        .disabled(model.pinInput.count != 6)
                }
            }
            .padding(32)
            .interactiveDismissDisabled()
        }
        .sheet(isPresented: $showStreamSettings) {
            StreamSettingsSheet(
                resolution: model.preferredResolution,
                framerate: model.preferredFramerate,
                codec: model.preferredCodec,
                bitrate: model.preferredBitrate,
                customBitrateMbps: model.customBitrateMbps,
                onSave: { resolution, framerate, codec, bitrate, customBitrateMbps in
                    model.preferredResolution = resolution
                    model.preferredFramerate = framerate
                    model.preferredCodec = codec
                    model.preferredBitrate = bitrate
                    if let customBitrateMbps {
                        model.customBitrateMbps = customBitrateMbps
                    }
                    model.persistStreamSettingsPreferences()
                }
            )
            // The parent ContentView re-renders ~every second from chatty model
            // traffic/health updates. Wrapping the sheet in .equatable() and
            // comparing only the seeded initial values lets SwiftUI skip those
            // identical parent-driven re-renders while @State-driven user edits
            // still invalidate the view normally.
            .equatable()
        }
        .statusBarHidden(true)
        .persistentSystemOverlays(.hidden)
        .onChange(of: model.physicalKeyboardConnected) { connected in
            if connected {
                isKeyboardActive = false
            }
        }
    }

    // MARK: - Floating button bar

    @ViewBuilder
    private func floatingBar(in geo: GeometryProxy) -> some View {
        let layout = barOrientation == .vertical
            ? AnyLayout(VStackLayout(spacing: Self.btnSpacing))
            : AnyLayout(HStackLayout(spacing: Self.btnSpacing))

        let pos = clampedBarPosition(
            CGPoint(x: barPosition.x + dragOffset.width, y: barPosition.y + dragOffset.height),
            in: geo.size
        )

        layout {
            if model.isConnected {
                toolbarButton(
                    icon: barOrientation == .vertical
                        ? (isToolbarExpanded ? "chevron.down" : "chevron.up")
                        : (isToolbarExpanded ? "chevron.right" : "chevron.left"),
                    color: .white
                ) {
                    let nextExpanded = !isToolbarExpanded
                    withAnimation(.easeInOut(duration: 0.2)) {
                        isToolbarExpanded = nextExpanded
                    }
                    Self.saveToolbarExpanded(nextExpanded)
                    DispatchQueue.main.async {
                        clampBarPosition(in: geo.size)
                    }
                }

                if isToolbarExpanded {
                    toolbarButton(
                        icon: "rectangle.and.hand.point.up.left",
                        active: model.isTrackpadEnabled,
                        color: model.isTrackpadEnabled ? .green : .white
                    ) {
                        model.setTrackpadEnabled(!model.isTrackpadEnabled)
                    }
                    .accessibilityLabel(model.isTrackpadEnabled ? "Hide trackpad" : "Show trackpad")
                    .accessibilityHint("Move the pointer, tap to click, or use two fingers to scroll")

                    toolbarButton(
                        icon: model.isControllerPassthroughEnabled ? "gamecontroller.fill" : "gamecontroller",
                        active: model.isControllerPassthroughEnabled,
                        color: !model.isControllerAvailable ? .gray : (model.isControllerPassthroughEnabled ? .green : .white)
                    ) {
                        if model.isControllerAvailable {
                            model.toggleControllerPassthrough()
                        } else {
                            showToolbarMessage("Gamepad passthrough not supported by this daemon")
                        }
                    }

                    toolbarButton(
                        icon: model.isCameraActive
                            ? (model.isCameraFront ? "arrow.triangle.2.circlepath.camera.fill" : "video.fill")
                            : "video",
                        active: model.isCameraActive,
                        color: !model.isCameraAvailable ? .gray : (model.isCameraActive ? .green : .white)
                    ) {
                        if model.isCameraAvailable {
                            model.toggleCamera()
                        } else {
                            showToolbarMessage("Camera not supported by this daemon")
                        }
                    }
                        .onLongPressGesture(minimumDuration: 0.5) { model.flipCamera() }

                    toolbarButton(
                        icon: model.isMicActive ? "mic.fill" : "mic",
                        active: model.isMicActive,
                        color: !model.isMicAvailable ? .gray : (model.isMicActive ? .green : .white)
                    ) {
                        if model.isMicAvailable {
                            model.toggleMic()
                        } else {
                            showToolbarMessage("Microphone not supported by this daemon")
                        }
                    }

                    toolbarButton(
                        icon: model.isSpeakerActive ? "speaker.wave.2.fill" : "speaker.slash.fill",
                        active: model.isSpeakerActive,
                        color: !model.isSpeakerAvailable ? .gray : (model.isSpeakerActive ? .green : .white)
                    ) {
                        if model.isSpeakerAvailable {
                            model.toggleSpeaker()
                        } else {
                            showToolbarMessage("Speaker not supported by this daemon")
                        }
                    }
                }

                if model.externalDisplayAvailable || model.displayRouting.committedMask & 2 != 0 {
                    Button {
                        showDisplaySettings = true
                    } label: {
                        Image(systemName: model.isChangingDisplays ? "arrow.triangle.2.circlepath" : "display.2")
                            .font(.footnote)
                            .foregroundStyle(model.isChangingDisplays ? .orange : .white)
                            .frame(width: Self.btnSize, height: Self.btnSize)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel("Displays")
                    .accessibilityValue(model.isChangingDisplays ? "Switching displays" : model.selectedDisplayMode.label)
                    .accessibilityHint("Choose internal, external, or both displays")
                }

                toolbarButton(
                    icon: isKeyboardActive ? "keyboard.fill" : "keyboard",
                    active: isKeyboardActive && !model.physicalKeyboardConnected,
                    color: model.physicalKeyboardConnected ? .gray : (isKeyboardActive ? .green : .white)
                ) {
                    if model.physicalKeyboardConnected {
                        showToolbarMessage("External keyboard detected")
                    } else {
                        isKeyboardActive.toggle()
                    }
                }

                toolbarButton(
                    icon: showOverlay ? "info.circle.fill" : "info.circle",
                    active: showOverlay,
                    color: showOverlay ? .green : .white
                ) {
                    withAnimation(.easeInOut(duration: 0.2)) { showOverlay.toggle() }
                }
            }
        }
        .padding(4)
        .background(.ultraThinMaterial, in: Capsule())
        .contentShape(Capsule())
        .opacity(isDragging ? 0.8 : 1)
        .background(
            GeometryReader { pillGeo in
                Color.clear.onAppear { pillSize = pillGeo.size }
                    .onChange(of: pillGeo.size) { newSize in
                        DispatchQueue.main.async {
                            pillSize = newSize
                            clampBarPosition(in: geo.size)
                        }
                    }
                    .onChange(of: model.isConnected) { _ in
                        DispatchQueue.main.async {
                            pillSize = pillGeo.size
                            clampBarPosition(in: geo.size)
                        }
                    }
                    .onChange(of: isToolbarExpanded) { _ in
                        DispatchQueue.main.async {
                            pillSize = pillGeo.size
                            withAnimation(.easeInOut(duration: 0.2)) {
                                clampBarPosition(in: geo.size)
                            }
                        }
                    }
            }
        )
        .position(pos)
        .gesture(
            DragGesture(minimumDistance: 5, coordinateSpace: .global)
                .onChanged { value in
                    isDragging = true
                    dragOffset = value.translation
                }
                .onEnded { value in
                    isDragging = false
                    var newPos = CGPoint(
                        x: barPosition.x + value.translation.width,
                        y: barPosition.y + value.translation.height
                    )

                    var newOrientation = barOrientation
                    if newPos.x < Self.edgeThreshold {
                        newOrientation = .vertical
                    } else if newPos.y < Self.edgeThreshold {
                        newOrientation = .horizontal
                    } else if newPos.y > geo.size.height - Self.edgeThreshold {
                        newOrientation = .horizontal
                    }

                    newPos = clampedBarPosition(newPos, in: geo.size)
                    preKeyboardY = nil

                    barPosition = newPos
                    dragOffset = .zero

                    if newOrientation != barOrientation {
                        withAnimation(.easeOut(duration: 0.2)) {
                            barOrientation = newOrientation
                        }
                    }

                    Self.saveBarPosition(newPos)
                    Self.saveBarOrientation(newOrientation)
                }
        )
    }

    @ViewBuilder
    private func toolbarButton(icon: String, active: Bool = false, color: Color, action: @escaping () -> Void) -> some View {
        Image(systemName: icon)
            .font(.footnote)
            .foregroundStyle(color)
            .frame(width: Self.btnSize, height: Self.btnSize)
            .contentShape(Circle())
            .onTapGesture(perform: action)
    }

    private func showToolbarMessage(_ message: String) {
        withAnimation(.easeInOut(duration: 0.2)) {
            toolbarMessage = message
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.8) {
            guard toolbarMessage == message else { return }
            withAnimation(.easeInOut(duration: 0.2)) {
                toolbarMessage = nil
            }
        }
    }

    @ViewBuilder
    private func connectionRow(_ connection: RecentConnection) -> some View {
        Button {
            model.connectRecent(connection)
        } label: {
            HStack(spacing: 10) {
                Image(systemName: connection.isPinned ? "star.fill" : "clock.arrow.circlepath")
                    .font(.system(size: 14))
                    .foregroundStyle(connection.isPinned ? .orange : Color(.systemGray3))
                    .frame(width: 20)

                VStack(alignment: .leading, spacing: 1) {
                    Text(connection.displayName)
                        .font(.subheadline.weight(.medium))
                        .foregroundStyle(.primary)
                    Text(connection.endpointLabel)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }

                Spacer()

                Image(systemName: "chevron.right")
                    .font(.caption2.weight(.semibold))
                    .foregroundStyle(Color(.systemGray3))
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(model.isConnecting)
        .listRowInsets(EdgeInsets(top: 0, leading: 12, bottom: 0, trailing: 12))
        .swipeActions(edge: .leading, allowsFullSwipe: false) {
            Button(connection.isPinned ? "Unpin" : "Pin") {
                model.togglePinned(connection)
            }
            .tint(.orange)
        }
        .swipeActions(edge: .trailing, allowsFullSwipe: false) {
            Button("Delete", role: .destructive) {
                model.deleteConnection(connection)
            }
        }
    }

    private func defaultBarPosition(in size: CGSize) -> CGPoint {
        let halfW = pillSize.width / 2 + 4
        let halfH = pillSize.height / 2 + 4
        return CGPoint(x: size.width - halfW, y: size.height - halfH)
    }

    private func clampedBarPosition(_ position: CGPoint, in size: CGSize, keyboardHeight: CGFloat? = nil) -> CGPoint {
        guard size.width > 0, size.height > 0 else { return position }

        let halfW = pillSize.width / 2 + 4
        let halfH = pillSize.height / 2 + 4
        let activeKeyboardHeight = keyboardHeight ?? self.keyboardHeight

        let minX = halfW
        let maxX = max(halfW, size.width - halfW)
        let minY = halfH
        let maxVisibleY = size.height - activeKeyboardHeight - halfH
        let maxY = max(halfH, maxVisibleY)

        return CGPoint(
            x: min(max(position.x, minX), maxX),
            y: min(max(position.y, minY), maxY)
        )
    }

    private func clampBarPosition(in size: CGSize, persist: Bool = true) {
        let target = barPosition == .zero ? defaultBarPosition(in: size) : barPosition
        let clamped = clampedBarPosition(target, in: size)
        guard clamped != barPosition else { return }
        barPosition = clamped
        if persist {
            Self.saveBarPosition(clamped)
        }
    }

    // MARK: - Persistence

    private static let posXKey = "screx_bar_x"
    private static let posYKey = "screx_bar_y"
    private static let orientKey = "screx_bar_orient"
    private static let expandedKey = "screx_bar_expanded"

    private static func loadBarPosition() -> CGPoint {
        let defaults = UserDefaults.standard
        guard defaults.object(forKey: posXKey) != nil else { return .zero }
        return CGPoint(x: defaults.double(forKey: posXKey), y: defaults.double(forKey: posYKey))
    }

    private static func saveBarPosition(_ pos: CGPoint) {
        let defaults = UserDefaults.standard
        defaults.set(pos.x, forKey: posXKey)
        defaults.set(pos.y, forKey: posYKey)
    }

    private static func loadBarOrientation() -> ToolbarOrientation {
        let raw = UserDefaults.standard.string(forKey: orientKey) ?? "horizontal"
        return ToolbarOrientation(rawValue: raw) ?? .horizontal
    }

    private static func saveBarOrientation(_ orient: ToolbarOrientation) {
        UserDefaults.standard.set(orient.rawValue, forKey: orientKey)
    }

    private static func loadToolbarExpanded() -> Bool {
        UserDefaults.standard.object(forKey: expandedKey) != nil
            ? UserDefaults.standard.bool(forKey: expandedKey)
            : false
    }

    private static func saveToolbarExpanded(_ expanded: Bool) {
        UserDefaults.standard.set(expanded, forKey: expandedKey)
    }
}

/// Stream Settings sheet content, extracted out of `ContentView` so it does not observe
/// `StreamViewModel` at all. `ContentView` re-renders roughly every second from the
/// model's chatty `@Published` properties (connection health, discovery, stats); while a
/// `.pickerStyle(.menu)` menu is open, each re-render used to rebuild the menu content and
/// snap its scroll position back to the top. This view only depends on local `@State`
/// seeded once from the model when the sheet is presented, so it never re-renders because
/// of model changes.
private struct StreamSettingsSheet: View, Equatable {
    @Environment(\.dismiss) private var dismiss

    // Seeded values passed by the parent. These never change during the lifetime
    // of the sheet, so they are used only for Equatable comparison to skip
    // parent-driven re-renders caused by the chatty StreamViewModel.
    private let initialResolution: StreamResolutionPreset
    private let initialFramerate: StreamFrameratePreset
    private let initialCodec: StreamCodecPreset
    private let initialBitrate: StreamBitratePreset
    private let initialCustomBitrateMbps: Double

    @State private var resolution: StreamResolutionPreset
    @State private var framerate: StreamFrameratePreset
    @State private var codec: StreamCodecPreset
    @State private var bitrate: StreamBitratePreset
    @State private var customBitrateText: String

    let onSave: (StreamResolutionPreset, StreamFrameratePreset, StreamCodecPreset, StreamBitratePreset, Double?) -> Void

    static func == (lhs: StreamSettingsSheet, rhs: StreamSettingsSheet) -> Bool {
        lhs.initialResolution == rhs.initialResolution
            && lhs.initialFramerate == rhs.initialFramerate
            && lhs.initialCodec == rhs.initialCodec
            && lhs.initialBitrate == rhs.initialBitrate
            && lhs.initialCustomBitrateMbps == rhs.initialCustomBitrateMbps
    }

    init(
        resolution: StreamResolutionPreset,
        framerate: StreamFrameratePreset,
        codec: StreamCodecPreset,
        bitrate: StreamBitratePreset,
        customBitrateMbps: Double,
        onSave: @escaping (StreamResolutionPreset, StreamFrameratePreset, StreamCodecPreset, StreamBitratePreset, Double?) -> Void
    ) {
        self.initialResolution = resolution
        self.initialFramerate = framerate
        self.initialCodec = codec
        self.initialBitrate = bitrate
        self.initialCustomBitrateMbps = customBitrateMbps
        _resolution = State(initialValue: resolution)
        _framerate = State(initialValue: framerate)
        _codec = State(initialValue: codec)
        _bitrate = State(initialValue: bitrate)
        _customBitrateText = State(initialValue: String(format: "%.1f", customBitrateMbps))
        self.onSave = onSave
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 20) {
            Text("Stream Settings")
                .font(.title2.bold())

            Text("Applied on your next connection. If a setting is beyond what the daemon supports, the connection will fail instead of being adjusted automatically.")
                .font(.subheadline)
                .foregroundStyle(.secondary)

            VStack(alignment: .leading, spacing: 14) {
                streamSettingRow(title: "Resolution") {
                    Picker("Resolution", selection: $resolution) {
                        ForEach(StreamResolutionPreset.pickerCases) { preset in
                            Text(preset.label).tag(preset)
                        }
                    }
                    .labelsHidden()
                    .pickerStyle(.menu)
                }

                streamSettingRow(title: "Framerate") {
                    Picker("Framerate", selection: $framerate) {
                        ForEach(StreamFrameratePreset.allCases) { preset in
                            Text(preset.label).tag(preset)
                        }
                    }
                    .labelsHidden()
                    .pickerStyle(.menu)
                }

                streamSettingRow(title: "Codec") {
                    Picker("Codec", selection: $codec) {
                        ForEach(StreamCodecPreset.allCases) { preset in
                            Text(preset.label).tag(preset)
                        }
                    }
                    .labelsHidden()
                    .pickerStyle(.menu)
                }

                streamSettingRow(title: "Bitrate") {
                    HStack(spacing: 8) {
                        Picker("Bitrate", selection: $bitrate) {
                            ForEach(StreamBitratePreset.allCases) { preset in
                                Text(preset.label).tag(preset)
                            }
                        }
                        .labelsHidden()
                        .pickerStyle(.menu)

                        if bitrate == .custom {
                            HStack(spacing: 4) {
                                TextField("Mbps", text: $customBitrateText)
                                    .keyboardType(.decimalPad)
                                    .multilineTextAlignment(.trailing)
                                    .frame(width: 64)
                                    .textFieldStyle(.roundedBorder)
                                Text("Mbps")
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                        }
                    }
                }
            }

            Button("Done") { dismiss() }
                .buttonStyle(.borderedProminent)
                .frame(maxWidth: .infinity)
        }
        .padding(32)
        .onDisappear {
            let parsedCustomBitrate: Double? = {
                guard bitrate == .custom, let parsed = Double(customBitrateText), parsed.isFinite, parsed > 0 else {
                    return nil
                }
                return parsed
            }()
            onSave(resolution, framerate, codec, bitrate, parsedCustomBitrate)
        }
    }

    @ViewBuilder
    private func streamSettingRow<Content: View>(title: String, @ViewBuilder content: () -> Content) -> some View {
        HStack {
            Text(title)
                .font(.subheadline.weight(.medium))
            Spacer()
            content()
        }
    }
}

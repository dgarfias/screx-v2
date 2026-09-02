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

    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil
    ) -> Bool {
        model.configureAudioSession()
        return true
    }

    func application(
        _ application: UIApplication,
        configurationForConnecting connectingSceneSession: UISceneSession,
        options: UIScene.ConnectionOptions
    ) -> UISceneConfiguration {
        let config = UISceneConfiguration(name: "Default Configuration", sessionRole: connectingSceneSession.role)
        config.delegateClass = SceneDelegate.self
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
/// means "omit the resolution TLV entry from `STNG`" (let the daemon pick). `native`
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
        model.physicalMouseConnected
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
    }

    private func updatePhysicalMouseCapture() {
        let captureActive = model.physicalMouseConnected
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

/// Owns the receive/send byte counters and publishes the formatted rate
/// strings once a second. Isolated behind its own `ObservableObject` so the
/// 1s traffic tick only invalidates the small info overlay that reads it —
/// never the whole `ContentView` (which would tear down an open settings
/// sheet or interrupt a picker).
@MainActor
final class TrafficMonitor: ObservableObject {
    @Published private(set) var rxText: String = "0 B/s"
    @Published private(set) var txText: String = "0 B/s"

    private let counter = TrafficCounter()
    private var timer: DispatchSourceTimer?

    func start() {
        guard timer == nil else { return }
        let timer = DispatchSource.makeTimerSource(queue: .main)
        timer.schedule(deadline: .now() + 1.0, repeating: 1.0)
        timer.setEventHandler { [weak self] in
            guard let self else { return }
            let (rx, tx) = self.counter.readAndReset()
            let rxText = formatByteRate(Double(rx))
            let txText = formatByteRate(Double(tx))
            // Only publish on an actual change so the timer never invalidates
            // subscribers while the rates read the same.
            if rxText != self.rxText { self.rxText = rxText }
            if txText != self.txText { self.txText = txText }
        }
        timer.resume()
        self.timer = timer
    }

    func add(rxBytes: Int = 0, txBytes: Int = 0) {
        guard rxBytes > 0 || txBytes > 0 else { return }
        counter.add(rx: UInt64(rxBytes), tx: UInt64(txBytes))
    }

    func reset() {
        _ = counter.readAndReset()
        rxText = "0 B/s"
        txText = "0 B/s"
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
    /// Control-channel round-trip time from the in-session `PING` probe.
    @Published private(set) var latencyText: String = "—"
    /// Inter-sample variation of the control-channel RTT.
    @Published private(set) var jitterText: String = "—"

    /// What the currently-connected daemon advertised via `CAPS`. Nil until a `CAPS`
    /// message arrives (or the 2s timeout fallback fires, see `beginCapabilityNegotiation`).
    @Published private(set) var daemonCapabilities: DaemonCapabilities?

    /// Stream-settings preferences the user picked in the Stream Settings sheet,
    /// persisted in UserDefaults and re-sent unmodified as `STNG` on every connect —
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
    private var micSeq: UInt32 = 0
    @Published private(set) var isConnecting = false
    private var activeTransport: ConnectionTransport = .none
    /// Traffic-rate text lives on its own `TrafficMonitor` observable so the
    /// 1s tick never invalidates `ContentView`. The model only forwards byte
    /// counts into it.
    let trafficMonitor = TrafficMonitor()

    /// Fires if no `CAPS` message arrives within `capsTimeoutInterval` of the session
    /// coming up, meaning "assume this is an old daemon that predates capability
    /// negotiation." Cancelled if a real `CAPS` arrives first.
    private var capsTimeoutWorkItem: DispatchWorkItem?
    private static let capsTimeoutInterval: TimeInterval = 2.0

    @Published var physicalMouseConnected = false
    @Published var physicalKeyboardConnected = false
    private var mouseObservers: [Any] = []
    private var keyboardObservers: [Any] = []
    private var physicalMouseButtonMask: UInt8 = 0
    private var physicalMouseScrollAccumulator: Float = 0

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
    /// We always keep the category as `.playAndRecord` so iOS honors the low-latency IO buffer
    /// duration. The Bluetooth option changes based on mic state:
    /// - Mic OFF: `.allowBluetoothA2DP` routes Bluetooth output over the high-quality A2DP profile.
    /// - Mic ON: `.allowBluetooth` lets the system default input be a Bluetooth HFP mic.
    /// We always use the system default route and do not force a speaker override or add any picker UI.
    func configureAudioSession() {
        configureAudioSession(micActive: micCapture.isRunning)
    }

    private func configureAudioSession(micActive: Bool) {
        let session = AVAudioSession.sharedInstance()
        do {
            let options: AVAudioSession.CategoryOptions = micActive
                ? [.defaultToSpeaker, .mixWithOthers, .allowBluetooth]
                : [.defaultToSpeaker, .mixWithOthers, .allowBluetoothA2DP]
            try session.setCategory(.playAndRecord, mode: .default, options: options)
            try session.setPreferredSampleRate(48000)
            try session.setPreferredIOBufferDuration(0.01)
            try session.setActive(true)
            print("[app] audio session configured: mic=\(micActive), options=\(options), sampleRate=\(session.sampleRate), ioBufferDuration=\(session.ioBufferDuration)")
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
    /// picked. These persisted values are what feed the `STNG` validated and sent on every
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

    var sessionTransportTitle: String {
        let transportLabel = transport.isEmpty ? "connection" : transport.lowercased()
        switch connectionHealth {
        case .streaming:
            return "Streaming via \(transportLabel)"
        default:
            return "\(connectionHealthTitle) via \(transportLabel)"
        }
    }

    var codecLabel: String {
        switch decoder.codec {
        case .h264:
            return "H264"
        case .h265:
            return "H265"
        }
    }

    private func applyConnectionHealth(
        _ state: ConnectionHealthState,
        detail: String? = nil,
        transport: ConnectionTransport = .none
    ) {
        connectionHealth = state
        activeTransport = transport
        self.transport = transport.rawValue
        status = detail ?? defaultDetail(for: state)
        isConnected = state.isConnected
        isConnecting = state.isConnecting
        if transport == .none && !state.isConnected {
            trafficMonitor.reset()
            latencyText = "—"
            jitterText = "—"
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
    /// Starts the ~2s "assume legacy daemon" fallback timer and clears any capabilities
    /// left over from a previous session. If a real `CAPS` message arrives first,
    /// `handleDaemonCapabilities` cancels this timer.
    private func beginCapabilityNegotiation(transport: ConnectionTransport) {
        log("beginCapabilityNegotiation(transport: \(transport.rawValue))")
        capsTimeoutWorkItem?.cancel()
        daemonCapabilities = nil

        let workItem = DispatchWorkItem { [weak self] in
            guard let self else { return }
            self.log("CAPS timeout: assuming legacy daemon, all features available")
            self.daemonCapabilities = .assumeAllAvailable
            // Deliberately do NOT send STNG here — an old daemon that predates this
            // feature may not safely ignore an unrecognized control-message prefix.
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

        let settings = preferredStreamSettings
        let violations = capabilities.validate(settings)
        guard violations.isEmpty else {
            log("CAPS received: \(capabilities); settings \(settings) violate capabilities: \(violations)")
            failConnection(detail: "Failed to connect: " + violations.joined(separator: "; "))
            return
        }

        log("CAPS received: \(capabilities); sending STNG: \(settings)")
        sendStreamSettingsTransportState(settings)
    }

    /// Fails the in-progress connection because the daemon's advertised capabilities
    /// don't satisfy the user's configured stream settings. Tears down the active
    /// session (stream, network control, decoder/audio/camera/mic/peripheral state, and
    /// capability-negotiation state) then reports the failure through the same
    /// `applyConnectionHealth(.connectionError, ...)` surface every other connection
    /// failure uses, so the UI shows it identically. Safe to call regardless of whether a
    /// session is actually active: every teardown call here is nil-safe.
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
        networkControl?.sendStreamSettings(settings)
    }

    /// Clears capability-negotiation state on disconnect so a fresh connection starts
    /// from a clean slate (pre-CAPS "everything available" state) rather than carrying
    /// over the previous session's daemon capabilities.
    private func resetCapabilityNegotiation() {
        capsTimeoutWorkItem?.cancel()
        capsTimeoutWorkItem = nil
        daemonCapabilities = nil
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
        trafficMonitor.start()
        if !isConnected && !isConnecting {
            applyConnectionHealth(.idle, detail: disconnectedPrompt())
        }
    }

    private func updateSessionHostname(_ hostname: String) {
        guard !hostname.isEmpty else { return }
        sessionDisplayName = hostname
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
        trafficMonitor.reset()

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
            case .sessionEstablished(let key, let connection):
                self.log("PairingService result: session established")
                self.sessionKey = key
                self.pairingService = nil

                let control = NetworkControlClient(connection: connection, sessionKey: key)
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
                    self?.trafficMonitor.add(rxBytes: rxBytes, txBytes: txBytes)
                }
                control.onHostname = { [weak self, weak control] hostname in
                    Task { @MainActor in
                        guard let self, let control else { return }
                        guard self.networkControl === control else { return }
                        self.updateSessionHostname(hostname)
                    }
                }
                control.onCapabilities = { [weak self, weak control] capabilities in
                    Task { @MainActor in
                        guard let self, let control else { return }
                        guard self.networkControl === control else { return }
                        self.handleDaemonCapabilities(capabilities)
                    }
                }
                control.onLatency = { [weak self, weak control] rttMs, jitterMs in
                    Task { @MainActor in
                        guard let self, let control else { return }
                        guard self.networkControl === control else { return }
                        self.latencyText = Self.formatLatency(rttMs)
                        self.jitterText = Self.formatLatency(jitterMs)
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

        decoder.hasReportedFirstFrame = false

        let client = StreamClient(endpoint: endpoint, decoder: decoder, audioPlayer: audioPlayer, avSync: avSync)
        client.sessionKey = sessionKey
        client.sendPliRequest = { [weak controlClient] in
            controlClient?.sendPli()
        }
        self.stream = client
        client.onTraffic = { [weak self] rxBytes, txBytes in
            self?.trafficMonitor.add(rxBytes: rxBytes, txBytes: txBytes)
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

    // MARK: - Peripheral Monitoring (GCMouse / GCKeyboard)

    func startPeripheralMonitoring() {
        stopPeripheralMonitoring()

        if let mouse = GCMouse.current {
            attachMouse(mouse)
        }
        if let kb = GCKeyboard.coalesced {
            attachKeyboard(kb)
        }

        let mc = NotificationCenter.default.addObserver(
            forName: .GCMouseDidConnect, object: nil, queue: .main
        ) { [weak self] note in
            guard let self, let mouse = note.object as? GCMouse else { return }
            Task { @MainActor in self.attachMouse(mouse) }
        }
        let md = NotificationCenter.default.addObserver(
            forName: .GCMouseDidDisconnect, object: nil, queue: .main
        ) { [weak self] note in
            guard let self else { return }
            Task { @MainActor in self.detachMouse() }
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
        mouseObservers = [mc, md]
        keyboardObservers = [kc, kd]
    }

    func stopPeripheralMonitoring() {
        for obs in mouseObservers { NotificationCenter.default.removeObserver(obs) }
        for obs in keyboardObservers { NotificationCenter.default.removeObserver(obs) }
        mouseObservers.removeAll()
        keyboardObservers.removeAll()
        detachMouse()
        detachKeyboard()
    }

    private func attachMouse(_ mouse: GCMouse) {
        physicalMouseConnected = true
        physicalMouseButtonMask = 0
        physicalMouseScrollAccumulator = 0
        sendPeripheral(Data([0x01, 0x01])) // PERIPH_MOUSE, ATTACHED

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

    private func detachMouse() {
        guard physicalMouseConnected else { return }
        physicalMouseButtonMask = 0
        physicalMouseScrollAccumulator = 0
        physicalMouseConnected = false
        sendPeripheral(Data([0x01, 0x00])) // PERIPH_MOUSE, DETACHED
        print("[periph] mouse detached")
    }

    private func updatePhysicalMouseButton(button: UInt8, pressed: Bool) {
        let mouseButtonCode: UInt8
        switch button {
        case 0x01: mouseButtonCode = 0x00
        case 0x02: mouseButtonCode = 0x01
        case 0x04: mouseButtonCode = 0x02
        default: return
        }

        if pressed {
            physicalMouseButtonMask |= button
        } else {
            physicalMouseButtonMask &= ~button
        }

        sendMouse(Data([0x02, mouseButtonCode, pressed ? 1 : 0]))
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

    // MARK: - Camera

    /// True while `daemonCapabilities` is nil (pre-CAPS state, treat everything as
    /// available) or once `CAPS` reports camera support.
    var isCameraAvailable: Bool { daemonCapabilities?.camera ?? true }

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
        let width = UInt16(CameraCapture.outputSize.width)
        let height = UInt16(CameraCapture.outputSize.height)
        let fps = UInt16(CameraCapture.targetFps)
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

    /// True while `daemonCapabilities` is nil (pre-CAPS state) or once `CAPS` reports
    /// microphone support.
    var isMicAvailable: Bool { daemonCapabilities?.microphone ?? true }

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

                // Build MIC packet: "MIC" + seq(4 BE) + opus_data
                var packet = Data("MIC".utf8)
                withUnsafeBytes(of: seq.bigEndian) { packet.append(contentsOf: $0) }
                packet.append(opusData)

                self.stream?.sendMicPacket(packet)
            }
            micCapture.start()
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

    // MARK: - Speakers

    /// True while `daemonCapabilities` is nil (pre-CAPS state) or once `CAPS` reports
    /// speaker/system-audio support.
    var isSpeakerAvailable: Bool { daemonCapabilities?.speaker ?? true }

    func toggleSpeaker() {
        guard isSpeakerAvailable else { return }
        let isEnabled = !audioPlayer.isOutputEnabled
        audioPlayer.setOutputEnabled(isEnabled)
        sendSpeakerTransportState(isEnabled: isEnabled)
        objectWillChange.send()
    }

    var isSpeakerActive: Bool { audioPlayer.isOutputEnabled }
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
    @State private var preKeyboardY: CGFloat? = nil
    @State private var viewSize: CGSize = .zero
    @State private var pillSize: CGSize = CGSize(width: 80, height: 44)
    @State private var toolbarMessage: String? = nil
    @State private var showStreamSettings = false

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

                if let layer = model.displayLayer {
                    VideoDisplayView(
                        layer: layer,
                        videoWidth: model.decoder.videoWidth,
                        videoHeight: model.decoder.videoHeight,
                        onTouch: { data in model.sendTouch(data) },
                        onScroll: { deltaY in model.forwardPhysicalMouseScroll(deltaY: deltaY) },
                        hidePointer: model.physicalMouseConnected,
                        lockPointer: model.physicalMouseConnected
                    )
                    .ignoresSafeArea()
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
                if model.isConnected && showOverlay {
                    VStack(alignment: .leading, spacing: 10) {
                        Text(model.sessionTransportTitle)
                            .font(.title3.weight(.bold))

                        VStack(alignment: .leading, spacing: 6) {
                            infoRow(label: "Hostname", value: model.sessionDisplayName.isEmpty ? "Unknown" : model.sessionDisplayName)
                            ToggleableTrafficRows(trafficMonitor: model.trafficMonitor)
                            infoRow(label: "Codec", value: model.codecLabel)
                            infoRow(label: "Latency", value: model.latencyText)
                            infoRow(label: "Jitter", value: model.jitterText)
                        }

                        if !model.status.isEmpty {
                            Text(model.status)
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }

                        Button("Disconnect") { model.disconnect() }
                            .buttonStyle(.bordered)
                            .font(.caption)
                    }
                    .padding(12)
                    .frame(maxWidth: 380, alignment: .leading)
                    .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 14))
                    .padding(.horizontal, 16)
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
                    .padding(.top, 8)
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

                if model.isConnected {
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
        }
        .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillShowNotification)) { notif in
            guard let frame = notif.userInfo?[UIResponder.keyboardFrameEndUserInfoKey] as? CGRect else { return }
            let kbH = frame.height
            keyboardHeight = kbH
            let clamped = clampedBarPosition(barPosition, in: viewSize, keyboardHeight: kbH)
            if clamped != barPosition {
                preKeyboardY = barPosition.y
                withAnimation(.easeOut(duration: 0.25)) {
                    barPosition = clamped
                }
            }
        }
        .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillHideNotification)) { _ in
            keyboardHeight = 0
            if let savedY = preKeyboardY {
                withAnimation(.easeOut(duration: 0.25)) {
                    barPosition = clampedBarPosition(
                        CGPoint(x: barPosition.x, y: savedY),
                        in: viewSize,
                        keyboardHeight: 0
                    )
                }
                preKeyboardY = nil
            } else {
                barPosition = clampedBarPosition(barPosition, in: viewSize, keyboardHeight: 0)
            }
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
            // The parent ContentView no longer re-renders every second (the
            // 1s traffic tick now lives on TrafficMonitor and only invalidates
            // the info overlay), but .equatable() + seeded @State keeps the
            // sheet stable against any residual parent invalidation.
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

    @ViewBuilder
    private func infoRow(label: String, value: String) -> some View {
        HStack(spacing: 8) {
            Text("\(label):")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
            Text(value)
                .font(.caption)
                .foregroundStyle(.primary)
            Spacer(minLength: 0)
        }
    }

    /// The traffic-rate rows are the only part of the info overlay that
    /// changes every second. Wrapping them in this small child that observes
    /// only `TrafficMonitor` keeps the 1s tick from invalidating ContentView
    /// and tearing down an open settings sheet or picker.
    private struct ToggleableTrafficRows: View {
        @ObservedObject var trafficMonitor: TrafficMonitor

        var body: some View {
            VStack(alignment: .leading, spacing: 6) {
                HStack(spacing: 8) {
                    Text("Receiving:")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
                    Text(trafficMonitor.rxText)
                        .font(.caption)
                        .foregroundStyle(.primary)
                }
                HStack(spacing: 8) {
                    Text("Sending:")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
                    Text(trafficMonitor.txText)
                        .font(.caption)
                        .foregroundStyle(.primary)
                }
            }
        }
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

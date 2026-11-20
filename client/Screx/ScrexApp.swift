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
    @Published var transport: String = ""
    @Published var showPinEntry = false
    @Published var pinInput: String = ""
    @Published var pairingStatus: String = ""
    @Published var recentConnections: [RecentConnection] = StreamViewModel.loadRecentConnections()
    @Published private(set) var sessionDisplayName: String = ""
    /// The host being connected to, or whose connection just failed. The connection screen shows
    /// progress and errors on its tile, and as a temporary tile when it isn't saved yet.
    @Published private(set) var connectionTarget: RecentConnection?
    /// The last failure was the stream settings exceeding the host's limits.
    @Published private(set) var failureIsSettings = false
    /// Limits each host advertised, keyed like `RecentConnection.id`.
    @Published private(set) var hostLimits: [String: HostLimits] = StreamViewModel.loadHostLimits()
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
        let wasVisible = isExternalControlPanelVisible
        isLaptopMode = enabled && isExternalOnlyActive
        // Leaving laptop mode with the floating trackpad off: don't leave a button held by a
        // drag that was in progress on the laptop trackpad.
        if wasVisible && !isExternalControlPanelVisible && !isTrackpadEnabled {
            let previousMask = physicalMouseButtonMask | trackpadButtonMask
            trackpadButtonMask = 0
            sendMouseButtonChanges(from: previousMask)
        }
        trackpadMotionRemainder = .zero
        trackpadScrollRemainder = 0
    }

    /// Stream settings per host tile, keyed by tile ID.
    @Published private(set) var hostSettings: [String: HostStreamSettings] = StreamViewModel.loadHostSettings()

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
            try session.setPreferredIOBufferDuration(0.005)
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

    var canPinMore: Bool { pinnedConnections.count < Self.maxPinnedConnections }

    func tileState(for id: String) -> HostTileState {
        guard id == connectionTarget?.id else { return .idle }
        if isConnecting { return .connecting(showPinEntry ? "Waiting for PIN…" : "Connecting…") }
        if connectionHealth.isTerminalFailure { return .failed(status) }
        return .idle
    }

    private static let hostLimitsKey = "screx_host_limits"

    private static func loadHostLimits() -> [String: HostLimits] {
        guard let data = UserDefaults.standard.data(forKey: hostLimitsKey) else { return [:] }
        return (try? JSONDecoder().decode([String: HostLimits].self, from: data)) ?? [:]
    }

    private func persistHostLimits() {
        if let data = try? JSONEncoder().encode(hostLimits) {
            UserDefaults.standard.set(data, forKey: Self.hostLimitsKey)
        }
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
            let connections = normalizeConnections(try JSONDecoder().decode([RecentConnection].self, from: data))
            // Entries saved before tiles had IDs were just given one; save it so it stays stable
            // (their pairing key and settings are stored under it).
            if let updated = try? JSONEncoder().encode(connections), updated != data {
                UserDefaults.standard.set(updated, forKey: recentConnectionsKey)
            }
            return connections
        } catch {
            print("[app] failed to load recent connections: \(error)")
            return []
        }
    }

    private func persistRecentConnections() {
        // A removed tile takes its pairing key, settings and limits with it. The tile being
        // connected to is kept while it isn't saved yet.
        var keep = Set(recentConnections.map(\.id))
        if let target = connectionTarget?.id { keep.insert(target) }
        if hostLimits.keys.contains(where: { !keep.contains($0) }) {
            hostLimits = hostLimits.filter { keep.contains($0.key) }
            persistHostLimits()
        }
        if hostSettings.keys.contains(where: { !keep.contains($0) }) {
            hostSettings = hostSettings.filter { keep.contains($0.key) }
            persistHostSettings()
        }
        KeychainHelper.deletePairingKeys(except: keep)
        do {
            let data = try JSONEncoder().encode(recentConnections)
            UserDefaults.standard.set(data, forKey: Self.recentConnectionsKey)
        } catch {
            log("failed to persist recent connections: \(error)")
        }
    }

    // MARK: - Per-host settings

    private static let hostSettingsKey = "screx_host_stream_settings"
    private static let legacyPreferenceKeys = ["screx_pref_resolution", "screx_pref_framerate", "screx_pref_codec",
                                               "screx_pref_bitrate", "screx_pref_bitrate_custom_mbps"]

    /// Before settings were per host there was one global set; it becomes the starting settings
    /// of every host saved at the time.
    private static func loadHostSettings() -> [String: HostStreamSettings] {
        let defaults = UserDefaults.standard
        if let data = defaults.data(forKey: hostSettingsKey),
           let stored = try? JSONDecoder().decode([String: HostStreamSettings].self, from: data) {
            return stored
        }
        var result: [String: HostStreamSettings] = [:]
        if legacyPreferenceKeys.contains(where: { defaults.object(forKey: $0) != nil }) {
            var legacy = HostStreamSettings.defaults
            let raw = { (key: String) in defaults.string(forKey: key) ?? "" }
            if let value = StreamResolutionPreset(rawValue: raw("screx_pref_resolution")) {
                legacy.resolution = value == .native ? (StreamResolutionPreset.deviceNative ?? .native) : value
            }
            if let value = StreamFrameratePreset(rawValue: raw("screx_pref_framerate")) { legacy.framerate = value }
            if let value = StreamCodecPreset(rawValue: raw("screx_pref_codec")) { legacy.codec = value }
            if let value = StreamBitratePreset(rawValue: raw("screx_pref_bitrate")) { legacy.bitrate = value }
            let custom = defaults.double(forKey: "screx_pref_bitrate_custom_mbps")
            if custom.isFinite && custom > 0 { legacy.customBitrateMbps = custom }
            for host in loadRecentConnections() { result[host.id] = legacy }
            legacyPreferenceKeys.forEach(defaults.removeObject(forKey:))
        }
        if let data = try? JSONEncoder().encode(result) { defaults.set(data, forKey: hostSettingsKey) }
        return result
    }

    private func persistHostSettings() {
        if let data = try? JSONEncoder().encode(hostSettings) {
            UserDefaults.standard.set(data, forKey: Self.hostSettingsKey)
        }
    }

    func settings(for tileId: String?) -> HostStreamSettings {
        tileId.flatMap { hostSettings[$0] } ?? .defaults
    }

    /// The reason `address` can't be used for the tile (nil: a new host), or nil if it can.
    func addressError(_ address: String, for tileId: String?) -> String? {
        switch Self.parseAddress(address) {
        case .failure(let error):
            return error.message
        case .success(let target):
            // A new host may match a saved tile; it connects through that tile.
            guard let tileId else { return nil }
            let taken = recentConnections.contains {
                $0.id != tileId && $0.host == target.host && $0.port == target.port
            }
            return taken ? "Another host already uses this address" : nil
        }
    }

    /// Saves a host's settings and, when valid, a new address. The tile keeps its ID, so its
    /// pairing survives the address change.
    func updateHost(_ tileId: String, settings: HostStreamSettings, address: String) {
        hostSettings[tileId] = settings
        persistHostSettings()
        guard addressError(address, for: tileId) == nil,
              case .success(let target) = Self.parseAddress(address) else { return }
        if let index = recentConnections.firstIndex(where: { $0.id == tileId }),
           recentConnections[index].host != target.host || recentConnections[index].port != target.port {
            // A tile named after its old address would keep showing it.
            if recentConnections[index].name == recentConnections[index].endpointLabel
                || recentConnections[index].name == recentConnections[index].host {
                recentConnections[index].name = ""
            }
            recentConnections[index].host = target.host
            recentConnections[index].port = target.port
            persistRecentConnections()
        }
        if connectionTarget?.id == tileId, !isConnecting, !isConnected {
            connectionTarget?.host = target.host
            connectionTarget?.port = target.port
        }
    }

    /// Saves the tile that just connected, with the daemon's hostname as its name.
    private func rememberTarget(name: String) {
        guard var tile = connectionTarget else { return }
        let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        if !trimmed.isEmpty { tile.name = trimmed }
        tile.lastConnectedAt = Date()
        if let saved = recentConnections.first(where: { $0.id == tile.id }) { tile.isPinned = saved.isPinned }
        connectionTarget = tile
        var updated = recentConnections.filter { $0.id != tile.id }
        updated.insert(tile, at: 0)
        recentConnections = Self.normalizeConnections(updated)
        persistRecentConnections()
    }

    private func makeEndpoint(host: String, port: UInt16) -> NWEndpoint {
        NWEndpoint.hostPort(
            host: NWEndpoint.Host(host),
            port: NWEndpoint.Port(integerLiteral: port)
        )
    }

    struct AddressError: Error { let message: String }

    static func parseAddress(_ rawInput: String) -> Result<(host: String, port: UInt16), AddressError> {
        let input = rawInput.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !input.isEmpty else { return .failure(AddressError(message: "Enter an address")) }
        func fail(_ message: String) -> Result<(host: String, port: UInt16), AddressError> {
            .failure(AddressError(message: message))
        }

        if input.hasPrefix("[") {
            guard let closeBracket = input.firstIndex(of: "]") else {
                return fail("Invalid address. Use host, host:port, or [ipv6]:port.")
            }

            let host = String(input[input.index(after: input.startIndex)..<closeBracket])
            let suffix = String(input[input.index(after: closeBracket)...])
            guard !host.isEmpty else {
                return fail("Host cannot be empty.")
            }
            if suffix.isEmpty {
                return .success((host, defaultDaemonPort))
            }
            guard suffix.hasPrefix(":"), let port = UInt16(suffix.dropFirst()), port > 0 else {
                return fail("Invalid port. Use a value from 1 to 65535.")
            }
            return .success((host, port))
        }

        let colonCount = input.reduce(into: 0) { count, char in
            if char == ":" { count += 1 }
        }

        if colonCount == 1, let colon = input.lastIndex(of: ":") {
            let host = String(input[..<colon])
            let portPart = String(input[input.index(after: colon)...])
            guard !host.isEmpty else {
                    return fail("Host cannot be empty.")
            }
            guard let port = UInt16(portPart), port > 0 else {
                    return fail("Invalid port. Use a value from 1 to 65535.")
            }
            return .success((host, port))
        }

        return .success((input, defaultDaemonPort))
    }

    private func endpointHostAndPort(_ endpoint: NWEndpoint, fallbackHost: String) -> (host: String, port: UInt16) {
        switch endpoint {
        case .hostPort(let host, let port):
            return ("\(host)", port.rawValue)
        default:
            return (fallbackHost, defaultDaemonPort)
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
        case .idle, .connecting, .pairing, .waitingForVideo, .streaming:
            return ""
        case .busy:
            return Self.busyMessage
        case .connectionRefused:
            return Self.refusedMessage
        case .timedOut:
            return Self.timedOutMessage
        case .sessionStale:
            return Self.staleMessage
        case .connectionError:
            return "Couldn’t connect. Try again."
        }
    }

    private static let busyMessage = "In use by another device"
    private static let refusedMessage = "Screx isn’t running on this computer"
    private static let timedOutMessage = "Not responding. Check that it’s on and on the same network."
    private static let staleMessage = "Session expired. Try again."

    private func applyConnectionFailure(_ message: String, transport: ConnectionTransport = .none) {
        let normalized = message.lowercased()
        if normalized.contains("busy") {
            applyConnectionHealth(.busy, detail: Self.busyMessage, transport: transport)
        } else if normalized.contains("refused") {
            applyConnectionHealth(.connectionRefused, detail: Self.refusedMessage, transport: transport)
        } else if normalized.contains("timed out") || normalized.contains("timeout") {
            applyConnectionHealth(.timedOut, detail: Self.timedOutMessage, transport: transport)
        } else if normalized.contains("pair again") || normalized.contains("stale") {
            applyConnectionHealth(.sessionStale, detail: Self.staleMessage, transport: transport)
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
            self.failConnection(detail: "Screx on this computer didn’t respond. Try again.")
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
        if let id = connectionTarget?.id {
            hostLimits[id] = HostLimits(capabilities)
            persistHostLimits()
        }
        updateSessionHostname(capabilities.hostname)
        currentHostKey = capabilities.hostname.isEmpty
            ? (lastNetName ?? "")
            : capabilities.hostname

        // Internal resolution/FPS/codec/bitrate come from the host's settings. The display mode and
        // the external output's resolution/FPS come from this host's remembered choice, falling
        // back to internal-only and the display's recommended resolution.
        var settings = settings(for: connectionTarget?.id).streamSettings
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
            failConnection(detail: violations.joined(separator: ". "))
            failureIsSettings = true
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
        stream?.onFirstFrame = nil
        stream?.disconnect()
        stream = nil
        closeNetworkControl(gracefully: false)
        pairingService?.cancel()
        pairingService = nil
        pendingPinCompletion = nil
        showPinEntry = false
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
        disconnect(detail: "Disconnected while Screx was in the background")
    }

    func startServices() {
        guard !servicesStarted else { return }
        servicesStarted = true
        log("startServices()")
        log("local display: \(DeviceDisplayIdentity.current.name), EDID: \(DeviceDisplayIdentity.current.edidName)")
        startTrafficMonitoring()
        syncCameraCompressionQuality()
        if !isConnected && !isConnecting {
            applyConnectionHealth(.idle)
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

    /// Connects to an address from New Host with the settings chosen there. An address that
    /// matches a saved tile connects through that tile (and its pairing), taking the new
    /// settings; anything else starts a new tile, saved after its first successful connection.
    /// Returns the reason it can't connect, or nil.
    func connectNewHost(address: String, settings: HostStreamSettings) -> String? {
        guard !isConnecting else { return "Wait for the current connection to finish" }
        let target: (host: String, port: UInt16)
        switch Self.parseAddress(address) {
        case .failure(let error):
            return error.message
        case .success(let parsed):
            target = parsed
        }
        let matches = { (tile: RecentConnection) in tile.host == target.host && tile.port == target.port }
        let tile = recentConnections.first(where: matches)
            ?? connectionTarget.flatMap { matches($0) ? $0 : nil }
            ?? RecentConnection(host: target.host, port: target.port, name: "", lastConnectedAt: Date())
        hostSettings[tile.id] = settings
        persistHostSettings()
        connect(to: tile)
        return nil
    }

    func connectRecent(_ tile: RecentConnection) {
        guard !isConnecting else {
            log("connectRecent() ignored while already connecting")
            return
        }
        connect(to: tile)
    }

    private func connect(to tile: RecentConnection) {
        let endpoint = makeEndpoint(host: tile.host, port: tile.port)
        lastNetEndpoint = endpoint
        lastNetName = tile.displayName
        connectionTarget = tile
        failureIsSettings = false
        connectToEndpoint(endpoint, name: tile.displayName, tile: tile)
    }

    private func removeHosts(where shouldRemove: (RecentConnection) -> Bool) {
        recentConnections.removeAll(where: shouldRemove)
        if let target = connectionTarget, !isConnecting, !isConnected,
           !recentConnections.contains(where: { $0.id == target.id }) {
            connectionTarget = nil
        }
        persistRecentConnections()
    }

    func clearUnpinnedHosts() { removeHosts { !$0.isPinned } }

    func clearAllHosts() { removeHosts { _ in true } }

    /// Removing a tile also forgets its pairing, so adding it back asks for the PIN.
    func deleteConnection(_ connection: RecentConnection) {
        removeHosts { $0.id == connection.id }
    }

    func togglePinned(_ connection: RecentConnection) {
        if !connection.isPinned && !canPinMore { return }
        guard let index = recentConnections.firstIndex(where: { $0.id == connection.id }) else { return }
        recentConnections[index].isPinned.toggle()
        recentConnections = Self.normalizeConnections(recentConnections)
        persistRecentConnections()
    }

    func connectToEndpoint(_ endpoint: NWEndpoint, name: String, tile: RecentConnection) {
        log("connectToEndpoint(name=\(name), endpoint=\(endpoint)) start; isConnected=\(isConnected) isConnecting=\(isConnecting)")
        // Detach old stream's callbacks so stale async events can't interfere
        stream?.onFirstFrame = nil
        stream?.disconnect()
        closeNetworkControl(gracefully: true)
        pairingService?.cancel()

        applyConnectionHealth(.connecting, transport: .network)
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

        // Authenticate one QUIC connection before exposing control, input or media.
        let ps = PairingService()
        self.pairingService = ps
        log("starting PairingService for host=\(host) port=\(port)")

        ps.onResult = { [weak self, weak ps] result in
            guard let self, let ps, self.pairingService === ps else { return }
            switch result {
            case .sessionEstablished(let session):
                self.log("PairingService result: session established")
                self.pairingService = nil

                let control = NetworkControlClient(session: session)
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
                        self.stream?.updateNetworkStats(rttMs: rttMs, jitterMs: jitterMs)
                        let constrained = rttMs > 60 || jitterMs > 25
                        self.cameraCapture.compressionQuality = constrained ? 0.25 : networkCameraCompressionQuality
                        self.cameraCapture.frameSkipFactor = constrained ? 2 : 1
                    }
                }
                self.beginCapabilityNegotiation(transport: .network)
                control.start()
                self.decoder.setSuspended(false)
                self.syncSpeakerPassthroughState()

                self.startEncryptedStream(endpoint: endpoint, name: name, session: session, controlClient: control)

            case .pinRequired(let completion):
                self.log("PairingService result: PIN required")
                self.pendingPinCompletion = completion
                self.pinInput = ""
                self.showPinEntry = true
                self.applyConnectionHealth(.pairing, detail: "Enter the PIN shown on the daemon.", transport: .network)
                self.pairingStatus = "Enter the PIN shown on the daemon"

            case .rejected(let reason):
                self.log("PairingService result: rejected (\(reason))")
                self.pendingPinCompletion = nil
                self.showPinEntry = false
                self.pairingService = nil
                self.applyConnectionFailure("Pairing rejected: \(reason)", transport: .none)

            case .error(let msg):
                self.log("PairingService result: error (\(msg))")
                self.pendingPinCompletion = nil
                self.showPinEntry = false
                self.pairingService = nil
                self.applyConnectionFailure("Pairing error: \(msg)", transport: .none)
            }
        }

        ps.pair(host: host, port: port, tileId: tile.tileId, deviceId: tile.deviceId, clientName: UIDevice.current.name)
    }

    func submitPin() {
        guard pairingService != nil, let completion = pendingPinCompletion else { return }
        let pin = normalizedPairingPin
        guard pin.utf8.count == 8, pin.utf8.allSatisfy({ (48...57).contains($0) }) else {
            pairingStatus = "PIN must be exactly 8 digits"
            return
        }
        log("submitPin()")
        showPinEntry = false
        applyConnectionHealth(.pairing, detail: "Verifying the pairing PIN.", transport: .network)
        completion(pin)
        pendingPinCompletion = nil
    }

    /// Accept both the displayed XXXX-XXXX form and eight ungrouped digits.
    var normalizedPairingPin: String {
        pinInput.trimmingCharacters(in: .whitespacesAndNewlines)
            .replacingOccurrences(of: "-", with: "")
    }

    func cancelPin() {
        log("cancelPin()")
        showPinEntry = false
        pendingPinCompletion = nil
        pairingService?.cancel()
        pairingService = nil
        connectionTarget = nil
        applyConnectionHealth(.idle, transport: .none)
    }

    private func startEncryptedStream(endpoint: NWEndpoint, name: String, session: QUICSession, controlClient: NetworkControlClient) {
        log("startEncryptedStream(name=\(name), endpoint=\(endpoint))")
        applyConnectionHealth(.waitingForVideo, detail: "Waiting for video from \(name).", transport: .network)


        decoder.onVideoSizeChanged = { [weak self] width, height in
            Task { @MainActor in self?.internalVideoSize = CGSize(width: width, height: height) }
        }
        let client = StreamClient(session: session, decoder: decoder, externalDecoder: externalDecoder, audioPlayer: audioPlayer, avSync: avSync)
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

        client.onFirstFrame = { [weak self, weak client] in
            Task { @MainActor in
                guard let self, let client, self.stream === client else { return }
                self.log("StreamClient first frame")
                self.applyConnectionHealth(.streaming, transport: .network)
                self.rememberTarget(name: self.sessionDisplayName)
                self.audioPlayer.start()
                self.startPeripheralMonitoring()
            }
        }
        client.connect()
    }

    /// Called when we've lost all streams and should return to idle state.
    private func handleStreamLost() {
        log("handleStreamLost() lastNetEndpoint=\(String(describing: lastNetEndpoint)) lastNetName=\(String(describing: lastNetName))")
        let preservedHealth = connectionHealth
        let preservedStatus = status
        stream?.onFirstFrame = nil
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
            applyConnectionHealth(.timedOut, detail: "Connection lost", transport: .none)
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
        stream?.onFirstFrame = nil
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
        // A reason means the app ended the session, so show it on the host's tile.
        if let detail {
            applyConnectionHealth(.connectionError, detail: detail, transport: .none)
        } else {
            connectionTarget = nil
            applyConnectionHealth(.idle, transport: .none)
        }
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

    /// On-screen trackpad input is accepted from the floating trackpad when it's enabled, and
    /// always from laptop mode's built-in trackpad, independent of the floating toggle.
    private var acceptsTrackpadInput: Bool {
        isConnected && (isTrackpadEnabled || isExternalControlPanelVisible)
    }

    func moveTrackpad(dx: CGFloat, dy: CGFloat) {
        guard acceptsTrackpadInput else { return }
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
        guard acceptsTrackpadInput, code < 3 else { return }
        let previousMask = physicalMouseButtonMask | trackpadButtonMask
        let bit: UInt8 = 1 << code
        if pressed { trackpadButtonMask |= bit } else { trackpadButtonMask &= ~bit }
        sendMouseButtonChanges(from: previousMask)
    }

    func scrollTrackpad(deltaY: CGFloat) {
        guard acceptsTrackpadInput else { return }
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
            cameraCapture.onJPEG = { [weak self, weak stream = self.stream] jpeg in
                DispatchQueue.main.async {
                    guard let self, let stream, self.stream === stream, self.cameraCapture.isRunning else { return }
                    let fid = self.camFrameId
                    self.camFrameId = self.camFrameId &+ 1
                    stream.sendCameraFrame(jpeg, frameId: fid)
                }
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
            micCapture.onOpusPacket = { [weak self, weak stream = self.stream] opusData in
                DispatchQueue.main.async {
                    guard let self, let stream, self.stream === stream, self.micCapture.isRunning else { return }
                    let seq = self.micSeq
                    self.micSeq = self.micSeq &+ 1
                    stream.sendMicPacket(opusData, seq: seq)
                }
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


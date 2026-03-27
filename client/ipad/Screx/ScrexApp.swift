import SwiftUI
import Combine
import AVFoundation
import CryptoKit
import Network
import GameController
import UIKit

@main
final class AppDelegate: UIResponder, UIApplicationDelegate {
    let model = StreamViewModel()

    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil
    ) -> Bool {
        let session = AVAudioSession.sharedInstance()
        do {
            try session.setCategory(
                .playAndRecord,
                mode: .default,
                options: [.defaultToSpeaker, .mixWithOthers, .allowBluetooth]
            )
            try session.setPreferredSampleRate(48000)
            try session.setPreferredIOBufferDuration(0.01)
            try session.setActive(true)
            print("[app] audio session: rate=\(session.sampleRate)Hz, ioBufferDuration=\(session.ioBufferDuration)")
        } catch {
            print("[app] audio session setup failed: \(error)")
        }

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

private let networkCameraCompressionQuality: CGFloat = 0.4
private let usbCameraCompressionQuality: CGFloat = 0.7

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
    case usb = "USB"
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
    @Published private(set) var receiveRateText: String = "0 B/s"
    @Published private(set) var sendRateText: String = "0 B/s"

    let decoder = VideoDecoder()
    let avSync = AVSyncState()
    let audioPlayer: AudioPlayer
    let cameraCapture = CameraCapture()
    let micCapture = MicCapture()

    private var stream: StreamClient?
    private var networkControl: NetworkControlClient?
    private var usbListener: USBListener?
    private var pairingService: PairingService?
    private var pendingPinCompletion: ((String) -> Void)?
    private var sessionKey: SymmetricKey?

    nonisolated init() {
        self.audioPlayer = AudioPlayer(avSync: avSync)
    }
    private var servicesStarted = false
    private var usbConnected = false
    @Published var isCharging = false
    @Published var isUSBListening = false
    private var camFrameId: UInt32 = 0

    private var lastNetEndpoint: NWEndpoint?
    private var lastNetName: String?
    private var micSeq: UInt32 = 0
    @Published private(set) var isConnecting = false
    private var activeTransport: ConnectionTransport = .none
    private var trafficTimer: DispatchSourceTimer?
    private var totalRxBytes: UInt64 = 0
    private var totalTxBytes: UInt64 = 0
    private var previousRxBytes: UInt64 = 0
    private var previousTxBytes: UInt64 = 0

    @Published var physicalMouseConnected = false
    @Published var physicalKeyboardConnected = false
    @Published var physicalControllerConnectedCount = 0
    @Published private(set) var isControllerPassthroughEnabled = true
    private var mouseObservers: [Any] = []
    private var keyboardObservers: [Any] = []
    private var controllerObservers: [Any] = []
    private var controllerSlots: [ObjectIdentifier: UInt8] = [:]
    private var physicalMouseButtonMask: UInt8 = 0
    private var physicalMouseScrollAccumulator: Float = 0

    private static let recentConnectionsKey = "screx_recent_connections"
    private static let maxRecentConnections = 5
    private static let maxPinnedConnections = 10

    private func log(_ message: String) {
        print("[app] \(message)")
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
        syncCameraCompressionQuality()
        status = detail ?? defaultDetail(for: state)
        isConnected = state.isConnected
        isConnecting = state.isConnecting
        if transport == .none && !state.isConnected {
            receiveRateText = "0 bytes/s"
            sendRateText = "0 bytes/s"
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
        if activeTransport == .usb {
            usbListener?.sendSpeakerState(isEnabled: isEnabled)
        } else {
            networkControl?.sendSpeakerState(isEnabled: isEnabled)
        }
    }

    private func syncSpeakerPassthroughState() {
        sendSpeakerTransportState(isEnabled: audioPlayer.isOutputEnabled)
    }

    func handleAppDidEnterBackground() {
        log("sceneDidEnterBackground")
        guard activeTransport != .none || isConnecting || isUSBListening || pairingService != nil else { return }
        disconnect(detail: "Screx disconnected when the app entered the background. Reconnect to continue.")
    }

    func startServices() {
        guard !servicesStarted else { return }
        servicesStarted = true
        log("startServices()")
        startBatteryMonitoring()
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
            let rxDelta = self.totalRxBytes &- self.previousRxBytes
            let txDelta = self.totalTxBytes &- self.previousTxBytes
            self.previousRxBytes = self.totalRxBytes
            self.previousTxBytes = self.totalTxBytes
            self.receiveRateText = formatByteRate(Double(rxDelta))
            self.sendRateText = formatByteRate(Double(txDelta))
        }
        timer.resume()
        trafficTimer = timer
    }

    private func recordTraffic(rxBytes: Int = 0, txBytes: Int = 0) {
        if rxBytes > 0 {
            totalRxBytes = totalRxBytes &+ UInt64(rxBytes)
        }
        if txBytes > 0 {
            totalTxBytes = totalTxBytes &+ UInt64(txBytes)
        }
    }

    private func updateSessionHostname(_ hostname: String) {
        guard !hostname.isEmpty else { return }
        sessionDisplayName = hostname
    }

    private func syncCameraCompressionQuality() {
        let isUSBTransport = activeTransport == .usb
        cameraCapture.compressionQuality = isUSBTransport
            ? usbCameraCompressionQuality
            : networkCameraCompressionQuality
        cameraCapture.captureProfile = isUSBTransport ? .usb : .network
    }

    private func startBatteryMonitoring() {
        UIDevice.current.isBatteryMonitoringEnabled = true
        updateChargingState()
        NotificationCenter.default.addObserver(
            forName: UIDevice.batteryStateDidChangeNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            self?.updateChargingState()
        }
    }

    private func updateChargingState() {
        let state = UIDevice.current.batteryState
        isCharging = state == .charging || state == .full
    }

    func connectUSB() {
        if isUSBListening {
            cancelUSBListening()
            return
        }
        guard !isConnecting && !isConnected else { return }

        isUSBListening = true
        applyConnectionHealth(.connecting, detail: "Listening for USB connection.", transport: .usb)

        let usb = USBListener(decoder: decoder, audioPlayer: audioPlayer, avSync: avSync)
        self.usbListener = usb
        self.sessionDisplayName = "USB device"
        self.totalRxBytes = 0
        self.totalTxBytes = 0
        self.previousRxBytes = 0
        self.previousTxBytes = 0

        usb.onEvent = { [weak self] event in
            Task { @MainActor in
                guard let self else { return }
                self.log("usb event: \(String(describing: event))")
                switch event {
                case .listenerReady:
                    if self.isUSBListening && !self.usbConnected {
                        self.applyConnectionHealth(.connecting, detail: "USB listener is ready. Waiting for the daemon.", transport: .usb)
                    }
                case .listenerFailed(let message):
                    self.isUSBListening = false
                    self.applyConnectionFailure("USB listener failed: \(message)", transport: .none)
                case .connected:
                    self.applyConnectionHealth(.waitingForVideo, detail: "USB connected. Waiting for video.", transport: .usb)
                case .firstFrame:
                    self.applyConnectionHealth(.streaming, transport: .usb)
                    if let endpoint = self.lastNetEndpoint {
                        let target = self.endpointHostAndPort(endpoint, fallbackHost: self.lastNetName ?? "")
                        self.manualHost = formatEndpointInput(host: target.host, port: target.port)
                    }
                }
            }
        }
        usb.onTraffic = { [weak self] rxBytes, txBytes in
            Task { @MainActor in
                self?.recordTraffic(rxBytes: rxBytes, txBytes: txBytes)
            }
        }
        usb.onHostname = { [weak self] hostname in
            Task { @MainActor in
                self?.updateSessionHostname(hostname)
            }
        }
        usb.onConnected = { [weak self] in
            Task { @MainActor in
                guard let self else { return }
                self.log("usb connected")
                self.usbConnected = true
                self.isUSBListening = false
                self.stream?.suppressTimeout = true
                self.decoder.setSuspended(false)
                self.syncSpeakerPassthroughState()
                self.audioPlayer.start()
                self.startPeripheralMonitoring()
            }
        }
        usb.onDisconnected = { [weak self] in
            Task { @MainActor in
                guard let self else { return }
                self.log("usb disconnected")
                self.usbConnected = false
                self.isUSBListening = false
                self.usbListener?.stop()
                self.usbListener = nil
                self.stream?.suppressTimeout = false
                self.stream?.onEvent = nil
                self.stream?.onDisconnect = nil
                self.stream?.disconnect()
                self.stream = nil
                self.closeNetworkControl(gracefully: false)
                self.decoder.setSuspended(false)
                self.audioPlayer.stop()
                self.micCapture.stop()
                self.cameraCapture.stop()
                self.stopPeripheralMonitoring()
                self.applyConnectionHealth(.connectionError, detail: "USB disconnected. \(self.disconnectedPrompt())", transport: .none)
            }
        }
        usb.start()
    }

    private func cancelUSBListening() {
        usbListener?.stop()
        usbListener = nil
        isUSBListening = false
        usbConnected = false
        applyConnectionHealth(.idle, detail: disconnectedPrompt())
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
        if !isConnected && !isConnecting && !usbConnected {
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
        if !isConnected && !isConnecting && !usbConnected {
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

        if !isConnected && !isConnecting && !usbConnected {
            applyConnectionHealth(.idle, detail: disconnectedPrompt())
        }
    }

    func connectToEndpoint(_ endpoint: NWEndpoint, name: String) {
        log("connectToEndpoint(name=\(name), endpoint=\(endpoint)) start; isConnected=\(isConnected) isConnecting=\(isConnecting) usbConnected=\(usbConnected)")
        // Detach old stream's callbacks so stale async events can't interfere
        stream?.onEvent = nil
        stream?.onDisconnect = nil
        stream?.disconnect()
        closeNetworkControl(gracefully: true)
        pairingService?.cancel()

        applyConnectionHealth(.connecting, detail: "Connecting to \(name).", transport: .network)
        sessionDisplayName = name
        totalRxBytes = 0
        totalTxBytes = 0
        previousRxBytes = 0
        previousTxBytes = 0

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
                        if !self.usbConnected {
                            self.handleStreamLost()
                        }
                    }
                }
                self.networkControl = control
                control.onTraffic = { [weak self, weak control] rxBytes, txBytes in
                    Task { @MainActor in
                        guard let self, let control else { return }
                        guard self.networkControl === control else { return }
                        self.recordTraffic(rxBytes: rxBytes, txBytes: txBytes)
                    }
                }
                control.onHostname = { [weak self, weak control] hostname in
                    Task { @MainActor in
                        guard let self, let control else { return }
                        guard self.networkControl === control else { return }
                        self.updateSessionHostname(hostname)
                    }
                }
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
        if !usbConnected {
            applyConnectionHealth(.waitingForVideo, detail: "Waiting for video from \(name).", transport: .network)
        }

        decoder.hasReportedFirstFrame = false

        let client = StreamClient(endpoint: endpoint, decoder: decoder, audioPlayer: audioPlayer, avSync: avSync)
        client.sessionKey = sessionKey
        client.sendPliRequest = { [weak controlClient] in
            controlClient?.sendPli()
        }
        self.stream = client
        client.onTraffic = { [weak self, weak client] rxBytes, txBytes in
            Task { @MainActor in
                guard let self, let client else { return }
                guard self.stream === client else { return }
                self.recordTraffic(rxBytes: rxBytes, txBytes: txBytes)
            }
        }

        client.onEvent = { [weak self, weak client] event in
            Task { @MainActor in
                guard let self, let client else { return }
                guard self.stream === client else { return }
                self.log("StreamClient event: \(String(describing: event))")
                if !self.usbConnected {
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
        }
        client.onDisconnect = { [weak self, weak client] in
            Task { @MainActor in
                guard let self, let client else { return }
                guard self.stream === client else { return }
                self.log("StreamClient onDisconnect")
                self.stream = nil
                if !self.usbConnected {
                    self.handleStreamLost()
                }
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
        cameraCapture.stop()
        stopPeripheralMonitoring()
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
        usbListener?.stop()
        usbListener = nil
        usbConnected = false
        isUSBListening = false
        decoder.setSuspended(false)
        audioPlayer.stop()
        micCapture.stop()
        cameraCapture.stop()
        stopPeripheralMonitoring()
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
        if usbConnected, let usb = usbListener {
            usb.sendTouch(data)
        } else if let control = networkControl {
            control.sendTouch(data)
        }
    }

    func sendKey(_ keyData: Data) {
        if usbConnected, let usb = usbListener {
            usb.sendKey(keyData)
        } else if let control = networkControl {
            control.sendKey(keyData)
        }
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
        if usbConnected, let usb = usbListener {
            usb.sendMouse(mouseData)
        } else if let control = networkControl {
            control.sendMouse(mouseData)
        }
    }

    func sendRawKey(_ keyData: Data) {
        if usbConnected, let usb = usbListener {
            usb.sendRawKey(keyData)
        } else if let control = networkControl {
            control.sendRawKey(keyData)
        }
    }

    func sendPeripheral(_ periphData: Data) {
        if usbConnected, let usb = usbListener {
            usb.sendPeripheral(periphData)
        } else if let control = networkControl {
            control.sendPeripheral(periphData)
        }
    }

    func sendGamepad(_ gamepadData: Data) {
        if usbConnected, let usb = usbListener {
            usb.sendGamepad(gamepadData)
        } else if let control = networkControl {
            control.sendGamepad(gamepadData)
        }
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
        for obs in mouseObservers { NotificationCenter.default.removeObserver(obs) }
        for obs in keyboardObservers { NotificationCenter.default.removeObserver(obs) }
        for obs in controllerObservers { NotificationCenter.default.removeObserver(obs) }
        mouseObservers.removeAll()
        keyboardObservers.removeAll()
        controllerObservers.removeAll()
        detachMouse()
        detachKeyboard()
        detachAllControllers()
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

    func toggleCamera() {
        if cameraCapture.isRunning {
            cameraCapture.stop()
            sendCameraDisable()
        } else {
            sendCameraEnable()
            cameraCapture.onJPEG = { [weak self] jpeg in
                guard let self else { return }
                let fid = self.camFrameId
                self.camFrameId = self.camFrameId &+ 1
                if self.usbConnected, let usb = self.usbListener {
                    usb.sendCameraFrame(jpeg, frameId: fid)
                } else if let stream = self.stream {
                    stream.sendCameraFrame(jpeg, frameId: fid)
                }
            }
            cameraCapture.start()
        }
        objectWillChange.send()
    }

    /// Tells the daemon to create the virtual webcam with our capture profile.
    private func sendCameraEnable() {
        let profile: CameraCapture.CaptureProfile = activeTransport == .usb ? .usb : .network
        let width = UInt16(profile.outputSize.width)
        let height = UInt16(profile.outputSize.height)
        let fps = UInt16(profile.targetFps)
        if activeTransport == .usb {
            usbListener?.sendCameraConfig(width: width, height: height, fps: fps)
        } else {
            networkControl?.sendCameraConfig(width: width, height: height, fps: fps)
        }
    }

    /// Tells the daemon to destroy the virtual webcam.
    private func sendCameraDisable() {
        if activeTransport == .usb {
            usbListener?.sendCameraConfig(width: 0, height: 0, fps: 0)
        } else {
            networkControl?.sendCameraConfig(width: 0, height: 0, fps: 0)
        }
    }

    var isCameraActive: Bool { cameraCapture.isRunning }
    var isCameraFront: Bool { cameraCapture.usingFront }

    func flipCamera() {
        cameraCapture.flipCamera()
        objectWillChange.send()
    }

    // MARK: - Microphone

    func toggleMic() {
        if micCapture.isRunning {
            micCapture.stop()
            sendMicDisable()
        } else {
            sendMicEnable()
            micCapture.onOpusPacket = { [weak self] opusData in
                guard let self else { return }
                let seq = self.micSeq
                self.micSeq = self.micSeq &+ 1

                // Build MIC packet: "MIC" + seq(4 BE) + opus_data
                var packet = Data("MIC".utf8)
                withUnsafeBytes(of: seq.bigEndian) { packet.append(contentsOf: $0) }
                packet.append(opusData)

                if self.usbConnected, let usb = self.usbListener {
                    usb.sendMicPacket(packet)
                } else if let stream = self.stream {
                    stream.sendMicPacket(packet)
                }
            }
            micCapture.start()
        }
        objectWillChange.send()
    }

    var isMicActive: Bool { micCapture.isRunning }

    /// Tells the daemon to create the virtual microphone device.
    private func sendMicEnable() {
        if activeTransport == .usb {
            usbListener?.sendMicState(isEnabled: true)
        } else {
            networkControl?.sendMicState(isEnabled: true)
        }
    }

    /// Tells the daemon to destroy the virtual microphone device.
    private func sendMicDisable() {
        if activeTransport == .usb {
            usbListener?.sendMicState(isEnabled: false)
        } else {
            networkControl?.sendMicState(isEnabled: false)
        }
    }

    // MARK: - Speakers / Controllers

    func toggleSpeaker() {
        let isEnabled = !audioPlayer.isOutputEnabled
        audioPlayer.setOutputEnabled(isEnabled)
        sendSpeakerTransportState(isEnabled: isEnabled)
        objectWillChange.send()
    }

    var isSpeakerActive: Bool { audioPlayer.isOutputEnabled }

    func toggleControllerPassthrough() {
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
    @State private var preKeyboardY: CGFloat? = nil
    @State private var viewSize: CGSize = .zero
    @State private var pillSize: CGSize = CGSize(width: 80, height: 44)
    @State private var toolbarMessage: String? = nil

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
                                        : model.isConnecting && !model.isUSBListening
                                            ? "Connecting…"
                                            : "Connect"
                            ) { model.connectManual() }
                                .buttonStyle(.borderedProminent)
                                .disabled(model.isConnecting || model.manualHost.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                        }

                        HStack(spacing: 6) {
                            Rectangle()
                                .frame(height: 1)
                                .foregroundStyle(Color(.separator))
                            Text("or")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                            Rectangle()
                                .frame(height: 1)
                                .foregroundStyle(Color(.separator))
                        }
                        .padding(.vertical, 2)

                        Button {
                            model.connectUSB()
                        } label: {
                            HStack(spacing: 6) {
                                Image(systemName: "cable.connector")
                                Text(
                                    model.isUSBListening
                                        ? "Cancel USB"
                                        : model.connectionHealth == .waitingForVideo && model.transport == ConnectionTransport.usb.rawValue
                                            ? "Waiting for Video…"
                                            : "Connect via USB"
                                )
                            }
                            .frame(maxWidth: .infinity)
                        }
                        .buttonStyle(.bordered)
                        .tint(model.isUSBListening ? .red : .green)
                        .disabled(!model.isUSBListening && (!model.isCharging || model.isConnecting))

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
                            infoRow(label: "Receiving", value: model.receiveRateText)
                            infoRow(label: "Sending", value: model.sendRateText)
                            infoRow(label: "Codec", value: model.codecLabel)
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
                        icon: model.isControllerPassthroughEnabled ? "gamecontroller.fill" : "gamecontroller",
                        active: model.isControllerPassthroughEnabled,
                        color: model.isControllerPassthroughEnabled ? .green : .white
                    ) { model.toggleControllerPassthrough() }

                    toolbarButton(
                        icon: model.isCameraActive
                            ? (model.isCameraFront ? "arrow.triangle.2.circlepath.camera.fill" : "video.fill")
                            : "video",
                        active: model.isCameraActive,
                        color: model.isCameraActive ? .green : .white
                    ) { model.toggleCamera() }
                        .onLongPressGesture(minimumDuration: 0.5) { model.flipCamera() }

                    toolbarButton(
                        icon: model.isMicActive ? "mic.fill" : "mic",
                        active: model.isMicActive,
                        color: model.isMicActive ? .green : .white
                    ) { model.toggleMic() }

                    toolbarButton(
                        icon: model.isSpeakerActive ? "speaker.wave.2.fill" : "speaker.slash.fill",
                        active: model.isSpeakerActive,
                        color: model.isSpeakerActive ? .green : .white
                    ) { model.toggleSpeaker() }
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

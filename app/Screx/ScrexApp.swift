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
}

final class MouseCaptureRootView: UIView {
    override var canBecomeFirstResponder: Bool {
        true
    }
}

private let defaultDaemonPort: UInt16 = 9000

private func formatEndpointInput(host: String, port: UInt16) -> String {
    let formattedHost = host.contains(":") ? "[\(host)]" : host
    return port == defaultDaemonPort ? formattedHost : "\(formattedHost):\(port)"
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
    @Published var isConnected = false
    @Published var manualHost: String = ""
    @Published var transport: String = ""
    @Published var showPinEntry = false
    @Published var pinInput: String = ""
    @Published var pairingStatus: String = ""
    @Published var recentConnections: [RecentConnection] = StreamViewModel.loadRecentConnections()

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
    private var camFrameId: UInt32 = 0

    private var lastNetEndpoint: NWEndpoint?
    private var lastNetName: String?
    private var micSeq: UInt32 = 0
    @Published private(set) var isConnecting = false

    @Published var physicalMouseConnected = false
    @Published var physicalKeyboardConnected = false
    @Published var physicalControllerConnectedCount = 0
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
                status = "Invalid address. Use host, host:port, or [ipv6]:port."
                return nil
            }

            let host = String(input[input.index(after: input.startIndex)..<closeBracket])
            let suffix = String(input[input.index(after: closeBracket)...])
            guard !host.isEmpty else {
                status = "Host cannot be empty."
                return nil
            }
            if suffix.isEmpty {
                return (host, defaultDaemonPort)
            }
            guard suffix.hasPrefix(":"), let port = UInt16(suffix.dropFirst()), port > 0 else {
                status = "Invalid port. Use a value from 1 to 65535."
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
                status = "Host cannot be empty."
                return nil
            }
            guard let port = UInt16(portPart), port > 0 else {
                status = "Invalid port. Use a value from 1 to 65535."
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

    func startServices() {
        guard !servicesStarted else { return }
        servicesStarted = true
        log("startServices()")

        // Start USB listener
        let usb = USBListener(decoder: decoder, audioPlayer: audioPlayer, avSync: avSync)
        self.usbListener = usb

        usb.onStatus = { [weak self] msg in
            Task { @MainActor in
                guard let self else { return }
                self.log("usb status: \(msg)")
                if self.usbConnected {
                    self.status = msg
                } else if msg.localizedCaseInsensitiveContains("failed") || msg.localizedCaseInsensitiveContains("error") {
                    self.status = msg
                }
            }
        }
        usb.onConnected = { [weak self] in
            Task { @MainActor in
                guard let self else { return }
                self.log("usb connected")
                self.usbConnected = true
                self.isConnected = true
                self.isConnecting = false
                self.transport = "USB"
                self.stream?.suppressTimeout = true
                self.audioPlayer.start()
                self.startPeripheralMonitoring()
            }
        }
        usb.onDisconnected = { [weak self] in
            Task { @MainActor in
                guard let self else { return }
                self.log("usb disconnected")
                self.usbConnected = false
                self.stream?.suppressTimeout = false
                self.fallbackToNetwork()
            }
        }
        usb.start()
        if !isConnected && !isConnecting {
            status = disconnectedPrompt()
        }
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
            status = disconnectedPrompt()
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
            status = disconnectedPrompt()
        }
    }

    func togglePinned(_ connection: RecentConnection) {
        if !connection.isPinned && pinnedConnections.count >= Self.maxPinnedConnections {
            status = "Pinned connections are limited to 10."
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
            status = disconnectedPrompt()
        }
    }

    func connectToEndpoint(_ endpoint: NWEndpoint, name: String) {
        log("connectToEndpoint(name=\(name), endpoint=\(endpoint)) start; isConnected=\(isConnected) isConnecting=\(isConnecting) usbConnected=\(usbConnected)")
        // Detach old stream's callbacks so stale async events can't interfere
        stream?.onStatus = nil
        stream?.onDisconnect = nil
        stream?.disconnect()
        closeNetworkControl(gracefully: true)
        pairingService?.cancel()

        isConnecting = true

        if !usbConnected {
            status = "Pairing with \(name)..."
        }

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
                control.start()

                self.startEncryptedStream(endpoint: endpoint, name: name, sessionKey: key, controlClient: control)

            case .pinRequired(let completion):
                self.log("PairingService result: PIN required")
                self.pendingPinCompletion = completion
                self.pinInput = ""
                self.showPinEntry = true
                self.pairingStatus = "Enter the PIN shown on the daemon"

            case .rejected(let reason):
                self.log("PairingService result: rejected (\(reason))")
                self.status = "Pairing rejected: \(reason)"
                self.pairingService = nil
                self.isConnecting = false

            case .error(let msg):
                self.log("PairingService result: error (\(msg))")
                self.status = "Pairing error: \(msg)"
                self.pairingService = nil
                self.isConnecting = false
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
        status = "Verifying PIN..."
        completion(pin)
        pendingPinCompletion = nil
    }

    func cancelPin() {
        log("cancelPin()")
        showPinEntry = false
        pendingPinCompletion = nil
        pairingService?.cancel()
        pairingService = nil
        status = "Pairing cancelled"
    }

    private func startEncryptedStream(endpoint: NWEndpoint, name: String, sessionKey: SymmetricKey, controlClient: NetworkControlClient) {
        log("startEncryptedStream(name=\(name), endpoint=\(endpoint))")
        if !usbConnected {
            status = "Connecting to \(name)..."
        }

        decoder.hasReportedFirstFrame = false

        let client = StreamClient(endpoint: endpoint, decoder: decoder, audioPlayer: audioPlayer, avSync: avSync)
        client.sessionKey = sessionKey
        client.sendPliRequest = { [weak controlClient] in
            controlClient?.sendPli()
        }
        self.stream = client

        client.onStatus = { [weak self, weak client] msg in
            Task { @MainActor in
                guard let self, let client else { return }
                guard self.stream === client else { return }
                self.log("StreamClient status: \(msg)")
                if !self.usbConnected {
                    self.status = msg
                    let nowConnected = msg.contains("Streaming")
                    if nowConnected && !self.isConnected {
                        let target = self.endpointHostAndPort(endpoint, fallbackHost: name)
                        self.isConnected = true
                        self.isConnecting = false
                        self.transport = "Network"
                        self.manualHost = formatEndpointInput(host: target.host, port: target.port)
                        self.rememberRecentConnection(name: name, host: target.host, port: target.port)
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

    /// Called when USB disconnects — try to resume network connection immediately
    private func fallbackToNetwork() {
        log("fallbackToNetwork() lastNetEndpoint=\(String(describing: lastNetEndpoint)) lastNetName=\(String(describing: lastNetName))")
        stream?.onStatus = nil
        stream?.onDisconnect = nil
        stream?.disconnect()
        stream = nil
        closeNetworkControl(gracefully: false)
        isConnected = false
        isConnecting = false
        transport = ""
        audioPlayer.stop()
        micCapture.stop()
        stopPeripheralMonitoring()
        status = "USB disconnected. \(disconnectedPrompt())"
    }

    /// Called when we've lost all streams and should return to idle state.
    private func handleStreamLost() {
        log("handleStreamLost() lastNetEndpoint=\(String(describing: lastNetEndpoint)) lastNetName=\(String(describing: lastNetName))")
        stream?.onStatus = nil
        stream?.onDisconnect = nil
        stream?.disconnect()
        stream = nil
        closeNetworkControl(gracefully: true)
        isConnected = false
        isConnecting = false
        transport = ""
        audioPlayer.stop()
        micCapture.stop()
        stopPeripheralMonitoring()
        status = "Daemon disconnected. \(disconnectedPrompt())"
    }

    func disconnect() {
        stream?.onStatus = nil
        stream?.onDisconnect = nil
        stream?.disconnect()
        stream = nil
        closeNetworkControl(gracefully: true)
        pairingService?.cancel()
        pairingService = nil
        usbListener?.stop()
        usbListener = nil
        usbConnected = false
        isConnected = false
        isConnecting = false
        transport = ""
        status = "Disconnected. \(disconnectedPrompt())"
        audioPlayer.stop()
        micCapture.stop()
        stopPeripheralMonitoring()
        lastNetEndpoint = nil
        lastNetName = nil
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
        for controller in GCController.controllers() {
            attachController(controller)
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
        } else {
            cameraCapture.onJPEG = { [weak self] jpeg in
                guard let self else { return }
                let fid = self.camFrameId
                self.camFrameId = self.camFrameId &+ 1
                if self.usbConnected, let usb = self.usbListener {
                    usb.sendCameraFrame(jpeg)
                } else if let stream = self.stream {
                    stream.sendCameraFrame(jpeg, frameId: fid)
                }
            }
            cameraCapture.start()
        }
        objectWillChange.send()
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
        } else {
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
}

enum ToolbarOrientation: String {
    case horizontal, vertical
}

struct ContentView: View {
    @EnvironmentObject private var model: StreamViewModel
    @State private var showOverlay = true

    @State private var barPosition: CGPoint = Self.loadBarPosition()
    @State private var barOrientation: ToolbarOrientation = Self.loadBarOrientation()
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
    private static let connectionRowHeight: CGFloat = 52
    private static let connectionListChromeHeight: CGFloat = 28

    private func connectionListHeight(for rowCount: Int) -> CGFloat {
        guard rowCount > 0 else { return 0 }
        let estimated = CGFloat(rowCount) * Self.connectionRowHeight + Self.connectionListChromeHeight
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

                if showOverlay {
                    VStack(alignment: .leading, spacing: 10) {
                        HStack {
                            Text("Screx").font(.headline)
                            if !model.transport.isEmpty {
                                Text(model.transport)
                                    .font(.caption2)
                                    .padding(.horizontal, 6)
                                    .padding(.vertical, 2)
                                    .background(model.transport == "USB" ? Color.green : Color.blue, in: Capsule())
                                    .foregroundStyle(.white)
                            }
                        }
                        Text(model.status).font(.caption).foregroundStyle(.secondary)

                        if !model.isConnected {
                            HStack {
                                TextField("Daemon host or IP[:port]", text: $model.manualHost)
                                    .textFieldStyle(.roundedBorder)
                                    .autocorrectionDisabled()
                                    .textInputAutocapitalization(.never)
                                    .keyboardType(.URL)
                                    .disabled(model.isConnecting)

                                Button(model.isConnecting ? "Connecting..." : "Connect") { model.connectManual() }
                                    .buttonStyle(.borderedProminent)
                                    .disabled(model.isConnecting || model.manualHost.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                            }

                            if !model.pinnedConnections.isEmpty {
                                VStack(alignment: .leading, spacing: 4) {
                                    Text("Pinned Connections")
                                        .font(.caption.weight(.semibold))

                                    List {
                                        ForEach(model.pinnedConnections) { connection in
                                            connectionRow(connection)
                                        }
                                    }
                                    .listStyle(.plain)
                                    .environment(\.defaultMinListRowHeight, Self.connectionRowHeight)
                                    .background(Color.white, in: RoundedRectangle(cornerRadius: 12))
                                    .clipShape(RoundedRectangle(cornerRadius: 12))
                                    .frame(height: connectionListHeight(for: model.pinnedConnections.count))
                                }
                            }

                            if !model.unpinnedRecentConnections.isEmpty {
                                VStack(alignment: .leading, spacing: 4) {
                                    HStack {
                                        Text("Recent Connections")
                                            .font(.caption.weight(.semibold))
                                        Spacer()
                                        Button("Clear") { model.clearRecentConnections() }
                                            .font(.caption.weight(.semibold))
                                            .padding(.horizontal, 8)
                                            .padding(.vertical, 4)
                                            .background(Color.red.opacity(0.18), in: Capsule())
                                            .foregroundStyle(.red)
                                            .buttonStyle(.plain)
                                    }

                                    List {
                                        ForEach(model.unpinnedRecentConnections) { connection in
                                            connectionRow(connection)
                                        }
                                    }
                                    .listStyle(.plain)
                                    .environment(\.defaultMinListRowHeight, Self.connectionRowHeight)
                                    .background(Color.white, in: RoundedRectangle(cornerRadius: 12))
                                    .clipShape(RoundedRectangle(cornerRadius: 12))
                                    .frame(height: connectionListHeight(for: model.unpinnedRecentConnections.count))
                                }
                            }
                        } else {
                            Button("Disconnect") { model.disconnect() }
                                .buttonStyle(.bordered)
                                .font(.caption)
                        }
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
                        .padding(.top, showOverlay ? 92 : 16)
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

                floatingBar(in: geo)
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
                    icon: model.isMicActive ? "mic.fill" : "mic",
                    active: model.isMicActive,
                    color: model.isMicActive ? .green : .white
                ) { model.toggleMic() }

                toolbarButton(
                    icon: model.isCameraActive
                        ? (model.isCameraFront ? "arrow.triangle.2.circlepath.camera.fill" : "video.fill")
                        : "video",
                    active: model.isCameraActive,
                    color: model.isCameraActive ? .green : .white
                ) { model.toggleCamera() }
                    .onLongPressGesture(minimumDuration: 0.5) { model.flipCamera() }

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
            }

            Image(systemName: showOverlay ? "info.circle.fill" : "info.circle")
                .font(.footnote)
                .foregroundStyle(.white)
                .frame(width: Self.btnSize, height: Self.btnSize)
                .contentShape(Circle())
                .onTapGesture {
                    withAnimation(.easeInOut(duration: 0.2)) { showOverlay.toggle() }
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
                    .font(.caption)
                    .foregroundStyle(connection.isPinned ? .yellow : .secondary)

                VStack(alignment: .leading, spacing: 2) {
                    Text(connection.displayName)
                        .font(.caption.weight(.medium))
                        .foregroundStyle(.primary)
                    Text(connection.endpointLabel)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }

                Spacer()
            }
            .padding(.vertical, 4)
        }
        .buttonStyle(.plain)
        .disabled(model.isConnecting)
        .listRowBackground(Color.clear)
        .listRowSeparator(.hidden)
        .swipeActions(edge: .leading, allowsFullSwipe: false) {
            Button(connection.isPinned ? "Unpin" : "Pin") {
                model.togglePinned(connection)
            }
            .tint(.yellow)
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
}

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
                    .onAppear { model.startDiscovery() }
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
        updatePhysicalMouseCapture(model.physicalMouseConnected)
    }

    override var prefersPointerLocked: Bool {
        model.physicalMouseConnected
    }

    private func observeModel() {
        model.$physicalMouseConnected
            .removeDuplicates()
            .sink { [weak self] active in
                self?.updatePhysicalMouseCapture(active)
            }
            .store(in: &cancellables)
    }

    private func updatePhysicalMouseCapture(_ active: Bool) {
        controllerUserInteractionEnabled = !active
        if active {
            _ = captureView.becomeFirstResponder()
        }
        if #available(iOS 14.0, *) {
            setNeedsUpdateOfPrefersPointerLocked()
        }
    }
}

@MainActor
final class StreamViewModel: ObservableObject {
    @Published var status: String = "Looking for daemon..."
    @Published var isConnected = false
    @Published var manualIP: String = ""
    @Published var transport: String = ""
    @Published var showPinEntry = false
    @Published var pinInput: String = ""
    @Published var pairingStatus: String = ""

    let decoder = VideoDecoder()
    let avSync = AVSyncState()
    let audioPlayer: AudioPlayer
    let cameraCapture = CameraCapture()
    let micCapture = MicCapture()

    private let discovery = DiscoveryService()
    private var stream: StreamClient?
    private var networkControl: NetworkControlClient?
    private var usbListener: USBListener?
    private var pairingService: PairingService?
    private var pendingPinCompletion: ((String) -> Void)?
    private var sessionKey: SymmetricKey?

    nonisolated init() {
        self.audioPlayer = AudioPlayer(avSync: avSync)
    }
    private var discoveryStarted = false
    private var usbConnected = false
    private var camFrameId: UInt32 = 0

    private var lastNetEndpoint: NWEndpoint?
    private var lastNetName: String?
    private var micSeq: UInt32 = 0
    private var isConnecting = false

    @Published var physicalMouseConnected = false
    @Published var physicalKeyboardConnected = false
    private var mouseObservers: [Any] = []
    private var keyboardObservers: [Any] = []

    private func log(_ message: String) {
        print("[app] \(message)")
    }

    func startDiscovery() {
        guard !discoveryStarted else { return }
        discoveryStarted = true
        log("startDiscovery()")

        // Start USB listener
        let usb = USBListener(decoder: decoder, audioPlayer: audioPlayer, avSync: avSync)
        self.usbListener = usb

        usb.onStatus = { [weak self] msg in
            Task { @MainActor in
                guard let self else { return }
                self.log("usb status: \(msg)")
                if self.usbConnected || !self.isConnected {
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

        // Start network discovery (beacon listener)
        discovery.onStatusUpdate = { [weak self] msg in
            Task { @MainActor in
                guard let self else { return }
                self.log("discovery status: \(msg)")
                if !self.usbConnected && !self.isConnected {
                    self.status = msg
                }
            }
        }
        discovery.onEndpointFound = { [weak self] ep in
            Task { @MainActor in
                guard let self else { return }
                let endpoint = NWEndpoint.hostPort(
                    host: NWEndpoint.Host(ep.host),
                    port: NWEndpoint.Port(integerLiteral: ep.port)
                )
                self.lastNetEndpoint = endpoint
                self.lastNetName = ep.name
                self.log("discovery found daemon: name=\(ep.name) host=\(ep.host) port=\(ep.port) isConnected=\(self.isConnected) isConnecting=\(self.isConnecting)")

                if !self.isConnected && !self.isConnecting {
                    self.connectToEndpoint(endpoint, name: ep.name)
                } else {
                    self.log("ignoring discovered endpoint because isConnected=\(self.isConnected) isConnecting=\(self.isConnecting)")
                }
            }
        }
        discovery.onDaemonLost = { [weak self] in
            Task { @MainActor in
                guard let self else { return }
                self.log("discovery reported daemon lost")
                self.lastNetEndpoint = nil
                self.lastNetName = nil
                // Don't tear down an active stream just because beacons stopped --
                // the stream has its own data timeout for true disconnections.
                // This allows manual IP connections (no beacons) to stay alive.
            }
        }
        discovery.startListening()
    }

    func connectManual() {
        let ip = manualIP.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !ip.isEmpty else { return }
        let host = NWEndpoint.Host(ip)
        let port = NWEndpoint.Port(integerLiteral: 9000)
        let endpoint = NWEndpoint.hostPort(host: host, port: port)
        lastNetEndpoint = endpoint
        lastNetName = ip
        connectToEndpoint(endpoint, name: ip)
    }

    func connectToEndpoint(_ endpoint: NWEndpoint, name: String) {
        log("connectToEndpoint(name=\(name), endpoint=\(endpoint)) start; isConnected=\(isConnected) isConnecting=\(isConnecting) usbConnected=\(usbConnected)")
        // Detach old stream's callbacks so stale async events can't interfere
        stream?.onStatus = nil
        stream?.onDisconnect = nil
        stream?.disconnect()
        networkControl?.onDisconnect = nil
        networkControl?.disconnect()
        networkControl = nil
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
            port = 9000
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
                // Retry after a delay
                DispatchQueue.main.asyncAfter(deadline: .now() + 2) { [weak self] in
                    guard let self, !self.isConnected else { return }
                    self.log("retrying connectToEndpoint after pairing error")
                    self.connectToEndpoint(endpoint, name: name)
                }
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
                        self.isConnected = true
                        self.isConnecting = false
                        self.transport = "Network"
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
        if let endpoint = lastNetEndpoint, let name = lastNetName {
            status = "USB disconnected, switching to network..."
            transport = "Network"
            stream?.disconnect()
            connectToEndpoint(endpoint, name: name)
        } else {
            isConnected = false
            transport = ""
            status = "USB disconnected, looking for daemon..."
            audioPlayer.stop()
            stopPeripheralMonitoring()
            discovery.resetKnownHost()
        }
    }

    /// Called when we've lost all streams and need to start looking again
    private func handleStreamLost() {
        log("handleStreamLost() lastNetEndpoint=\(String(describing: lastNetEndpoint)) lastNetName=\(String(describing: lastNetName))")
        stream?.onStatus = nil
        stream?.onDisconnect = nil
        stream?.disconnect()
        stream = nil
        networkControl?.onDisconnect = nil
        networkControl?.disconnect()
        networkControl = nil
        isConnected = false
        isConnecting = false
        transport = ""
        audioPlayer.stop()
        micCapture.stop()
        stopPeripheralMonitoring()

        // Try to reconnect immediately using the last known endpoint
        if let endpoint = lastNetEndpoint, let name = lastNetName {
            status = "Reconnecting to \(name)..."
            connectToEndpoint(endpoint, name: name)
        } else {
            status = "Daemon disconnected, looking..."
            discovery.resetKnownHost()
        }
    }

    func disconnect() {
        stream?.onStatus = nil
        stream?.onDisconnect = nil
        stream?.disconnect()
        stream = nil
        networkControl?.onDisconnect = nil
        networkControl?.disconnect()
        networkControl = nil
        pairingService?.cancel()
        pairingService = nil
        usbListener?.stop()
        usbListener = nil
        usbConnected = false
        isConnected = false
        isConnecting = false
        transport = ""
        status = "Disconnected"
        audioPlayer.stop()
        micCapture.stop()
        stopPeripheralMonitoring()
        lastNetEndpoint = nil
        lastNetName = nil
    }

    var displayLayer: AVSampleBufferDisplayLayer? {
        decoder.displayLayer
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
            Task { @MainActor in self.sendMouse(Data([0x02, 0x00, pressed ? 1 : 0])) }
        }

        if let right = input.rightButton {
            right.pressedChangedHandler = { [weak self] _, _, pressed in
                guard let self else { return }
                Task { @MainActor in self.sendMouse(Data([0x02, 0x01, pressed ? 1 : 0])) }
            }
        }

        if let middle = input.middleButton {
            middle.pressedChangedHandler = { [weak self] _, _, pressed in
                guard let self else { return }
                Task { @MainActor in self.sendMouse(Data([0x02, 0x02, pressed ? 1 : 0])) }
            }
        }

        input.scroll.yAxis.valueChangedHandler = { [weak self] _, value in
            guard let self else { return }
            var data = Data([0x03]) // MOUSE_SCROLL
            let dy = Int16(clamping: Int(value)).bigEndian
            withUnsafeBytes(of: dy) { data.append(contentsOf: $0) }
            Task { @MainActor in self.sendMouse(data) }
        }

        print("[periph] mouse attached")
    }

    private func detachMouse() {
        guard physicalMouseConnected else { return }
        physicalMouseConnected = false
        sendPeripheral(Data([0x01, 0x00])) // PERIPH_MOUSE, DETACHED
        print("[periph] mouse detached")
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
    @State private var pillSize: CGSize = CGSize(width: 80, height: 44)

    private static let btnSize: CGFloat = 32
    private static let btnSpacing: CGFloat = 6
    private static let edgeThreshold: CGFloat = 40

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
                                TextField("Daemon IP", text: $model.manualIP)
                                    .textFieldStyle(.roundedBorder)
                                    .autocorrectionDisabled()
                                    .textInputAutocapitalization(.never)
                                    .keyboardType(.numbersAndPunctuation)

                                Button("Connect") { model.connectManual() }
                                    .buttonStyle(.borderedProminent)
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
                if barPosition == .zero {
                    let halfW = pillSize.width / 2 + 4
                    let halfH = pillSize.height / 2 + 4
                    barPosition = CGPoint(x: geo.size.width - halfW, y: geo.size.height - halfH)
                }
            }
        }
        .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillShowNotification)) { notif in
            guard let frame = notif.userInfo?[UIResponder.keyboardFrameEndUserInfoKey] as? CGRect else { return }
            let kbH = frame.height
            keyboardHeight = kbH
            let halfH = pillSize.height / 2 + 4
            let maxY = UIScreen.main.bounds.height - kbH - halfH
            if barPosition.y > maxY {
                preKeyboardY = barPosition.y
                withAnimation(.easeOut(duration: 0.25)) {
                    barPosition.y = maxY
                }
            }
        }
        .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillHideNotification)) { _ in
            keyboardHeight = 0
            if let savedY = preKeyboardY {
                withAnimation(.easeOut(duration: 0.25)) {
                    barPosition.y = savedY
                }
                preKeyboardY = nil
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
    }

    // MARK: - Floating button bar

    @ViewBuilder
    private func floatingBar(in geo: GeometryProxy) -> some View {
        let layout = barOrientation == .vertical
            ? AnyLayout(VStackLayout(spacing: Self.btnSpacing))
            : AnyLayout(HStackLayout(spacing: Self.btnSpacing))

        let pos = CGPoint(
            x: barPosition.x + dragOffset.width,
            y: barPosition.y + dragOffset.height
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
                    active: isKeyboardActive,
                    color: isKeyboardActive ? .green : .white
                ) { isKeyboardActive.toggle() }
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
                    .onChange(of: model.isConnected) { _ in
                        DispatchQueue.main.async { pillSize = pillGeo.size }
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

                    let halfW = pillSize.width / 2 + 4
                    let halfH = pillSize.height / 2 + 4

                    newPos.x = max(halfW, min(newPos.x, geo.size.width - halfW))
                    newPos.y = max(halfH, min(newPos.y, geo.size.height - halfH))

                    if keyboardHeight > 0 {
                        let maxY = geo.size.height - keyboardHeight - halfH
                        newPos.y = min(newPos.y, maxY)
                        preKeyboardY = nil
                    }

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

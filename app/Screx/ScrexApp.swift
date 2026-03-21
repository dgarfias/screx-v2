import SwiftUI
import Combine
import AVFoundation
import Network

@main
struct ScrexApp: App {
    @StateObject private var model = StreamViewModel()

    init() {
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
    }

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environmentObject(model)
                .onAppear { model.startDiscovery() }
        }
    }
}

@MainActor
final class StreamViewModel: ObservableObject {
    @Published var status: String = "Looking for daemon..."
    @Published var isConnected = false
    @Published var manualIP: String = ""
    @Published var transport: String = ""

    let decoder = H264Decoder()
    let avSync = AVSyncState()
    let audioPlayer: AudioPlayer
    let cameraCapture = CameraCapture()
    let micCapture = MicCapture()

    private let discovery = DiscoveryService()
    private var stream: StreamClient?
    private var usbListener: USBListener?

    nonisolated init() {
        self.audioPlayer = AudioPlayer(avSync: avSync)
    }
    private var discoveryStarted = false
    private var usbConnected = false
    private var camFrameId: UInt32 = 0

    /// Remembered endpoint so we can reconnect WiFi without waiting for a new beacon
    private var lastWifiEndpoint: NWEndpoint?
    private var lastWifiName: String?
    private var micSeq: UInt32 = 0

    func startDiscovery() {
        guard !discoveryStarted else { return }
        discoveryStarted = true

        // Start USB listener
        let usb = USBListener(decoder: decoder, audioPlayer: audioPlayer, avSync: avSync)
        self.usbListener = usb

        usb.onStatus = { [weak self] msg in
            Task { @MainActor in
                guard let self else { return }
                if self.usbConnected || !self.isConnected {
                    self.status = msg
                }
            }
        }
        usb.onConnected = { [weak self] in
            Task { @MainActor in
                guard let self else { return }
                self.usbConnected = true
                self.isConnected = true
                self.transport = "USB"
                self.stream?.suppressTimeout = true
                self.audioPlayer.start()
            }
        }
        usb.onDisconnected = { [weak self] in
            Task { @MainActor in
                guard let self else { return }
                self.usbConnected = false
                self.stream?.suppressTimeout = false
                self.fallbackToWifi()
            }
        }
        usb.start()

        // Start WiFi discovery (beacon listener)
        discovery.onStatusUpdate = { [weak self] msg in
            Task { @MainActor in
                guard let self else { return }
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
                self.lastWifiEndpoint = endpoint
                self.lastWifiName = ep.name

                if !self.isConnected {
                    self.connectToEndpoint(endpoint, name: ep.name)
                }
            }
        }
        discovery.onDaemonLost = { [weak self] in
            Task { @MainActor in
                guard let self else { return }
                self.lastWifiEndpoint = nil
                self.lastWifiName = nil
                if !self.usbConnected {
                    self.handleStreamLost()
                }
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
        lastWifiEndpoint = endpoint
        lastWifiName = ip
        connectToEndpoint(endpoint, name: ip)
    }

    func connectToEndpoint(_ endpoint: NWEndpoint, name: String) {
        stream?.disconnect()
        if !usbConnected {
            status = "Connecting to \(name)..."
        }

        let client = StreamClient(endpoint: endpoint, decoder: decoder, audioPlayer: audioPlayer, avSync: avSync)
        self.stream = client

        client.onStatus = { [weak self] msg in
            Task { @MainActor in
                guard let self else { return }
                if !self.usbConnected {
                    self.status = msg
                    let nowConnected = msg.contains("Streaming")
                    if nowConnected && !self.isConnected {
                        self.isConnected = true
                        self.transport = "WiFi"
                        self.audioPlayer.start()
                    }
                }
            }
        }
        client.onDisconnect = { [weak self] in
            Task { @MainActor in
                guard let self else { return }
                self.stream = nil
                if !self.usbConnected {
                    self.handleStreamLost()
                }
            }
        }
        client.connect()
    }

    /// Called when USB disconnects — try to resume WiFi immediately
    private func fallbackToWifi() {
        if let endpoint = lastWifiEndpoint, let name = lastWifiName {
            status = "USB disconnected, switching to WiFi..."
            transport = "WiFi"
            // Reconnect WiFi using the remembered endpoint
            stream?.disconnect()
            connectToEndpoint(endpoint, name: name)
        } else {
            isConnected = false
            transport = ""
            status = "USB disconnected, looking for daemon..."
            audioPlayer.stop()
            discovery.resetKnownHost()
        }
    }

    /// Called when we've lost all streams and need to start looking again
    private func handleStreamLost() {
        stream?.disconnect()
        stream = nil
        isConnected = false
        transport = ""
        status = "Daemon disconnected, looking..."
        audioPlayer.stop()
        micCapture.stop()
        discovery.resetKnownHost()
    }

    func disconnect() {
        stream?.disconnect()
        stream = nil
        usbListener?.stop()
        usbListener = nil
        usbConnected = false
        isConnected = false
        transport = ""
        status = "Disconnected"
        audioPlayer.stop()
        micCapture.stop()
        lastWifiEndpoint = nil
        lastWifiName = nil
    }

    var displayLayer: AVSampleBufferDisplayLayer? {
        decoder.displayLayer
    }

    func sendTouch(_ data: Data) {
        if usbConnected, let usb = usbListener {
            usb.sendTouch(data)
        } else if let stream {
            stream.sendTouch(data)
        }
    }

    func sendKey(_ keyData: Data) {
        if usbConnected, let usb = usbListener {
            usb.sendKey(keyData)
        } else if let stream {
            stream.sendKey(keyData)
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
                        onTouch: { data in model.sendTouch(data) }
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
                    dragOffset = .zero

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

                    withAnimation(.easeOut(duration: 0.2)) {
                        barPosition = newPos
                        barOrientation = newOrientation
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

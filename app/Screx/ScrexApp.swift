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
            try session.setCategory(.playback, mode: .default, options: [.mixWithOthers])
            try session.setActive(true)
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
    let audioPlayer = AudioPlayer()
    let micCapture = MicCapture()
    let cameraCapture = CameraCapture()

    private let discovery = DiscoveryService()
    private var stream: StreamClient?
    private var usbListener: USBListener?
    private var discoveryStarted = false
    private var usbConnected = false
    private var camFrameId: UInt32 = 0

    /// Remembered endpoint so we can reconnect WiFi without waiting for a new beacon
    private var lastWifiEndpoint: NWEndpoint?
    private var lastWifiName: String?

    func startDiscovery() {
        guard !discoveryStarted else { return }
        discoveryStarted = true

        // Start USB listener
        let usb = USBListener(decoder: decoder, audioPlayer: audioPlayer)
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

        let client = StreamClient(endpoint: endpoint, decoder: decoder, audioPlayer: audioPlayer)
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

    // MARK: - Mic

    func toggleMic() {
        if micCapture.isRunning {
            micCapture.stop()
        } else {
            micCapture.onPCM = { [weak self] pcm in
                guard let self else { return }
                if self.usbConnected, let usb = self.usbListener {
                    usb.sendMicAudio(pcm)
                } else if let stream = self.stream {
                    stream.sendMicAudio(pcm)
                }
            }
            micCapture.start()
        }
        objectWillChange.send()
    }

    var isMicActive: Bool { micCapture.isRunning }

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
}

struct ContentView: View {
    @EnvironmentObject private var model: StreamViewModel
    @State private var showOverlay = true
    @State private var keyboardActive = false

    var body: some View {
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

            KeyboardInputView(
                isActive: $keyboardActive,
                onText: { text in model.sendTextInsert(text) },
                onDelete: { model.sendSpecialKey(0x01) }
            )
            .frame(width: 0, height: 0)

            VStack {
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
                    .padding(.top, 8)
                    .transition(.move(edge: .top).combined(with: .opacity))
                }

                Spacer()

                HStack(spacing: 6) {
                    Spacer()
                    if model.isConnected {
                        Button { model.toggleMic() } label: {
                            Image(systemName: model.isMicActive ? "mic.fill" : "mic")
                                .font(.footnote)
                                .foregroundStyle(model.isMicActive ? .green : .white)
                                .frame(width: 32, height: 32)
                                .background(.ultraThinMaterial, in: Circle())
                        }
                        Button { model.toggleCamera() } label: {
                            Image(systemName: model.isCameraActive
                                  ? (model.isCameraFront ? "arrow.triangle.2.circlepath.camera.fill" : "video.fill")
                                  : "video")
                                .font(.footnote)
                                .foregroundStyle(model.isCameraActive ? .green : .white)
                                .frame(width: 32, height: 32)
                                .background(.ultraThinMaterial, in: Circle())
                        }
                        .simultaneousGesture(
                            LongPressGesture(minimumDuration: 0.5).onEnded { _ in
                                model.flipCamera()
                            }
                        )
                        Button { keyboardActive.toggle() } label: {
                            Image(systemName: keyboardActive ? "keyboard.fill" : "keyboard")
                                .font(.footnote)
                                .foregroundStyle(.white)
                                .frame(width: 32, height: 32)
                                .background(.ultraThinMaterial, in: Circle())
                        }
                    }
                    Button {
                        withAnimation(.easeInOut(duration: 0.2)) { showOverlay.toggle() }
                    } label: {
                        Image(systemName: showOverlay ? "info.circle.fill" : "info.circle")
                            .font(.footnote)
                            .foregroundStyle(.white)
                            .frame(width: 32, height: 32)
                            .background(.ultraThinMaterial, in: Circle())
                    }
                }
                .padding(.trailing, 12)
                .padding(.bottom, 8)
            }
        }
        .statusBarHidden(true)
        .persistentSystemOverlays(.hidden)
    }
}

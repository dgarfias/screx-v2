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

    private let discovery = DiscoveryService()
    private var stream: StreamClient?
    private var usbListener: USBListener?
    private var discoveryStarted = false
    private var usbConnected = false

    func startDiscovery() {
        guard !discoveryStarted else { return }
        discoveryStarted = true

        // Start USB listener
        let usb = USBListener(decoder: decoder, audioPlayer: audioPlayer)
        self.usbListener = usb

        usb.onStatus = { [weak self] msg in
            Task { @MainActor in
                self?.status = msg
            }
        }
        usb.onConnected = { [weak self] in
            Task { @MainActor in
                self?.usbConnected = true
                self?.isConnected = true
                self?.transport = "USB"
            }
        }
        usb.onDisconnected = { [weak self] in
            Task { @MainActor in
                self?.usbConnected = false
                if self?.stream == nil {
                    self?.isConnected = false
                    self?.transport = ""
                    self?.status = "USB disconnected, looking for WiFi..."
                }
            }
        }
        usb.start()

        // Start WiFi discovery
        discovery.onStatusUpdate = { [weak self] msg in
            Task { @MainActor in
                guard let self else { return }
                if !self.usbConnected {
                    self.status = msg
                }
            }
        }
        discovery.onEndpointsChanged = { [weak self] endpoints in
            Task { @MainActor in
                guard let self, !self.isConnected, let ep = endpoints.first else { return }
                self.connectToEndpoint(ep.endpoint, name: ep.name)
            }
        }
        discovery.startBrowsing()
    }

    func connectManual() {
        let ip = manualIP.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !ip.isEmpty else { return }
        let host = NWEndpoint.Host(ip)
        let port = NWEndpoint.Port(integerLiteral: 9000)
        connectToEndpoint(.hostPort(host: host, port: port), name: ip)
    }

    func connectToEndpoint(_ endpoint: NWEndpoint, name: String) {
        stream?.disconnect()
        status = "Connecting to \(name)..."

        let client = StreamClient(endpoint: endpoint, decoder: decoder, audioPlayer: audioPlayer)
        self.stream = client

        client.onStatus = { [weak self] msg in
            Task { @MainActor in
                guard let self else { return }
                if !self.usbConnected {
                    self.status = msg
                    self.isConnected = msg.contains("Streaming")
                    if self.isConnected {
                        self.transport = "WiFi"
                    }
                }
            }
        }
        client.connect()
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
    }

    var displayLayer: AVSampleBufferDisplayLayer? {
        decoder.displayLayer
    }
}

struct ContentView: View {
    @EnvironmentObject private var model: StreamViewModel
    @State private var showOverlay = true

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()

            if let layer = model.displayLayer {
                VideoDisplayView(layer: layer)
                    .ignoresSafeArea()
            }

            VStack {
                HStack {
                    Spacer()
                    Button {
                        withAnimation(.easeInOut(duration: 0.2)) { showOverlay.toggle() }
                    } label: {
                        Image(systemName: showOverlay ? "info.circle.fill" : "info.circle")
                            .font(.title2)
                            .foregroundStyle(.white)
                            .padding(10)
                            .background(.ultraThinMaterial, in: Circle())
                    }
                    .padding(.trailing, 16)
                    .padding(.top, 8)
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
                    .transition(.move(edge: .top).combined(with: .opacity))
                }

                Spacer()
            }
        }
        .statusBarHidden(true)
        .persistentSystemOverlays(.hidden)
    }
}

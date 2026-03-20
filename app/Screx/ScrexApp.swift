import SwiftUI
import Combine
import AVFoundation

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

    private let discovery = DiscoveryService()
    private var stream: StreamClient?
    private var discoveryStarted = false

    func startDiscovery() {
        guard !discoveryStarted else { return }
        discoveryStarted = true

        discovery.onStatusUpdate = { [weak self] msg in
            Task { @MainActor in self?.status = msg }
        }
        discovery.onEndpointsChanged = { [weak self] endpoints in
            Task { @MainActor in
                guard let self, self.stream == nil, let ep = endpoints.first else { return }
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

        let client = StreamClient(endpoint: endpoint)
        self.stream = client

        client.onStatus = { [weak self] msg in
            Task { @MainActor in
                self?.status = msg
                self?.isConnected = msg.contains("Streaming")
            }
        }
        client.connect()
    }

    func disconnect() {
        stream?.disconnect()
        stream = nil
        isConnected = false
        status = "Disconnected"
    }

    var displayLayer: AVSampleBufferDisplayLayer? {
        stream?.decoder.displayLayer
    }

    func sendTouch(_ event: TouchEvent) {
        stream?.sendTouch(event)
    }
}

struct ContentView: View {
    @EnvironmentObject private var model: StreamViewModel
    @State private var showOverlay = true

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()

            if let layer = model.displayLayer {
                VideoDisplayView(layer: layer) { event in
                    model.sendTouch(event)
                }
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
                        Text("Screx").font(.headline)
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

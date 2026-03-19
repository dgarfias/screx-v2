import SwiftUI
import Combine
import AVFoundation

@main
struct ScrexApp: App {
    @StateObject private var model = StreamViewModel()

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
    @Published var status: String = "Searching for daemon..."
    @Published var isConnected = false

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

        Task { [weak self] in
            try? await Task.sleep(nanoseconds: 3_000_000_000)
            await MainActor.run {
                guard let self, self.stream == nil else { return }
                self.status = "Discovery timed out, trying screx-daemon.local:9000..."
                let host = NWEndpoint.Host("screx-daemon.local")
                let port = NWEndpoint.Port(integerLiteral: 9000)
                self.connectToEndpoint(.hostPort(host: host, port: port), name: "screx-daemon.local")
            }
        }
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
                    VStack(alignment: .leading, spacing: 8) {
                        Text("Screx").font(.headline)
                        Text(model.status).font(.caption).foregroundStyle(.secondary)

                        if model.isConnected {
                            Button("Disconnect") { model.disconnect() }
                                .buttonStyle(.bordered)
                                .font(.caption)
                        }
                    }
                    .padding(12)
                    .frame(maxWidth: 360, alignment: .leading)
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

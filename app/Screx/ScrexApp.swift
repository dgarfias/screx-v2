import Foundation
import SwiftUI
import Network
import Combine

@main
struct ScrexApp: App {
    @StateObject private var model = StreamViewModel()

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environmentObject(model)
                .task {
                    model.start()
                }
        }
    }
}

@MainActor
final class StreamViewModel: ObservableObject {
    @Published var statusText: String = "Idle"
    @Published var latencyText: String = "-- ms"
    @Published var jitterText: String = "-- ms"
    @Published var packetLossText: String = "--"
    @Published var droppedPacketsText: String = "--"
    @Published var droppedFramesText: String = "--"
    @Published var decoderErrorText: String = "--"
    @Published var connectedEndpointName: String = "None"
    @Published var discoveredEndpoints: [StreamEndpoint] = []

    let discovery = DiscoveryService()
    let transport = TransportService()
    let decoder = DecoderService()

    private var hasConnected = false

    init() {
        discovery.onStatusUpdate = { [weak self] status in
            Task { @MainActor in
                self?.statusText = status
            }
        }
        discovery.onEndpointsChanged = { [weak self] endpoints in
            Task { @MainActor in
                guard let self else { return }
                self.discoveredEndpoints = endpoints
                guard !self.hasConnected, let first = endpoints.first else { return }
                self.connect(to: first)
            }
        }

        transport.onStatusUpdate = { [weak self] status in
            Task { @MainActor in
                self?.statusText = status
                if status.contains("failed") || status.contains("disconnected") {
                    self?.hasConnected = false
                    self?.reconnectIfPossible()
                }
            }
        }
        transport.onMetricsUpdate = { [weak self] metrics in
            Task { @MainActor in
                self?.latencyText = String(format: "%.1f ms", metrics.estimatedOneWayLatencyMs)
                self?.jitterText = String(format: "%.1f ms", metrics.jitterMs)
                self?.packetLossText = String(format: "%.2f%%", metrics.lossPercent)
                self?.droppedPacketsText = "\(metrics.droppedPackets)"
            }
        }

        decoder.onRequestIDR = { [weak self] in
            self?.transport.sendControl(message: "IDR")
        }
        decoder.onHealthUpdate = { [weak self] health in
            Task { @MainActor in
                self?.droppedFramesText = "\(health.droppedFrames)"
                self?.decoderErrorText = "\(health.displayErrors)"
            }
        }
    }

    func start() {
        statusText = "Discovering services..."
        transport.startListening { [weak self] nalu, timestamp90k, isAccessUnitEnd in
            self?.decoder.handleNalu(
                nalu: nalu,
                timestamp90k: timestamp90k,
                isAccessUnitEnd: isAccessUnitEnd
            )
        }
        discovery.startBrowsing()
    }

    func connect(to endpoint: StreamEndpoint) {
        hasConnected = true
        connectedEndpointName = endpoint.name
        statusText = "Connecting to \(endpoint.name)..."
        transport.connect(endpoint: endpoint.endpoint) { [weak self] nalu, timestamp90k, isAccessUnitEnd in
            self?.decoder.handleNalu(
                nalu: nalu,
                timestamp90k: timestamp90k,
                isAccessUnitEnd: isAccessUnitEnd
            )
        }
    }

    func requestIDR() {
        transport.sendControl(message: "IDR")
    }

    func setBitrate(_ bps: Int) {
        transport.sendControl(message: "BITRATE:\(bps)")
    }

    func setResolution(width: Int, height: Int) {
        transport.sendControl(message: "RES:\(width)x\(height)")
    }

    private func reconnectIfPossible() {
        guard !hasConnected, let first = discoveredEndpoints.first else { return }
        Task { @MainActor in
            try? await Task.sleep(nanoseconds: 1_000_000_000)
            if !self.hasConnected {
                self.connect(to: first)
            }
        }
    }
}

struct ContentView: View {
    @EnvironmentObject private var model: StreamViewModel
    @State private var showInfoPanel = true

    var body: some View {
        ZStack(alignment: .topTrailing) {
            DisplayView(layer: model.decoder.displayLayer)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(Color.black)
                .ignoresSafeArea()

            VStack(alignment: .trailing, spacing: 8) {
                Button {
                    withAnimation(.easeInOut(duration: 0.2)) {
                        showInfoPanel.toggle()
                    }
                } label: {
                    Image(systemName: showInfoPanel ? "info.circle.fill" : "info.circle")
                        .font(.title3.weight(.semibold))
                }
                .buttonStyle(.borderedProminent)

                if showInfoPanel {
                    diagnosticsPanel
                        .transition(.move(edge: .trailing).combined(with: .opacity))
                }
            }
            .padding()
        }
    }

    private var diagnosticsPanel: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Screx V2")
                .font(.headline)
            Text("Status: \(model.statusText)")
            Text("Connected: \(model.connectedEndpointName)")
            Text("Latency: \(model.latencyText)")
            Text("Jitter: \(model.jitterText)")
            Text("Packet loss: \(model.packetLossText)")
            Text("Dropped packets: \(model.droppedPacketsText)")
            Text("Dropped frames: \(model.droppedFramesText)")
            Text("Decoder errors: \(model.decoderErrorText)")

            Divider()

            HStack(spacing: 6) {
                Button("IDR") {
                    model.requestIDR()
                }
                Button("8 Mbps") {
                    model.setBitrate(8_000_000)
                }
                Button("12 Mbps") {
                    model.setBitrate(12_000_000)
                }
            }
            .buttonStyle(.bordered)

            HStack(spacing: 6) {
                Button("720p") {
                    model.setResolution(width: 1280, height: 720)
                }
                Button("1080p") {
                    model.setResolution(width: 1920, height: 1080)
                }
            }
            .buttonStyle(.bordered)

            if !model.discoveredEndpoints.isEmpty {
                Divider()
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 6) {
                        ForEach(model.discoveredEndpoints) { endpoint in
                            Button(endpoint.name) {
                                model.connect(to: endpoint)
                            }
                            .buttonStyle(.borderedProminent)
                        }
                    }
                }
            }
        }
        .font(.caption)
        .padding(10)
        .frame(maxWidth: 360, alignment: .leading)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 14))
    }
}

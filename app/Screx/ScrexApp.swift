import SwiftUI
import WebKit
import Combine

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
    @Published var daemonURL: URL?
    @Published var manualHost: String = ""

    private let discovery = DiscoveryService()
    private var discoveryStarted = false

    func startDiscovery() {
        guard !discoveryStarted else { return }
        discoveryStarted = true

        discovery.onStatusUpdate = { [weak self] msg in
            Task { @MainActor in self?.status = msg }
        }
        discovery.onEndpointsChanged = { [weak self] endpoints in
            Task { @MainActor in
                guard let self, let ep = endpoints.first else { return }
                self.status = "Discovered \(ep.name)"
                self.daemonURL = ep.url
            }
        }
        discovery.startBrowsing()
    }

    func connectManually() {
        let host = manualHost.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !host.isEmpty else { return }
        connectTo(host: host)
    }

    private func connectTo(host: String) {
        let clean = host.contains("://") ? host : "http://\(host):8080"
        guard let url = URL(string: clean) else {
            status = "Invalid URL: \(clean)"
            return
        }
        status = "Connecting to \(url.absoluteString)..."
        daemonURL = url
    }
}

struct ContentView: View {
    @EnvironmentObject private var model: StreamViewModel
    @State private var showOverlay = true

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()

            if let url = model.daemonURL {
                WebRTCReceiverView(url: url)
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
                    connectionPanel
                        .transition(.move(edge: .top).combined(with: .opacity))
                }

                Spacer()
            }
        }
        .statusBarHidden(true)
        .persistentSystemOverlays(.hidden)
    }

    private var connectionPanel: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Screx").font(.headline)
            Text(model.status).font(.caption).foregroundStyle(.secondary)

            if model.daemonURL == nil {
                HStack {
                    TextField("Daemon IP (e.g. 192.168.1.100)", text: $model.manualHost)
                        .textFieldStyle(.roundedBorder)
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                        .keyboardType(.numbersAndPunctuation)

                    Button("Connect") { model.connectManually() }
                        .buttonStyle(.borderedProminent)
                }
            } else {
                HStack {
                    Text("Connected to: \(model.daemonURL!.host ?? "?")")
                        .font(.caption)
                    Spacer()
                    Button("Disconnect") {
                        model.daemonURL = nil
                        model.status = "Disconnected"
                    }
                    .buttonStyle(.bordered)
                    .font(.caption)
                }
            }
        }
        .padding(12)
        .frame(maxWidth: 400)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 14))
        .padding(.horizontal, 16)
    }
}

struct WebRTCReceiverView: UIViewRepresentable {
    let url: URL

    func makeUIView(context: Context) -> WKWebView {
        let config = WKWebViewConfiguration()
        config.allowsInlineMediaPlayback = true
        config.mediaTypesRequiringUserActionForPlayback = []

        let webView = WKWebView(frame: .zero, configuration: config)
        webView.isOpaque = true
        webView.backgroundColor = .black
        webView.scrollView.isScrollEnabled = false
        webView.scrollView.bounces = false
        webView.load(URLRequest(url: url))
        return webView
    }

    func updateUIView(_ webView: WKWebView, context: Context) {
        if webView.url != url {
            webView.load(URLRequest(url: url))
        }
    }
}

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
                if self.daemonURL == nil {
                    self.status = "Discovered \(ep.name)"
                    self.daemonURL = ep.url
                }
            }
        }
        discovery.startBrowsing()

        // Fallback for cases where Bonjour finds no entries yet: try the
        // conventional local mDNS hostname used by the daemon.
        Task { [weak self] in
            try? await Task.sleep(nanoseconds: 2_500_000_000)
            await MainActor.run {
                guard let self, self.daemonURL == nil else { return }
                self.status = "No service yet, trying screx-daemon.local..."
                self.connectTo(host: "screx-daemon.local")
            }
        }
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
    @State private var showManualEntry = false

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()

            if let url = model.daemonURL {
                WebRTCReceiverView(url: url) { webStatus in
                    model.status = webStatus
                }
                    .ignoresSafeArea()
                    .allowsHitTesting(false)
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
            .zIndex(100)
        }
        .statusBarHidden(true)
        .persistentSystemOverlays(.hidden)
    }

    private var connectionPanel: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Screx").font(.headline)
            Text(model.status).font(.caption).foregroundStyle(.secondary)

            if model.daemonURL == nil {
                Button(showManualEntry ? "Hide Manual IP" : "Enter IP Manually") {
                    withAnimation(.easeInOut(duration: 0.2)) {
                        showManualEntry.toggle()
                    }
                }
                .buttonStyle(.bordered)

                if showManualEntry {
                    HStack {
                        TextField("Daemon IP (e.g. 192.168.1.100)", text: $model.manualHost)
                            .textFieldStyle(.roundedBorder)
                            .autocorrectionDisabled()
                            .textInputAutocapitalization(.never)
                            .keyboardType(.numbersAndPunctuation)

                        Button("Connect") { model.connectManually() }
                            .buttonStyle(.borderedProminent)
                    }
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
    let onStatus: (String) -> Void

    func makeUIView(context: Context) -> WKWebView {
        let config = WKWebViewConfiguration()
        config.allowsInlineMediaPlayback = true
        config.mediaTypesRequiringUserActionForPlayback = []

        let webView = WKWebView(frame: .zero, configuration: config)
        webView.navigationDelegate = context.coordinator
        webView.isOpaque = true
        webView.backgroundColor = .black
        webView.scrollView.isScrollEnabled = false
        webView.scrollView.bounces = false
        context.coordinator.lastRequestedURL = url
        webView.load(URLRequest(url: url))
        return webView
    }

    func updateUIView(_ webView: WKWebView, context: Context) {
        if !isSameEndpointURL(lhs: webView.url, rhs: url) &&
            !isSameEndpointURL(lhs: context.coordinator.lastRequestedURL, rhs: url) {
            context.coordinator.lastRequestedURL = url
            onStatus("Loading stream page...")
            webView.load(URLRequest(url: url))
        }
    }

    func makeCoordinator() -> Coordinator {
        Coordinator(onStatus: onStatus)
    }

    final class Coordinator: NSObject, WKNavigationDelegate {
        private let onStatus: (String) -> Void
        var lastRequestedURL: URL?

        init(onStatus: @escaping (String) -> Void) {
            self.onStatus = onStatus
        }

        func webView(_ webView: WKWebView, didStartProvisionalNavigation navigation: WKNavigation!) {
            onStatus("Loading stream page...")
        }

        func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
            onStatus("Page loaded, negotiating WebRTC...")
        }

        func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) {
            let nsError = error as NSError
            if nsError.domain == NSURLErrorDomain && nsError.code == NSURLErrorCancelled {
                return
            }
            onStatus("Load failed: \(error.localizedDescription)")
        }

        func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) {
            let nsError = error as NSError
            if nsError.domain == NSURLErrorDomain && nsError.code == NSURLErrorCancelled {
                return
            }
            onStatus("Navigation failed: \(error.localizedDescription)")
        }

        func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
            onStatus("Web content process terminated")
        }
    }

    private func isSameEndpointURL(lhs: URL?, rhs: URL?) -> Bool {
        guard let lhs, let rhs else { return lhs == nil && rhs == nil }
        return normalizedURLKey(lhs) == normalizedURLKey(rhs)
    }

    private func normalizedURLKey(_ url: URL) -> String {
        let scheme = url.scheme?.lowercased() ?? "http"
        let host = url.host?.lowercased() ?? ""
        let port: Int = {
            if let p = url.port { return p }
            return scheme == "https" ? 443 : 80
        }()
        let path = (url.path.isEmpty || url.path == "/") ? "/" : url.path
        return "\(scheme)://\(host):\(port)\(path)"
    }
}

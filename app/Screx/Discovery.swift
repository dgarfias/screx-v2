import Foundation
import Network

struct StreamEndpoint: Identifiable {
    let id = UUID()
    let name: String
    let endpoint: NWEndpoint
}

final class DiscoveryService {
    private var browser: NWBrowser?
    private let queue = DispatchQueue(label: "screx.discovery", qos: .userInteractive)

    var onStatusUpdate: ((String) -> Void)?
    var onEndpointsChanged: (([StreamEndpoint]) -> Void)?

    private func emitStatus(_ text: String) {
        DispatchQueue.main.async { [weak self] in
            self?.onStatusUpdate?(text)
        }
    }

    private func emitEndpoints(_ endpoints: [StreamEndpoint]) {
        DispatchQueue.main.async { [weak self] in
            self?.onEndpointsChanged?(endpoints)
        }
    }

    func startBrowsing() {
        guard browser == nil else { return }

        let descriptor = NWBrowser.Descriptor.bonjour(type: "_screx._udp", domain: "local.")
        let params = NWParameters()
        params.includePeerToPeer = true
        let browser = NWBrowser(for: descriptor, using: params)
        self.browser = browser

        browser.stateUpdateHandler = { [weak self] state in
            switch state {
            case .ready:
                self?.emitStatus("Discovery ready, looking for daemon...")
                print("[discovery] browser ready")
            case .failed(let error):
                self?.emitStatus("Discovery failed: \(error.localizedDescription)")
                print("[discovery] browser failed: \(error)")
                // Restart on failure after a delay
                DispatchQueue.main.asyncAfter(deadline: .now() + 2) { [weak self] in
                    self?.stopBrowsing()
                    self?.startBrowsing()
                }
            case .waiting(let error):
                self?.emitStatus("Discovery waiting: \(error.localizedDescription)")
                print("[discovery] browser waiting: \(error)")
            default:
                break
            }
        }

        browser.browseResultsChangedHandler = { [weak self] results, changes in
            let endpoints: [StreamEndpoint] = results.compactMap { result in
                let name: String
                switch result.endpoint {
                case .service(let svcName, _, _, _):
                    name = svcName
                default:
                    name = "\(result.endpoint)"
                }
                print("[discovery] found: \(name) -> \(result.endpoint)")
                return StreamEndpoint(name: name, endpoint: result.endpoint)
            }
            if endpoints.isEmpty {
                self?.emitStatus("Discovery ready, looking for daemon...")
            } else {
                self?.emitStatus("Found \(endpoints.count) daemon(s)")
            }
            self?.emitEndpoints(endpoints)
        }

        browser.start(queue: queue)
    }

    func stopBrowsing() {
        browser?.cancel()
        browser = nil
    }
}

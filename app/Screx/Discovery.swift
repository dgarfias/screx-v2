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

        let params = NWParameters()
        params.includePeerToPeer = false

        let browser = NWBrowser(
            for: .bonjour(type: "_screx._tcp", domain: nil),
            using: params
        )
        self.browser = browser

        browser.stateUpdateHandler = { [weak self] state in
            switch state {
            case .ready:
                self?.emitStatus("Discovery ready, looking for daemon...")
            case .failed(let error):
                self?.emitStatus("Discovery failed: \(error.localizedDescription)")
            case .waiting(let error):
                self?.emitStatus("Discovery waiting: \(error.localizedDescription)")
            default:
                break
            }
        }

        browser.browseResultsChangedHandler = { [weak self] results, _ in
            let endpoints: [StreamEndpoint] = results.compactMap { result in
                let name: String
                switch result.endpoint {
                case .service(let svcName, _, _, _):
                    name = svcName
                default:
                    name = "\(result.endpoint)"
                }
                return StreamEndpoint(name: name, endpoint: result.endpoint)
            }
            self?.emitStatus("Found \(endpoints.count) daemon(s)")
            self?.emitEndpoints(endpoints)
        }

        browser.start(queue: queue)
    }

    func stopBrowsing() {
        browser?.cancel()
        browser = nil
    }
}

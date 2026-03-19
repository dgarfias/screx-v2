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
        let parameters = NWParameters.udp
        parameters.includePeerToPeer = true

        let browser = NWBrowser(
            for: .bonjour(type: "_screenstream._udp", domain: nil),
            using: parameters
        )
        self.browser = browser

        browser.stateUpdateHandler = { [weak self] state in
            switch state {
            case .ready:
                self?.emitStatus("Discovery ready")
            case .failed(let error):
                self?.emitStatus("Discovery failed: \(error.localizedDescription)")
            case .waiting(let error):
                self?.emitStatus("Discovery waiting: \(error.localizedDescription)")
            default:
                break
            }
        }

        browser.browseResultsChangedHandler = { [weak self] results, _ in
            let endpoints: [StreamEndpoint] = results.map {
                let name: String
                switch $0.endpoint {
                case .service(let svcName, _, _, _):
                    name = svcName
                default:
                    name = "\($0.endpoint)"
                }
                return StreamEndpoint(name: name, endpoint: $0.endpoint)
            }
            self?.emitStatus("Found \(endpoints.count) service(s)")
            self?.emitEndpoints(endpoints)
        }

        browser.start(queue: queue)
    }

    func stopBrowsing() {
        browser?.cancel()
        browser = nil
    }
}

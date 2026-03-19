import Foundation
import Network

struct StreamEndpoint: Identifiable {
    let id = UUID()
    let name: String
    let url: URL
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

        let parameters = NWParameters.tcp
        parameters.includePeerToPeer = false

        let browser = NWBrowser(
            for: .bonjour(type: "_screx._tcp", domain: nil),
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
            let endpoints: [StreamEndpoint] = results.compactMap {
                switch $0.endpoint {
                case .service(let svcName, _, let domain, _):
                    let trimmedDomain = domain.trimmingCharacters(in: CharacterSet(charactersIn: "."))
                    let host = trimmedDomain.isEmpty ? "\(svcName).local" : "\(svcName).\(trimmedDomain)"
                    guard let url = URL(string: "http://\(host):8080") else {
                        return nil
                    }
                    return StreamEndpoint(name: svcName, url: url)
                case .hostPort(let host, let port):
                    let hostString: String
                    switch host {
                    case .ipv4(let addr):
                        hostString = addr.debugDescription
                    case .ipv6(let addr):
                        hostString = "[\(addr.debugDescription)]"
                    case .name(let name, _):
                        hostString = name
                    @unknown default:
                        hostString = "\(host)"
                    }
                    guard let url = URL(string: "http://\(hostString):\(port.rawValue)") else {
                        return nil
                    }
                    return StreamEndpoint(name: hostString, url: url)
                default:
                    return nil
                }
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

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
    private var endpointsByKey: [String: StreamEndpoint] = [:]
    private var resolveConnections: [String: NWConnection] = [:]

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
            self?.reconcile(results.map(\.endpoint))
        }

        browser.start(queue: queue)
    }

    func stopBrowsing() {
        browser?.cancel()
        browser = nil
        resolveConnections.values.forEach { $0.cancel() }
        resolveConnections.removeAll()
        endpointsByKey.removeAll()
    }

    private func reconcile(_ endpoints: [NWEndpoint]) {
        let incomingKeys = Set(endpoints.map(endpointKey))

        // Remove stale entries.
        for (key, connection) in resolveConnections where !incomingKeys.contains(key) {
            connection.cancel()
            resolveConnections.removeValue(forKey: key)
        }
        for key in endpointsByKey.keys where !incomingKeys.contains(key) {
            endpointsByKey.removeValue(forKey: key)
        }

        // Resolve each endpoint once.
        for endpoint in endpoints {
            let key = endpointKey(endpoint)
            // Publish an immediate candidate endpoint so discovery can progress
            // even if async TCP resolution takes longer.
            if let immediate = makeEndpoint(from: endpoint, fallback: endpoint) {
                endpointsByKey[key] = immediate
            }
            if resolveConnections[key] != nil {
                continue
            }
            resolve(endpoint, key: key)
        }

        emitStatus("Found \(endpointsByKey.count) service(s)")
        emitEndpoints(Array(endpointsByKey.values).sorted { $0.name < $1.name })
    }

    private func resolve(_ endpoint: NWEndpoint, key: String) {
        let connection = NWConnection(to: endpoint, using: .tcp)
        resolveConnections[key] = connection

        connection.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            switch state {
            case .ready:
                let resolved = connection.currentPath?.remoteEndpoint ?? endpoint
                if let streamEndpoint = self.makeEndpoint(from: resolved, fallback: endpoint) {
                    self.endpointsByKey[key] = streamEndpoint
                    self.emitStatus("Found \(self.endpointsByKey.count) service(s)")
                    self.emitEndpoints(Array(self.endpointsByKey.values).sorted { $0.name < $1.name })
                }
                connection.cancel()
                self.resolveConnections.removeValue(forKey: key)
            case .failed(let error):
                self.emitStatus("Discovery resolve failed: \(error.localizedDescription)")
                connection.cancel()
                self.resolveConnections.removeValue(forKey: key)
            case .cancelled:
                self.resolveConnections.removeValue(forKey: key)
            default:
                break
            }
        }

        connection.start(queue: queue)
    }

    private func makeEndpoint(from endpoint: NWEndpoint, fallback: NWEndpoint) -> StreamEndpoint? {
        let target = endpointHostPort(endpoint) ?? endpointHostPort(fallback)
        guard let (host, port) = target else { return nil }

        let hostPart: String
        if host.contains(":") && !host.hasPrefix("[") {
            hostPart = "[\(host)]"
        } else {
            hostPart = host
        }

        guard let url = URL(string: "http://\(hostPart):\(port)") else { return nil }
        return StreamEndpoint(name: host, url: url)
    }

    private func endpointHostPort(_ endpoint: NWEndpoint) -> (String, UInt16)? {
        switch endpoint {
        case .hostPort(let host, let port):
            let hostString: String
            switch host {
            case .ipv4(let addr):
                hostString = addr.debugDescription
            case .ipv6(let addr):
                hostString = addr.debugDescription
            case .name(let name, _):
                hostString = name
            @unknown default:
                hostString = "\(host)"
            }
            return (hostString, port.rawValue)
        case .service(let name, _, _, _):
            // Last-resort fallback if hostPort resolution fails.
            return ("\(name).local", 8080)
        default:
            return nil
        }
    }

    private func endpointKey(_ endpoint: NWEndpoint) -> String {
        switch endpoint {
        case .hostPort(let host, let port):
            return "host:\(host):\(port.rawValue)"
        case .service(let name, let type, let domain, let iface):
            return "service:\(name).\(type).\(domain)@\(String(describing: iface))"
        default:
            return "\(endpoint)"
        }
    }
}

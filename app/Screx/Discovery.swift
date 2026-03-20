import Foundation
import Network

struct StreamEndpoint: Identifiable {
    let id = UUID()
    let name: String
    let host: String
    let port: UInt16
}

final class DiscoveryService {
    private var listener: NWListener?
    private let queue = DispatchQueue(label: "screx.discovery", qos: .userInteractive)

    var onStatusUpdate: ((String) -> Void)?
    var onEndpointFound: ((StreamEndpoint) -> Void)?

    private static let beaconPort: UInt16 = 9999
    private static let beaconMagic = Data("SCREX_BEACON".utf8)

    private var knownHost: String?

    func startListening() {
        guard listener == nil else { return }

        do {
            let params = NWParameters.udp
            params.allowLocalEndpointReuse = true
            params.requiredLocalEndpoint = NWEndpoint.hostPort(
                host: .ipv4(.any),
                port: NWEndpoint.Port(integerLiteral: Self.beaconPort)
            )

            let l = try NWListener(using: params)
            self.listener = l

            l.stateUpdateHandler = { [weak self] state in
                switch state {
                case .ready:
                    self?.emit("Listening for daemon beacon...")
                    print("[discovery] beacon listener ready on port \(Self.beaconPort)")
                case .failed(let error):
                    print("[discovery] listener failed: \(error)")
                    self?.emit("Discovery failed: \(error.localizedDescription)")
                default:
                    break
                }
            }

            l.newConnectionHandler = { [weak self] conn in
                self?.handleBeaconConnection(conn)
            }

            l.start(queue: queue)
        } catch {
            print("[discovery] failed to create beacon listener: \(error)")
            emit("Discovery error: \(error.localizedDescription)")
        }
    }

    func stopListening() {
        listener?.cancel()
        listener = nil
        knownHost = nil
    }

    private func handleBeaconConnection(_ conn: NWConnection) {
        conn.stateUpdateHandler = { state in
            if case .ready = state {
                conn.receiveMessage { [weak self] data, _, _, _ in
                    self?.processBeacon(data: data, from: conn)
                    conn.cancel()
                }
            }
        }
        conn.start(queue: queue)
    }

    private func processBeacon(data: Data?, from conn: NWConnection) {
        guard let data, data.count >= Self.beaconMagic.count + 2 else { return }
        guard data.prefix(Self.beaconMagic.count) == Self.beaconMagic else { return }

        let portOffset = Self.beaconMagic.count
        let streamPort = UInt16(data[portOffset]) << 8 | UInt16(data[portOffset + 1])

        // Extract sender IP from the connection's remote endpoint
        guard let remoteEndpoint = conn.currentPath?.remoteEndpoint else {
            print("[discovery] beacon received but no remote endpoint")
            return
        }

        let host: String
        switch remoteEndpoint {
        case .hostPort(let h, _):
            host = "\(h)"
        default:
            print("[discovery] unexpected endpoint type: \(remoteEndpoint)")
            return
        }

        // Only emit once per unique host (avoid spamming on every beacon)
        if knownHost != host {
            knownHost = host
            print("[discovery] daemon found at \(host):\(streamPort)")
            emit("Found daemon at \(host)")

            let endpoint = StreamEndpoint(name: host, host: host, port: streamPort)
            DispatchQueue.main.async { [weak self] in
                self?.onEndpointFound?(endpoint)
            }
        }
    }

    private func emit(_ text: String) {
        DispatchQueue.main.async { [weak self] in
            self?.onStatusUpdate?(text)
        }
    }
}

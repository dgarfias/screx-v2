import Foundation
import Network

final class StreamClient {
    private let endpoint: NWEndpoint
    private var connection: NWConnection?
    private let queue = DispatchQueue(label: "screx.stream", qos: .userInteractive)
    let decoder = H264Decoder()

    var onStatus: ((String) -> Void)?

    init(endpoint: NWEndpoint) {
        self.endpoint = endpoint
    }

    func connect() {
        let tcp = NWProtocolTCP.Options()
        tcp.noDelay = true
        let params = NWParameters(tls: nil, tcp: tcp)

        let conn = NWConnection(to: endpoint, using: params)
        self.connection = conn

        conn.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            switch state {
            case .ready:
                self.onStatus?("Connected, waiting for video...")
                self.readFrameLoop(conn)
            case .failed(let error):
                self.onStatus?("Connection failed: \(error.localizedDescription)")
            case .waiting(let error):
                self.onStatus?("Waiting: \(error.localizedDescription)")
            default:
                break
            }
        }

        conn.start(queue: queue)
    }

    func disconnect() {
        connection?.cancel()
        connection = nil
    }

    private func readFrameLoop(_ conn: NWConnection) {
        readLengthPrefix(conn)
    }

    private func readLengthPrefix(_ conn: NWConnection) {
        conn.receive(minimumIncompleteLength: 4, maximumLength: 4) { [weak self] data, _, isComplete, error in
            guard let self else { return }

            if isComplete || error != nil {
                self.onStatus?("Stream ended")
                return
            }

            guard let data, data.count == 4 else {
                self.onStatus?("Bad header, reconnecting...")
                return
            }

            let length = data.withUnsafeBytes { $0.load(as: UInt32.self).bigEndian }
            guard length > 0 && length < 10_000_000 else {
                self.onStatus?("Invalid frame size: \(length)")
                return
            }

            self.readPayload(conn, length: Int(length))
        }
    }

    private func readPayload(_ conn: NWConnection, length: Int) {
        conn.receive(minimumIncompleteLength: length, maximumLength: length) { [weak self] data, _, isComplete, error in
            guard let self else { return }

            if isComplete || error != nil {
                self.onStatus?("Stream ended")
                return
            }

            guard let data, data.count == length else {
                self.onStatus?("Incomplete frame (\(data?.count ?? 0)/\(length))")
                return
            }

            self.decoder.decodeAccessUnit(data)

            if !self.decoder.hasReportedFirstFrame {
                self.decoder.hasReportedFirstFrame = true
                self.onStatus?("Streaming")
            }

            self.readLengthPrefix(conn)
        }
    }
}

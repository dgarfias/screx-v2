import Foundation
import Network

struct TransportMetrics {
    let estimatedOneWayLatencyMs: Double
    let lossPercent: Double
    let jitterMs: Double
    let droppedPackets: UInt64
}

final class TransportService {
    private let queue = DispatchQueue(label: "screx.transport", qos: .userInteractive)
    private var streamConnection: NWConnection?
    private var controlConnection: NWConnection?
    private var streamListener: NWListener?
    private var selectedEndpoint: NWEndpoint?
    private var naluHandler: ((_ nalu: Data, _ timestamp90k: UInt32, _ isAccessUnitEnd: Bool) -> Void)?
    private var depacketizer = HevcDepacketizer()
    private var nextSequenceNumber: UInt16?
    private var jitterBuffer: [UInt16: RTPPacket] = [:]
    private var waitingForMissingSinceNs: UInt64?
    private let maxJitterBufferPackets = 192

    private var lastSequenceNumber: UInt16?
    private var expectedPackets: UInt64 = 0
    private var receivedPackets: UInt64 = 0
    private var droppedPackets: UInt64 = 0
    private var jitter90k: Double = 0
    private var lastTimestamp90k: UInt32?
    private var lastArrivalNs: UInt64?

    var onStatusUpdate: ((String) -> Void)?
    var onMetricsUpdate: ((TransportMetrics) -> Void)?

    private func emitStatus(_ text: String) {
        DispatchQueue.main.async { [weak self] in
            self?.onStatusUpdate?(text)
        }
    }

    private func emitMetrics(_ metrics: TransportMetrics) {
        DispatchQueue.main.async { [weak self] in
            self?.onMetricsUpdate?(metrics)
        }
    }

    func startListening(
        onNalu: @escaping (_ nalu: Data, _ timestamp90k: UInt32, _ isAccessUnitEnd: Bool) -> Void
    ) {
        naluHandler = onNalu
        if streamListener == nil {
            setupStreamListener()
        }
    }

    func connect(
        endpoint: NWEndpoint,
        onNalu: @escaping (_ nalu: Data, _ timestamp90k: UInt32, _ isAccessUnitEnd: Bool) -> Void
    ) {
        startListening(onNalu: onNalu)
        selectedEndpoint = endpoint
        setupControlConnection(to: endpoint)
    }

    func disconnect() {
        streamConnection?.cancel()
        streamConnection = nil
        controlConnection?.cancel()
        controlConnection = nil
        streamListener?.cancel()
        streamListener = nil
        selectedEndpoint = nil
        naluHandler = nil
        nextSequenceNumber = nil
        jitterBuffer.removeAll(keepingCapacity: true)
        waitingForMissingSinceNs = nil
        lastSequenceNumber = nil
        expectedPackets = 0
        receivedPackets = 0
        droppedPackets = 0
        jitter90k = 0
        lastTimestamp90k = nil
        lastArrivalNs = nil
    }

    func sendControl(message: String) {
        let payload = Data(message.utf8)
        if controlConnection == nil, let selectedEndpoint {
            setupControlConnection(to: selectedEndpoint)
        }
        controlConnection?.send(content: payload, completion: .contentProcessed { _ in })
    }

    private func receiveLoop() {
        streamConnection?.receiveMessage { [weak self] data, _, _, error in
            guard let self else { return }
            if let error {
                self.emitStatus("Receive error: \(error.localizedDescription)")
                return
            }

            guard let data, let packet = RTPPacket(data: data) else {
                self.receiveLoop()
                return
            }

            self.ingest(packet: packet)

            self.receiveLoop()
        }
    }

    private func setupControlConnection(to endpoint: NWEndpoint) {
        guard controlConnection == nil else { return }
        let connection = NWConnection(to: endpoint, using: .udp)
        connection.stateUpdateHandler = { [weak self] state in
            switch state {
            case .ready:
                self?.emitStatus("Control ready")
            case .waiting(let error):
                self?.emitStatus("Control waiting: \(error.localizedDescription)")
            case .failed(let error):
                self?.emitStatus("Control failed: \(error.localizedDescription)")
            case .cancelled:
                self?.emitStatus("Control disconnected")
            default:
                break
            }
        }
        connection.start(queue: queue)
        controlConnection = connection
    }

    private func setupStreamListener() {
        let parameters = NWParameters.udp
        parameters.includePeerToPeer = false

        do {
            let listener = try NWListener(
                using: parameters,
                on: NWEndpoint.Port(integerLiteral: 5004)
            )
            listener.stateUpdateHandler = { [weak self] state in
                switch state {
                case .ready:
                    self?.emitStatus("Listening on UDP 5004")
                case .waiting(let error):
                    self?.emitStatus("Listener waiting: \(error.localizedDescription)")
                case .failed(let error):
                    self?.emitStatus("Listener failed: \(error.localizedDescription)")
                case .cancelled:
                    self?.emitStatus("Listener stopped")
                default:
                    break
                }
            }
            listener.newConnectionHandler = { [weak self] connection in
                self?.attachStreamConnection(connection)
            }
            listener.start(queue: queue)
            streamListener = listener
        } catch {
            emitStatus("Listener setup failed: \(error.localizedDescription)")
        }
    }

    private func attachStreamConnection(_ connection: NWConnection) {
        // Keep the first active flow and reject duplicates to avoid listener churn.
        if streamConnection != nil {
            connection.cancel()
            return
        }
        streamConnection = connection
        connection.stateUpdateHandler = { [weak self] state in
            switch state {
            case .ready:
                self?.emitStatus("Stream connected")
            case .waiting(let error):
                self?.emitStatus("Stream waiting: \(error.localizedDescription)")
            case .failed(let error):
                self?.emitStatus("Stream failed: \(error.localizedDescription)")
            case .cancelled:
                self?.emitStatus("Stream disconnected")
            default:
                break
            }
            if case .failed = state, self?.streamConnection === connection {
                self?.streamConnection = nil
            }
            if case .cancelled = state, self?.streamConnection === connection {
                self?.streamConnection = nil
            }
        }
        connection.start(queue: queue)
        receiveLoop()
    }

    private func ingest(packet: RTPPacket) {
        updateMetrics(with: packet)

        guard let expected = nextSequenceNumber else {
            nextSequenceNumber = packet.sequenceNumber
            jitterBuffer[packet.sequenceNumber] = packet
            drainJitterBuffer()
            return
        }

        if isOlderSequence(packet.sequenceNumber, than: expected) {
            droppedPackets += 1
            publishMetricsFallback()
            return
        }

        jitterBuffer[packet.sequenceNumber] = packet
        trimJitterBufferIfNeeded()
        drainJitterBuffer()
    }

    private func process(packet: RTPPacket) {
        guard let naluHandler else { return }
        let nalus = depacketizer.push(payload: packet.payload, marker: packet.marker)
        for nalu in nalus {
            naluHandler(nalu.bytes, packet.timestamp, nalu.accessUnitEnd)
        }
    }

    private func updateMetrics(with packet: RTPPacket) {
        receivedPackets += 1
        if let last = lastSequenceNumber {
            let delta = UInt16(truncatingIfNeeded: packet.sequenceNumber &- last)
            expectedPackets += UInt64(max(delta, 1))
        } else {
            expectedPackets += 1
        }
        lastSequenceNumber = packet.sequenceNumber

        let loss = expectedPackets > 0
            ? max(0, (Double(expectedPackets - receivedPackets) / Double(expectedPackets)) * 100.0)
            : 0

        let nowNs = DispatchTime.now().uptimeNanoseconds
        if let lastArrivalNs, let lastTimestamp90k {
            let arrivalDelta90k = Double(nowNs - lastArrivalNs) * 90_000.0 / 1_000_000_000.0
            let tsDelta = Double(packet.timestamp &- lastTimestamp90k)
            let d = abs(arrivalDelta90k - tsDelta)
            jitter90k += (d - jitter90k) / 16.0
        }
        self.lastArrivalNs = nowNs
        self.lastTimestamp90k = packet.timestamp

        // Coarse estimate for "network + playout buffer" in this RTP-only transport.
        let jitterMs = (jitter90k / 90_000.0) * 1000.0
        let playoutDelayMs = Double(currentMissingTimeoutNs()) / 1_000_000.0
        let estimatedMs = max(0, jitterMs + playoutDelayMs)
        emitMetrics(
            TransportMetrics(
                estimatedOneWayLatencyMs: estimatedMs,
                lossPercent: loss,
                jitterMs: jitterMs,
                droppedPackets: droppedPackets
            )
        )
    }

    private func publishMetricsFallback() {
        let loss = expectedPackets > 0
            ? max(0, (Double(expectedPackets - receivedPackets) / Double(expectedPackets)) * 100.0)
            : 0
        let jitterMs = (jitter90k / 90_000.0) * 1000.0
        let playoutDelayMs = Double(currentMissingTimeoutNs()) / 1_000_000.0
        emitMetrics(
            TransportMetrics(
                estimatedOneWayLatencyMs: max(0, jitterMs + playoutDelayMs),
                lossPercent: loss,
                jitterMs: jitterMs,
                droppedPackets: droppedPackets
            )
        )
    }

    private func drainJitterBuffer() {
        guard var expected = nextSequenceNumber else { return }

        while true {
            if let packet = jitterBuffer.removeValue(forKey: expected) {
                process(packet: packet)
                expected = expected &+ 1
                nextSequenceNumber = expected
                waitingForMissingSinceNs = nil
                continue
            }

            guard !jitterBuffer.isEmpty else {
                nextSequenceNumber = expected
                return
            }

            let gap = smallestAheadGap(from: expected)
            let nowNs = DispatchTime.now().uptimeNanoseconds
            let timeoutNs = currentMissingTimeoutNs()
            let timedOut: Bool
            if let since = waitingForMissingSinceNs {
                timedOut = nowNs >= since ? (nowNs - since) >= timeoutNs : false
            } else {
                waitingForMissingSinceNs = nowNs
                timedOut = false
            }

            if gap >= currentReorderWindowPackets() || timedOut {
                droppedPackets += 1
                publishMetricsFallback()
                depacketizer.reset()
                expected = expected &+ 1
                nextSequenceNumber = expected
                waitingForMissingSinceNs = nil
                continue
            }
            nextSequenceNumber = expected
            return
        }
    }

    private func trimJitterBufferIfNeeded() {
        guard jitterBuffer.count > maxJitterBufferPackets, let expected = nextSequenceNumber else {
            return
        }

        var dropped = 0_u64
        while jitterBuffer.count > maxJitterBufferPackets {
            var dropKey: UInt16?
            var worstScore = UInt16.min
            for key in jitterBuffer.keys {
                let score = sequenceDistanceScore(from: expected, to: key)
                if dropKey == nil || score > worstScore {
                    worstScore = score
                    dropKey = key
                }
            }
            if let dropKey {
                jitterBuffer.removeValue(forKey: dropKey)
                dropped += 1
            } else {
                break
            }
        }
        if dropped > 0 {
            droppedPackets += dropped
            publishMetricsFallback()
        }
    }

    private func isOlderSequence(_ sequence: UInt16, than expected: UInt16) -> Bool {
        let delta = sequence &- expected
        return delta > 0x7FFF
    }

    private func smallestAheadGap(from expected: UInt16) -> UInt16 {
        var best = UInt16.max
        for key in jitterBuffer.keys {
            let delta = key &- expected
            if delta <= 0x7FFF, delta < best {
                best = delta
            }
        }
        return best
    }

    private func sequenceDistanceScore(from expected: UInt16, to sequence: UInt16) -> UInt16 {
        let delta = sequence &- expected
        // Older packets are always worse than future packets for live playout.
        return delta > 0x7FFF ? UInt16.max : delta
    }

    private func currentReorderWindowPackets() -> UInt16 {
        let jitterMs = (jitter90k / 90_000.0) * 1000.0
        let window = Int((6.0 + jitterMs * 0.6).rounded())
        return UInt16(min(48, max(6, window)))
    }

    private func currentMissingTimeoutNs() -> UInt64 {
        let jitterMs = (jitter90k / 90_000.0) * 1000.0
        let timeoutMs = min(90.0, max(8.0, 12.0 + jitterMs * 3.0))
        return UInt64(timeoutMs * 1_000_000.0)
    }

}

private struct AssembledNalu {
    let bytes: Data
    let accessUnitEnd: Bool
}

private final class HevcDepacketizer {
    private var fuBuffer = Data()

    func reset() {
        fuBuffer.removeAll(keepingCapacity: true)
    }

    func push(payload: Data, marker: Bool) -> [AssembledNalu] {
        guard payload.count >= 2 else { return [] }
        let nalType = (payload[0] >> 1) & 0x3F

        if nalType == 48 {
            return unpackAggregationPacket(payload: payload, marker: marker)
        }

        if nalType != 49 {
            return [AssembledNalu(bytes: payload, accessUnitEnd: marker)]
        }

        guard payload.count >= 4 else { return [] }
        let fuHeader = payload[2]
        let isStart = (fuHeader & 0x80) != 0
        let isEnd = (fuHeader & 0x40) != 0
        let originalType = fuHeader & 0x3F

        if isStart {
            fuBuffer.removeAll(keepingCapacity: true)
            let header0 = (payload[0] & 0x81) | (originalType << 1)
            let header1 = payload[1]
            fuBuffer.append(header0)
            fuBuffer.append(header1)
        } else if fuBuffer.isEmpty {
            // Lost FU start; discard orphan continuation fragment.
            return []
        }

        fuBuffer.append(contentsOf: payload[3...])

        if isEnd {
            let complete = fuBuffer
            fuBuffer.removeAll(keepingCapacity: true)
            return [AssembledNalu(bytes: complete, accessUnitEnd: marker)]
        }
        return []
    }

    private func unpackAggregationPacket(payload: Data, marker: Bool) -> [AssembledNalu] {
        // RFC 7798 AP payload:
        // [2-byte AP NAL header][2-byte NALU length][NALU]...
        guard payload.count >= 4 else { return [] }
        var offset = 2
        var nalus: [Data] = []
        while offset + 2 <= payload.count {
            let length = Int(UInt16(payload[offset]) << 8 | UInt16(payload[offset + 1]))
            offset += 2
            guard length > 0, offset + length <= payload.count else { break }
            nalus.append(payload.subdata(in: offset..<(offset + length)))
            offset += length
        }

        guard !nalus.isEmpty else { return [] }
        return nalus.enumerated().map { index, nalu in
            AssembledNalu(bytes: nalu, accessUnitEnd: marker && index == nalus.count - 1)
        }
    }
}

private struct RTPPacket {
    let marker: Bool
    let sequenceNumber: UInt16
    let timestamp: UInt32
    let payload: Data

    init?(data: Data) {
        guard data.count >= 12 else { return nil }
        let bytes = [UInt8](data)
        guard (bytes[0] >> 6) == 2 else { return nil }

        let csrcCount = Int(bytes[0] & 0x0F)
        let extensionBit = (bytes[0] & 0x10) != 0
        var offset = 12 + (csrcCount * 4)
        guard data.count >= offset else { return nil }

        if extensionBit {
            guard data.count >= offset + 4 else { return nil }
            let extLenWords = Int(UInt16(bytes[offset + 2]) << 8 | UInt16(bytes[offset + 3]))
            offset += 4 + (extLenWords * 4)
            guard data.count >= offset else { return nil }
        }

        marker = (bytes[1] & 0x80) != 0
        sequenceNumber = UInt16(bytes[2]) << 8 | UInt16(bytes[3])
        timestamp = UInt32(bytes[4]) << 24 | UInt32(bytes[5]) << 16 | UInt32(bytes[6]) << 8 | UInt32(bytes[7])
        payload = data.subdata(in: offset..<data.count)
    }
}

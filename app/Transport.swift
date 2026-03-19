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
    private var depacketizer = HevcDepacketizer()
    private var reorderBuffer: [UInt16: RTPPacket] = [:]
    private var nextSequenceNumber: UInt16?

    private var lastSequenceNumber: UInt16?
    private var expectedPackets: UInt64 = 0
    private var receivedPackets: UInt64 = 0
    private var droppedPackets: UInt64 = 0
    private var jitter90k: Double = 0
    private var lastTimestamp90k: UInt32?
    private var lastArrivalNs: UInt64?

    var onStatusUpdate: ((String) -> Void)?
    var onMetricsUpdate: ((TransportMetrics) -> Void)?

    func connect(
        endpoint: NWEndpoint,
        onNalu: @escaping (_ nalu: Data, _ timestamp90k: UInt32, _ isAccessUnitEnd: Bool) -> Void
    ) {
        let connection = NWConnection(to: endpoint, using: .udp)
        self.streamConnection = connection

        connection.stateUpdateHandler = { [weak self] state in
            switch state {
            case .ready:
                self?.onStatusUpdate?("Connected")
            case .waiting(let error):
                self?.onStatusUpdate?("Transport waiting: \(error.localizedDescription)")
            case .failed(let error):
                self?.onStatusUpdate?("Transport failed: \(error.localizedDescription)")
            case .cancelled:
                self?.onStatusUpdate?("Transport disconnected")
            default:
                break
            }
        }

        connection.start(queue: queue)
        receiveLoop(onNalu: onNalu)
    }

    func disconnect() {
        streamConnection?.cancel()
        streamConnection = nil
        reorderBuffer.removeAll(keepingCapacity: true)
        nextSequenceNumber = nil
    }

    func sendControl(message: String) {
        let payload = Data(message.utf8)
        // MVP control path uses the same advertised UDP endpoint as the stream.
        streamConnection?.send(content: payload, completion: .contentProcessed { _ in })
    }

    private func receiveLoop(
        onNalu: @escaping (_ nalu: Data, _ timestamp90k: UInt32, _ isAccessUnitEnd: Bool) -> Void
    ) {
        streamConnection?.receiveMessage { [weak self] data, _, _, error in
            guard let self else { return }
            if let error {
                self.onStatusUpdate?("Receive error: \(error.localizedDescription)")
                return
            }

            guard let data, let packet = RTPPacket(data: data) else {
                self.receiveLoop(onNalu: onNalu)
                return
            }

            self.ingest(packet: packet, onNalu: onNalu)

            self.receiveLoop(onNalu: onNalu)
        }
    }

    private func ingest(
        packet: RTPPacket,
        onNalu: @escaping (_ nalu: Data, _ timestamp90k: UInt32, _ isAccessUnitEnd: Bool) -> Void
    ) {
        updateMetrics(with: packet)

        guard let expected = nextSequenceNumber else {
            nextSequenceNumber = packet.sequenceNumber &+ 1
            process(packet: packet, onNalu: onNalu)
            return
        }

        let delta = packet.sequenceNumber &- expected
        if delta > 0x7FFF {
            // Old packet arrived too late, skip it.
            droppedPackets += 1
            publishMetricsFallback()
            return
        }

        reorderBuffer[packet.sequenceNumber] = packet
        if reorderBuffer.count > 64 {
            droppedPackets += UInt64(reorderBuffer.count)
            reorderBuffer.removeAll(keepingCapacity: true)
            nextSequenceNumber = packet.sequenceNumber &+ 1
            process(packet: packet, onNalu: onNalu)
            publishMetricsFallback()
            return
        }

        var cursor = expected
        while let next = reorderBuffer.removeValue(forKey: cursor) {
            process(packet: next, onNalu: onNalu)
            cursor = cursor &+ 1
            nextSequenceNumber = cursor
        }
    }

    private func process(
        packet: RTPPacket,
        onNalu: @escaping (_ nalu: Data, _ timestamp90k: UInt32, _ isAccessUnitEnd: Bool) -> Void
    ) {
        let nalus = depacketizer.push(payload: packet.payload, marker: packet.marker)
        for nalu in nalus {
            onNalu(nalu.bytes, packet.timestamp, nalu.accessUnitEnd)
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

        // This remains a coarse estimate without sender clock sync.
        let estimatedMs = max(0, (jitter90k / 90_000.0) * 1000.0 + (1000.0 / 60.0))
        let jitterMs = (jitter90k / 90_000.0) * 1000.0
        onMetricsUpdate?(
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
        onMetricsUpdate?(
            TransportMetrics(
                estimatedOneWayLatencyMs: max(0, jitterMs + (1000.0 / 60.0)),
                lossPercent: loss,
                jitterMs: jitterMs,
                droppedPackets: droppedPackets
            )
        )
    }
}

private struct AssembledNalu {
    let bytes: Data
    let accessUnitEnd: Bool
}

private final class HevcDepacketizer {
    private var fuBuffer = Data()

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

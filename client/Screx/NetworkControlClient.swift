import Foundation
import Network
import QuartzCore

/// Length-framed application control over QUIC. Input uses its own reliable
/// stream, so retransmission in control does not block pointer/keyboard events.
final class NetworkControlClient {
    private let session: QUICSession
    private var isClosed = false
    private var pingTimer: DispatchSourceTimer?
    private var pendingPingSentAt: TimeInterval?
    private var latencySamples: [Double] = []
    var onDisconnect: (() -> Void)?
    var onTraffic: ((Int, Int) -> Void)?
    var onCapabilities: ((DaemonCapabilities) -> Void)?
    var onDisplayStatus: ((DisplayStatus) -> Void)?
    var onLatency: ((Double, Double) -> Void)?

    init(session: QUICSession) { self.session = session }

    func start() {
        session.queue.async { [self] in
            session.onFailure = { [weak self] _ in self?.handleDisconnect() }
            receiveLoop()
            let timer = DispatchSource.makeTimerSource(queue: session.queue)
            timer.schedule(deadline: .now() + 1, repeating: 1)
            timer.setEventHandler { [weak self] in
                guard let self, !self.isClosed, self.pendingPingSentAt == nil else { return }
                self.pendingPingSentAt = CACurrentMediaTime()
                self.send(ScrexMessage.Client.ping)
            }
            timer.resume()
            pingTimer = timer
        }
    }
    func disconnect() {
        session.queue.async { [self] in
            isClosed = true; pingTimer?.cancel(); session.close()
        }
    }
    func disconnectGracefully() {
        session.queue.async { [self] in
            guard !isClosed, let control = session.control else { return }
            isClosed = true
            pingTimer?.cancel()
            control.send(content: QUICFrame.encode(Data([ScrexMessage.Client.disconnect])),
                completion: .contentProcessed { [session] _ in session.close() })
            session.queue.asyncAfter(deadline: .now() + 0.2) { [session] in session.close() }
        }
    }
    private func handleDisconnect() {
        guard !isClosed else { return }
        isClosed = true; pingTimer?.cancel()
        onDisconnect?()
    }
    private func receiveLoop() {
        guard !isClosed, let control = session.control else { return }
        QUICFrame.read(control) { [weak self] result in
            guard let self, !self.isClosed else { return }
            switch result {
            case .failure:
                self.session.close()
                self.handleDisconnect()
            case .success(let payload):
                self.onTraffic?(payload.count + 4, 0)
                self.handle(payload)
                self.receiveLoop()
            }
        }
    }
    private func handle(_ payload: Data) {
        guard let type = payload.first else { return }
        let body = Data(payload.dropFirst())
        switch type {
        case ScrexMessage.Daemon.pong:
            guard let sent = pendingPingSentAt else { return }
            pendingPingSentAt = nil
            let rtt = (CACurrentMediaTime() - sent) * 1000
            latencySamples.append(rtt)
            if latencySamples.count > 8 { latencySamples.removeFirst() }
            let variation = zip(latencySamples, latencySamples.dropFirst()).reduce(0.0) { $0 + abs($1.1 - $1.0) }
            onLatency?(rtt, variation / Double(max(1, latencySamples.count - 1)))
        case ScrexMessage.Daemon.caps:
            if let caps = DaemonCapabilities.parse(body) { onCapabilities?(caps) }
        case ScrexMessage.Daemon.displayStatus:
            if let status = DisplayStatus.parse(body) { onDisplayStatus?(status) }
        default: break
        }
    }
    private func send(_ type: UInt8, _ body: Data = Data()) {
        let payload = Data([type]) + body
        if (0x10...0x15).contains(type) {
            session.sendInput(payload)
            return
        }
        session.queue.async { [weak self] in
            guard let self, !self.isClosed, let control = self.session.control else { return }
            control.send(content: QUICFrame.encode(payload), completion: .contentProcessed { [weak self] error in
                if error != nil { self?.session.close(); self?.handleDisconnect() }
                else { self?.onTraffic?(payload.count + 4, 0) }
            })
        }
    }
    func sendPli(display: StreamDisplayID, generation: UInt32) {
        var body = Data([display.rawValue])
        withUnsafeBytes(of: generation.bigEndian) { body.append(contentsOf: $0) }
        send(ScrexMessage.Client.pli, body)
    }
    func sendDisplayPresented(revision: UInt32, display: StreamDisplayID, generation: UInt32) {
        var body = withUnsafeBytes(of: revision.bigEndian) { Data($0) }
        body.append(display.rawValue)
        withUnsafeBytes(of: generation.bigEndian) { body.append(contentsOf: $0) }
        send(ScrexMessage.Client.displayPresented, body)
    }
    func sendSpeakerState(isEnabled: Bool) { send(ScrexMessage.Client.speaker, Data([isEnabled ? 1 : 0])) }
    func sendMicState(isEnabled: Bool) { send(ScrexMessage.Client.mic, Data([isEnabled ? 1 : 0])) }
    func sendCameraConfig(width: UInt16, height: UInt16, fps: UInt16) {
        var body = Data()
        for number in [width, height, fps] { withUnsafeBytes(of: number.bigEndian) { body.append(contentsOf: $0) } }
        send(ScrexMessage.Client.camera, body)
    }
    func sendTouch(_ body: Data) { send(ScrexMessage.Client.touch, body) }
    func sendKey(_ body: Data) { send(ScrexMessage.Client.key, body) }
    func sendMouse(_ body: Data) { send(ScrexMessage.Client.mouse, body) }
    func sendRawKey(_ body: Data) { send(ScrexMessage.Client.rawKey, body) }
    func sendPeripheral(_ body: Data) { send(ScrexMessage.Client.peripheral, body) }
    func sendGamepad(_ body: Data) { send(ScrexMessage.Client.gamepad, body) }
    func sendStreamSettings(_ settings: StreamSettings) { send(ScrexMessage.Client.settings, settings.encode()) }
}

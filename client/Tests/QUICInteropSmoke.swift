import Foundation
import Network
import Darwin

// Compile with CPace.swift, QUICSession.swift, PairingService.swift and the
// shared C primitive. See docs/QUIC.md. Uses volatile test storage, never the
// production Keychain. Run against the ignored Rust apple_interop_server test.

@main
struct AppleQUICSmoke {
    static func main() {
        let pairing = PairingService(store: MemoryStore())
        let tile = "quic-smoke-" + UUID().uuidString
        var retained: QUICSession?
        var controlOK = false
        var inputOK = false
        var mediaOK = false
        func fail(_ message: String) {
            print("SMOKE_FAILED \(message)")
            fflush(stdout)
            exit(1)
        }
        func finishIfReady() {
            guard controlOK && inputOK && mediaOK, let session = retained, let control = session.control else { return }
            control.send(content: QUICFrame.encode(Data("done".utf8)), completion: .contentProcessed { error in
                if let error { fail(error.localizedDescription); return }
                print("SMOKE_PASSED: PAKE, TLS binding, control, input and 1022-byte datagrams")
                fflush(stdout)
                exit(0)
            })
        }
        pairing.onResult = { result in
            switch result {
            case .pinRequired(let submit):
                print("SMOKE_PIN_REQUIRED")
                fflush(stdout)
                guard let pin = readLine() else { fail("PIN input ended"); return }
                submit(pin)
            case .sessionEstablished(let session):
                retained = session
                session.queue.async {
                    session.onFailure = fail
                    session.setDatagramHandler { data in
                        guard data == Data(repeating: 0x5a, count: 1022) else { fail("wrong media payload"); return }
                        mediaOK = true
                        finishIfReady()
                    }
                    guard let control = session.control, let input = session.input else { fail("missing streams"); return }
                    QUICFrame.read(control) { result in
                        guard (try? result.get()) == Data("pong".utf8) else { fail("control reply"); return }
                        controlOK = true; finishIfReady()
                    }
                    QUICFrame.read(input) { result in
                        guard (try? result.get()) == Data("input-ok".utf8) else { fail("input reply"); return }
                        inputOK = true; finishIfReady()
                    }
                    control.send(content: QUICFrame.encode(Data("ping".utf8)), completion: .contentProcessed { error in
                        if let error { fail(error.localizedDescription) }
                    })
                    session.sendInput(Data("input".utf8))
                    session.sendDatagram(Data(repeating: 0x5a, count: 1022))
                }
            case .error(let message): fail(message)
            case .rejected(let reason): fail(reason)
            }
        }
        pairing.pair(host: CommandLine.arguments.dropFirst().first ?? "127.0.0.1", port: 19001, tileId: tile,
                     deviceId: Data(repeating: 0x46, count: 16), clientName: "Mac QUIC smoke")
        DispatchQueue.main.asyncAfter(deadline: .now() + 60) { fail("test timed out") }
        withExtendedLifetime(pairing) { RunLoop.main.run() }
    }
}

final class MemoryStore: PairingStore {
    private var records: [String: KeychainHelper.Record] = [:]
    func load(tileId: String) throws -> KeychainHelper.Record? { records[tileId] }
    func save(_ record: KeychainHelper.Record, tileId: String) throws { records[tileId] = record }
}

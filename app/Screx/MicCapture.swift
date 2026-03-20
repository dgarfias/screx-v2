import AVFoundation

final class MicCapture {
    private let engine = AVAudioEngine()
    private var running = false

    var onPCM: ((Data) -> Void)?

    func start() {
        guard !running else { return }

        let inputNode = engine.inputNode
        let hwFormat = inputNode.outputFormat(forBus: 0)
        let hwChannels = Int(hwFormat.channelCount)

        print("[mic] hardware format: \(hwFormat.sampleRate)Hz, \(hwChannels)ch, \(hwFormat.commonFormat.rawValue)")

        inputNode.installTap(onBus: 0, bufferSize: 1024, format: hwFormat) { [weak self] buffer, _ in
            guard let self, let onPCM = self.onPCM else { return }
            guard let floatData = buffer.floatChannelData else { return }

            let frameCount = Int(buffer.frameLength)
            guard frameCount > 0 else { return }

            var pcm = Data(count: frameCount * 2)
            pcm.withUnsafeMutableBytes { raw in
                let out = raw.bindMemory(to: Int16.self)
                for i in 0..<frameCount {
                    var sample: Float = floatData[0][i]
                    if hwChannels > 1 {
                        sample = (sample + floatData[1][i]) * 0.5
                    }
                    let clamped = max(-1.0, min(1.0, sample))
                    out[i] = Int16(clamped * 32767.0)
                }
            }
            onPCM(pcm)
        }

        do {
            try engine.start()
            running = true
            print("[mic] capture started")
        } catch {
            print("[mic] engine start failed: \(error)")
        }
    }

    func stop() {
        guard running else { return }
        engine.inputNode.removeTap(onBus: 0)
        engine.stop()
        running = false
        print("[mic] capture stopped")
    }

    var isRunning: Bool { running }
}

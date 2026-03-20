import AVFoundation

final class MicCapture {
    private let engine = AVAudioEngine()
    private var running = false

    var onPCM: ((Data) -> Void)?

    func start() {
        guard !running else { return }

        let inputNode = engine.inputNode

        // Request s16le mono 48kHz directly — AVAudioEngine handles conversion
        guard let tapFormat = AVAudioFormat(
            commonFormat: .pcmFormatInt16,
            sampleRate: 48000,
            channels: 1,
            interleaved: true
        ) else {
            print("[mic] failed to create tap format")
            return
        }

        inputNode.installTap(onBus: 0, bufferSize: 960, format: tapFormat) { [weak self] buffer, _ in
            guard let self, let onPCM = self.onPCM else { return }

            let frameCount = Int(buffer.frameLength)
            guard frameCount > 0, let int16Ptr = buffer.int16ChannelData else { return }

            let byteCount = frameCount * 2
            let data = Data(bytes: int16Ptr[0], count: byteCount)
            onPCM(data)
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

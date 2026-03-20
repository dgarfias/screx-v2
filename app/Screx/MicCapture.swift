import AVFoundation

final class MicCapture {
    private let engine = AVAudioEngine()
    private var running = false

    var onPCM: ((Data) -> Void)?

    func start() {
        guard !running else { return }

        let session = AVAudioSession.sharedInstance()
        do {
            try session.setCategory(.playAndRecord, mode: .default, options: [.mixWithOthers, .defaultToSpeaker])
            try session.setActive(true)
        } catch {
            print("[mic] audio session setup failed: \(error)")
            return
        }

        let inputNode = engine.inputNode
        let hwFormat = inputNode.outputFormat(forBus: 0)

        guard let targetFormat = AVAudioFormat(
            commonFormat: .pcmFormatInt16,
            sampleRate: 48000,
            channels: 1,
            interleaved: true
        ) else {
            print("[mic] failed to create target format")
            return
        }

        guard let converter = AVAudioConverter(from: hwFormat, to: targetFormat) else {
            print("[mic] failed to create audio converter")
            return
        }

        let bufferSize: AVAudioFrameCount = 480 // 10ms at 48kHz

        inputNode.installTap(onBus: 0, bufferSize: 1024, format: hwFormat) { [weak self] buffer, _ in
            guard let self, let onPCM = self.onPCM else { return }

            guard let outputBuffer = AVAudioPCMBuffer(pcmFormat: targetFormat, frameCapacity: bufferSize) else { return }

            var error: NSError?
            let status = converter.convert(to: outputBuffer, error: &error) { _, outStatus in
                outStatus.pointee = .haveData
                return buffer
            }

            guard status == .haveData, error == nil else { return }

            let frameCount = Int(outputBuffer.frameLength)
            guard frameCount > 0, let int16Ptr = outputBuffer.int16ChannelData else { return }

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

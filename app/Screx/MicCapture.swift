import AVFoundation

final class MicCapture {
    private let engine = AVAudioEngine()
    private let mixer = AVAudioMixerNode()
    private var running = false

    var onPCM: ((Data) -> Void)?

    func start() {
        guard !running else { return }

        let inputNode = engine.inputNode
        let hwFormat = inputNode.inputFormat(forBus: 0)

        guard let targetFormat = AVAudioFormat(
            commonFormat: .pcmFormatInt16,
            sampleRate: 48000,
            channels: 1,
            interleaved: true
        ) else {
            print("[mic] failed to create target format")
            return
        }

        print("[mic] hw: \(hwFormat.sampleRate)Hz \(hwFormat.channelCount)ch -> target: 48000Hz 1ch s16le")

        engine.attach(mixer)
        engine.connect(inputNode, to: mixer, format: hwFormat)
        engine.connect(mixer, to: engine.mainMixerNode, format: targetFormat)

        mixer.installTap(onBus: 0, bufferSize: 960, format: targetFormat) { [weak self] buffer, _ in
            guard let self, let onPCM = self.onPCM else { return }

            let frameCount = Int(buffer.frameLength)
            guard frameCount > 0, let int16Ptr = buffer.int16ChannelData else { return }

            let data = Data(bytes: int16Ptr[0], count: frameCount * 2)
            onPCM(data)
        }

        do {
            try engine.start()
            running = true
            print("[mic] capture started (s16le 48kHz mono)")
        } catch {
            print("[mic] engine start failed: \(error)")
        }
    }

    func stop() {
        guard running else { return }
        mixer.removeTap(onBus: 0)
        engine.stop()
        engine.disconnectNodeInput(mixer)
        engine.detach(mixer)
        running = false
        print("[mic] capture stopped")
    }

    var isRunning: Bool { running }
}

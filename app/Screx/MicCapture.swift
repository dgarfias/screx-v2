import AVFoundation
import Opus

final class MicCapture {
    private let captureEngine = AVAudioEngine()
    private var opusEncoder: Opus.Encoder?
    private let opusSampleRate: Double = 48000
    private let opusFrameSize: Int = 960 // 20ms at 48kHz

    private var sampleAccumulator: [Float] = []
    private let accumulatorLock = NSLock()

    var onOpusPacket: ((Data) -> Void)?

    private(set) var isRunning = false

    func start() {
        guard !isRunning else { return }

        do {
            let inputNode = captureEngine.inputNode
            let hwFormat = inputNode.outputFormat(forBus: 0)

            print("[mic] hardware format: rate=\(hwFormat.sampleRate) channels=\(hwFormat.channelCount) format=\(hwFormat.commonFormat.rawValue)")

            let opusFormat = AVAudioFormat(
                commonFormat: .pcmFormatFloat32,
                sampleRate: opusSampleRate,
                channels: 1,
                interleaved: false
            )!

            opusEncoder = try Opus.Encoder(format: opusFormat, application: .voip)

            // Tap format: mono Float32 at hardware sample rate
            // (iOS requires tap sample rate == hardware sample rate)
            let tapFormat = AVAudioFormat(
                commonFormat: .pcmFormatFloat32,
                sampleRate: hwFormat.sampleRate,
                channels: 1,
                interleaved: false
            )!

            let needsResample = hwFormat.sampleRate != opusSampleRate
            var converter: AVAudioConverter?
            if needsResample {
                converter = AVAudioConverter(from: tapFormat, to: opusFormat)
                print("[mic] will resample \(hwFormat.sampleRate) -> \(opusSampleRate)")
            }

            inputNode.installTap(onBus: 0, bufferSize: AVAudioFrameCount(opusFrameSize), format: tapFormat) { [weak self] buffer, _ in
                self?.processTapBuffer(buffer, converter: converter)
            }

            captureEngine.prepare()
            try captureEngine.start()
            isRunning = true
            print("[mic] capture started")
        } catch {
            print("[mic] failed to start: \(error)")
        }
    }

    func stop() {
        guard isRunning else { return }
        captureEngine.inputNode.removeTap(onBus: 0)
        captureEngine.stop()
        isRunning = false
        accumulatorLock.lock()
        sampleAccumulator.removeAll()
        accumulatorLock.unlock()
        opusEncoder = nil
        print("[mic] capture stopped")
    }

    private func processTapBuffer(_ buffer: AVAudioPCMBuffer, converter: AVAudioConverter?) {
        let samples: [Float]

        if let converter {
            let ratio = opusSampleRate / buffer.format.sampleRate
            let outputFrames = AVAudioFrameCount(Double(buffer.frameLength) * ratio) + 1
            guard let convertedBuffer = AVAudioPCMBuffer(pcmFormat: converter.outputFormat, frameCapacity: outputFrames) else { return }

            var inputConsumed = false
            var error: NSError?
            _ = converter.convert(to: convertedBuffer, error: &error) { _, outStatus in
                if inputConsumed {
                    outStatus.pointee = .noDataNow
                    return nil
                }
                inputConsumed = true
                outStatus.pointee = .haveData
                return buffer
            }

            if let error {
                print("[mic] resample error: \(error)")
                return
            }

            guard convertedBuffer.frameLength > 0, let channelData = convertedBuffer.floatChannelData else { return }
            samples = Array(UnsafeBufferPointer(start: channelData[0], count: Int(convertedBuffer.frameLength)))
        } else {
            guard let channelData = buffer.floatChannelData else { return }
            samples = Array(UnsafeBufferPointer(start: channelData[0], count: Int(buffer.frameLength)))
        }

        accumulatorLock.lock()
        sampleAccumulator.append(contentsOf: samples)

        while sampleAccumulator.count >= opusFrameSize {
            let frame = Array(sampleAccumulator.prefix(opusFrameSize))
            sampleAccumulator.removeFirst(opusFrameSize)
            accumulatorLock.unlock()

            encodeAndSend(frame)

            accumulatorLock.lock()
        }
        accumulatorLock.unlock()
    }

    private func encodeAndSend(_ samples: [Float]) {
        guard let encoder = opusEncoder else { return }

        let format = AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: opusSampleRate,
            channels: 1,
            interleaved: false
        )!

        guard let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: AVAudioFrameCount(opusFrameSize)) else { return }
        buffer.frameLength = AVAudioFrameCount(opusFrameSize)

        let channelData = buffer.floatChannelData![0]
        for i in 0..<opusFrameSize {
            channelData[i] = samples[i]
        }

        var encodedData = Data(count: 4000)
        do {
            let _ = try encoder.encode(buffer, to: &encodedData)
            onOpusPacket?(encodedData)
        } catch {
            print("[mic] opus encode error: \(error)")
        }
    }
}

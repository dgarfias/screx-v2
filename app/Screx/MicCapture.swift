import AVFoundation

final class MicCapture {
    private let engine = AVAudioEngine()
    private var converter: AVAudioConverter?
    private var running = false

    // Frame accumulation buffer to ensure fixed-size 10ms packets.
    private var pending = Data()
    private var pendingOffset = 0

    var onPCMFrame: ((Data) -> Void)?

    func start() {
        guard !running else { return }

        let inputNode = engine.inputNode
        let inputFormat = inputNode.outputFormat(forBus: 0)

        guard let targetFormat = AVAudioFormat(
            commonFormat: .pcmFormatInt16,
            sampleRate: Double(MicProtocol.sampleRate),
            channels: AVAudioChannelCount(MicProtocol.channels),
            interleaved: false
        ) else {
            print("[mic] failed to create target format")
            return
        }

        guard let converter = AVAudioConverter(from: inputFormat, to: targetFormat) else {
            print("[mic] failed to create AVAudioConverter")
            return
        }

        self.converter = converter
        pending.removeAll(keepingCapacity: true)
        pendingOffset = 0

        print(
            "[mic] capture format in=\(inputFormat.sampleRate)Hz/\(inputFormat.channelCount)ch " +
            "out=\(MicProtocol.sampleRate)Hz/\(MicProtocol.channels)ch s16le " +
            "frame=\(MicProtocol.samplesPerFrame)"
        )

        inputNode.installTap(onBus: 0, bufferSize: 1024, format: inputFormat) { [weak self] buffer, _ in
            self?.handleInput(buffer)
        }

        do {
            try engine.start()
            running = true
            print("[mic] capture started")
        } catch {
            inputNode.removeTap(onBus: 0)
            self.converter = nil
            print("[mic] engine start failed: \(error)")
        }
    }

    func stop() {
        guard running else { return }
        engine.inputNode.removeTap(onBus: 0)
        engine.stop()
        converter = nil
        running = false
        pending.removeAll(keepingCapacity: false)
        pendingOffset = 0
        print("[mic] capture stopped")
    }

    var isRunning: Bool { running }

    private func handleInput(_ inputBuffer: AVAudioPCMBuffer) {
        guard let converter, let onPCMFrame else { return }

        let outputCapacity = AVAudioFrameCount(
            Double(inputBuffer.frameLength) * Double(MicProtocol.sampleRate) / inputBuffer.format.sampleRate + 64.0
        )
        guard outputCapacity > 0 else { return }
        guard let outBuffer = AVAudioPCMBuffer(
            pcmFormat: converter.outputFormat,
            frameCapacity: outputCapacity
        ) else {
            return
        }

        var didProvideInput = false
        var convertError: NSError?
        let status = converter.convert(to: outBuffer, error: &convertError) { _, outStatus in
            if didProvideInput {
                outStatus.pointee = .noDataNow
                return nil
            }
            didProvideInput = true
            outStatus.pointee = .haveData
            return inputBuffer
        }

        if let convertError {
            print("[mic] converter error: \(convertError)")
            return
        }
        if status != .haveData && status != .inputRanDry {
            return
        }

        let frameCount = Int(outBuffer.frameLength)
        guard frameCount > 0, let int16Data = outBuffer.int16ChannelData else { return }

        let bytes = Data(bytes: int16Data[0], count: frameCount * 2)
        pending.append(bytes)

        let frameBytes = MicProtocol.bytesPerFrame
        while (pending.count - pendingOffset) >= frameBytes {
            let frame = pending.subdata(in: pendingOffset..<(pendingOffset + frameBytes))
            pendingOffset += frameBytes
            onPCMFrame(frame)
        }

        // Compact occasionally to avoid unbounded growth.
        if pendingOffset > 0 && (pendingOffset > 8192 || pendingOffset == pending.count) {
            pending.removeSubrange(0..<pendingOffset)
            pendingOffset = 0
        }
    }
}

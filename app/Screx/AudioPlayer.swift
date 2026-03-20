import AVFoundation

final class AudioPlayer {
    private let engine = AVAudioEngine()
    private let sourceNode: AVAudioSourceNode
    private let sampleRate: Double = 48000
    private let channelCount: UInt32 = 2

    private let lock = NSLock()
    private var ringBuffer = Data()
    private static let maxBufferSize = 48000 * 2 * 2 // ~500ms at 48kHz stereo i16

    init() {
        let format = AVAudioFormat(
            commonFormat: .pcmFormatInt16,
            sampleRate: 48000,
            channels: 2,
            interleaved: true
        )!

        sourceNode = AVAudioSourceNode(format: format) { [weak self] _, _, frameCount, bufferList -> OSStatus in
            guard let self else { return noErr }

            let ablPointer = UnsafeMutableAudioBufferListPointer(bufferList)
            let bytesNeeded = Int(frameCount) * 2 * 2 // frameCount * channels * sizeof(Int16)

            self.lock.lock()
            let available = self.ringBuffer.count

            if available >= bytesNeeded {
                let chunk = self.ringBuffer.prefix(bytesNeeded)
                self.ringBuffer.removeFirst(bytesNeeded)
                self.lock.unlock()

                for buffer in ablPointer {
                    guard let dest = buffer.mData else { continue }
                    chunk.copyBytes(to: dest.assumingMemoryBound(to: UInt8.self), count: bytesNeeded)
                    buffer.mDataByteSize = UInt32(bytesNeeded)
                }
            } else {
                self.lock.unlock()
                for buffer in ablPointer {
                    guard let dest = buffer.mData else { continue }
                    memset(dest, 0, bytesNeeded)
                    buffer.mDataByteSize = UInt32(bytesNeeded)
                }
            }

            return noErr
        }

        engine.attach(sourceNode)
        engine.connect(sourceNode, to: engine.mainMixerNode, format: format)
    }

    func start() {
        do {
            try engine.start()
            print("[audio] playback engine started")
        } catch {
            print("[audio] engine start failed: \(error)")
        }
    }

    func stop() {
        engine.stop()
        lock.lock()
        ringBuffer.removeAll()
        lock.unlock()
        print("[audio] playback engine stopped")
    }

    func enqueueAudio(_ data: Data) {
        lock.lock()
        ringBuffer.append(data)
        if ringBuffer.count > Self.maxBufferSize {
            let excess = ringBuffer.count - Self.maxBufferSize
            ringBuffer.removeFirst(excess)
        }
        lock.unlock()
    }
}

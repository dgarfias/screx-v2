import Foundation
import AVFoundation
import VideoToolbox
import CoreMedia

final class H264Decoder {
    let displayLayer = AVSampleBufferDisplayLayer()
    var hasReportedFirstFrame = false

    private var formatDescription: CMVideoFormatDescription?
    private var sps: Data?
    private var pps: Data?
    private var naluCount = 0

    private let bufferLock = NSLock()
    private var latestSampleBuffer: CMSampleBuffer?
    private var displayLink: CADisplayLink?

    private var framesReceived: UInt64 = 0
    private var framesDisplayed: UInt64 = 0
    private var framesDropped: UInt64 = 0
    private var statsWindowStart = CACurrentMediaTime()

    init() {
        displayLayer.videoGravity = .resizeAspect
        startDisplayLink()
    }

    deinit {
        displayLink?.invalidate()
    }

    private func startDisplayLink() {
        let link = CADisplayLink(target: self, selector: #selector(displayLinkFired))
        link.add(to: .main, forMode: .common)
        displayLink = link
    }

    @objc private func displayLinkFired() {
        bufferLock.lock()
        let sb = latestSampleBuffer
        latestSampleBuffer = nil
        bufferLock.unlock()

        guard let sb else { return }

        if displayLayer.status == .failed {
            let err = displayLayer.error
            print("[decoder] display layer FAILED: \(err?.localizedDescription ?? "unknown"), flushing")
            displayLayer.flush()
        }
        displayLayer.enqueue(sb)
        framesDisplayed += 1

        let now = CACurrentMediaTime()
        let elapsed = now - statsWindowStart
        if elapsed >= 2.0 {
            let recvFps = Double(framesReceived) / elapsed
            let dispFps = Double(framesDisplayed) / elapsed
            let dropCount = framesDropped
            print("[decoder] recv_fps=\(String(format: "%.1f", recvFps)) display_fps=\(String(format: "%.1f", dispFps)) dropped=\(dropCount)")
            framesReceived = 0
            framesDisplayed = 0
            framesDropped = 0
            statsWindowStart = now
        }
    }

    func decodeAccessUnit(_ data: Data) {
        let nalus = splitAnnexBNalus(data)

        for nalu in nalus {
            guard nalu.count > 0 else { continue }
            let naluType = nalu[0] & 0x1F
            naluCount += 1

            if naluCount <= 5 {
                print("[decoder] NALU #\(naluCount) type=\(naluType) len=\(nalu.count)")
            }

            switch naluType {
            case 7:
                sps = nalu
                tryBuildFormatDescription()
            case 8:
                pps = nalu
                tryBuildFormatDescription()
            case 1, 5:
                if formatDescription != nil {
                    enqueueNalu(nalu)
                } else if naluCount <= 10 {
                    print("[decoder] dropping slice type=\(naluType), no format description yet")
                }
            default:
                break
            }
        }
    }

    private func tryBuildFormatDescription() {
        guard let sps, let pps else { return }

        let spsBytes = [UInt8](sps)
        let ppsBytes = [UInt8](pps)

        var newFmt: CMVideoFormatDescription?
        let status = spsBytes.withUnsafeBufferPointer { spsBuf in
            ppsBytes.withUnsafeBufferPointer { ppsBuf in
                var paramPointers: [UnsafePointer<UInt8>] = [spsBuf.baseAddress!, ppsBuf.baseAddress!]
                var paramSizes: [Int] = [spsBytes.count, ppsBytes.count]
                return CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    allocator: kCFAllocatorDefault,
                    parameterSetCount: 2,
                    parameterSetPointers: &paramPointers,
                    parameterSetSizes: &paramSizes,
                    nalUnitHeaderLength: 4,
                    formatDescriptionOut: &newFmt
                )
            }
        }

        if status == noErr, let newFmt {
            formatDescription = newFmt
            let dims = CMVideoFormatDescriptionGetDimensions(newFmt)
            print("[decoder] format description created: \(dims.width)x\(dims.height)")
        } else {
            print("[decoder] CMVideoFormatDescriptionCreateFromH264ParameterSets failed: \(status)")
        }
    }

    private func enqueueNalu(_ nalu: Data) {
        guard let formatDescription else { return }

        var naluWithLength = Data(count: 4 + nalu.count)
        let length = UInt32(nalu.count).bigEndian
        naluWithLength.replaceSubrange(0..<4, with: withUnsafeBytes(of: length) { Data($0) })
        naluWithLength.replaceSubrange(4..<(4 + nalu.count), with: nalu)

        let frameData = naluWithLength
        let dataLength = frameData.count

        var blockBuffer: CMBlockBuffer?
        frameData.withUnsafeBytes { rawBuf in
            let ptr = rawBuf.baseAddress!.assumingMemoryBound(to: UInt8.self)
            CMBlockBufferCreateWithMemoryBlock(
                allocator: kCFAllocatorDefault,
                memoryBlock: nil,
                blockLength: dataLength,
                blockAllocator: kCFAllocatorDefault,
                customBlockSource: nil,
                offsetToData: 0,
                dataLength: dataLength,
                flags: 0,
                blockBufferOut: &blockBuffer
            )
            if let bb = blockBuffer {
                CMBlockBufferReplaceDataBytes(
                    with: ptr,
                    blockBuffer: bb,
                    offsetIntoDestination: 0,
                    dataLength: dataLength
                )
            }
        }

        guard let blockBuffer else { return }

        var sampleBuffer: CMSampleBuffer?
        var sampleSize = dataLength
        var timing = CMSampleTimingInfo(
            duration: .invalid,
            presentationTimeStamp: CMClockGetTime(CMClockGetHostTimeClock()),
            decodeTimeStamp: .invalid
        )

        CMSampleBufferCreateReady(
            allocator: kCFAllocatorDefault,
            dataBuffer: blockBuffer,
            formatDescription: formatDescription,
            sampleCount: 1,
            sampleTimingEntryCount: 1,
            sampleTimingArray: &timing,
            sampleSizeEntryCount: 1,
            sampleSizeArray: &sampleSize,
            sampleBufferOut: &sampleBuffer
        )

        guard let sampleBuffer else { return }

        if let attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, createIfNecessary: true) as? [NSMutableDictionary],
           let dict = attachments.first {
            dict[kCMSampleAttachmentKey_DisplayImmediately] = true
        }

        framesReceived += 1

        bufferLock.lock()
        let hadPrevious = latestSampleBuffer != nil
        latestSampleBuffer = sampleBuffer
        bufferLock.unlock()

        if hadPrevious {
            framesDropped += 1
        }
    }

    private func splitAnnexBNalus(_ data: Data) -> [Data] {
        var nalus: [Data] = []
        var i = 0
        let bytes = [UInt8](data)
        let count = bytes.count

        while i < count {
            var startCodeLen = 0
            if i + 3 < count && bytes[i] == 0 && bytes[i+1] == 0 && bytes[i+2] == 0 && bytes[i+3] == 1 {
                startCodeLen = 4
            } else if i + 2 < count && bytes[i] == 0 && bytes[i+1] == 0 && bytes[i+2] == 1 {
                startCodeLen = 3
            }

            if startCodeLen > 0 {
                let naluStart = i + startCodeLen
                var naluEnd = count
                var j = naluStart + 1
                while j < count - 2 {
                    if bytes[j] == 0 && bytes[j+1] == 0 && (bytes[j+2] == 1 || (j + 3 < count && bytes[j+2] == 0 && bytes[j+3] == 1)) {
                        naluEnd = j
                        break
                    }
                    j += 1
                }
                if naluStart < naluEnd {
                    nalus.append(Data(bytes[naluStart..<naluEnd]))
                }
                i = naluEnd
            } else {
                i += 1
            }
        }

        return nalus
    }
}

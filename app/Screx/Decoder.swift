import Foundation
import AVFoundation
import VideoToolbox
import CoreMedia

final class H264Decoder {
    let displayLayer = AVSampleBufferDisplayLayer()
    var hasReportedFirstFrame = false
    private(set) var videoWidth: Int = 1920
    private(set) var videoHeight: Int = 1080

    private var formatDescription: CMVideoFormatDescription?
    private var sps: Data?
    private var pps: Data?
    private var naluCount = 0

    private var framesReceived: UInt64 = 0
    private var framesDisplayed: UInt64 = 0
    private var statsWindowStart = CACurrentMediaTime()

    private let renderQueue = DispatchQueue(label: "screx.render", qos: .userInteractive)
    private var naluBuf = [UInt8]()

    init() {
        displayLayer.videoGravity = .resizeAspect
    }

    func decodeAccessUnit(_ data: Data) {
        let bytes = [UInt8](data)
        let count = bytes.count
        var i = 0

        while i < count {
            var startCodeLen = 0
            if i + 3 < count && bytes[i] == 0 && bytes[i+1] == 0 && bytes[i+2] == 0 && bytes[i+3] == 1 {
                startCodeLen = 4
            } else if i + 2 < count && bytes[i] == 0 && bytes[i+1] == 0 && bytes[i+2] == 1 {
                startCodeLen = 3
            }

            guard startCodeLen > 0 else { i += 1; continue }

            let naluStart = i + startCodeLen
            var naluEnd = count
            var j = naluStart + 1
            while j < count - 2 {
                if bytes[j] == 0 && bytes[j+1] == 0 &&
                    (bytes[j+2] == 1 || (j + 3 < count && bytes[j+2] == 0 && bytes[j+3] == 1)) {
                    naluEnd = j
                    break
                }
                j += 1
            }

            let length = naluEnd - naluStart
            guard length > 0 else { i = naluEnd; continue }

            let naluType = bytes[naluStart] & 0x1F
            naluCount += 1

            if naluCount <= 5 {
                print("[decoder] NALU #\(naluCount) type=\(naluType) len=\(length)")
            }

            switch naluType {
            case 7:
                sps = Data(bytes[naluStart..<naluEnd])
                tryBuildFormatDescription()
            case 8:
                pps = Data(bytes[naluStart..<naluEnd])
                tryBuildFormatDescription()
            case 1, 5:
                if formatDescription != nil {
                    enqueueSlice(bytes: bytes, offset: naluStart, length: length)
                } else if naluCount <= 10 {
                    print("[decoder] dropping slice type=\(naluType), no format description yet")
                }
            default:
                break
            }

            i = naluEnd
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
            videoWidth = Int(dims.width)
            videoHeight = Int(dims.height)
            if naluCount <= 20 {
                print("[decoder] format description created: \(dims.width)x\(dims.height)")
            }
        } else if naluCount <= 20 {
            print("[decoder] CMVideoFormatDescriptionCreateFromH264ParameterSets failed: \(status)")
        }
    }

    private func enqueueSlice(bytes: [UInt8], offset: Int, length: Int) {
        guard let formatDescription else { return }

        let totalLen = 4 + length

        if naluBuf.count < totalLen {
            naluBuf = [UInt8](repeating: 0, count: max(totalLen, naluBuf.count * 2))
        }

        let len32 = UInt32(length)
        naluBuf[0] = UInt8((len32 >> 24) & 0xFF)
        naluBuf[1] = UInt8((len32 >> 16) & 0xFF)
        naluBuf[2] = UInt8((len32 >> 8) & 0xFF)
        naluBuf[3] = UInt8(len32 & 0xFF)

        naluBuf.withUnsafeMutableBufferPointer { dst in
            bytes.withUnsafeBufferPointer { src in
                (dst.baseAddress! + 4).update(from: src.baseAddress! + offset, count: length)
            }
        }

        var blockBuffer: CMBlockBuffer?
        naluBuf.withUnsafeBufferPointer { bufPtr in
            let ptr = bufPtr.baseAddress!
            CMBlockBufferCreateWithMemoryBlock(
                allocator: kCFAllocatorDefault,
                memoryBlock: nil,
                blockLength: totalLen,
                blockAllocator: kCFAllocatorDefault,
                customBlockSource: nil,
                offsetToData: 0,
                dataLength: totalLen,
                flags: 0,
                blockBufferOut: &blockBuffer
            )
            if let bb = blockBuffer {
                CMBlockBufferReplaceDataBytes(
                    with: ptr,
                    blockBuffer: bb,
                    offsetIntoDestination: 0,
                    dataLength: totalLen
                )
            }
        }

        guard let blockBuffer else { return }

        var sampleBuffer: CMSampleBuffer?
        var sampleSize = totalLen
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

        renderQueue.async { [weak self] in
            guard let self else { return }
            if self.displayLayer.status == .failed {
                self.displayLayer.flush()
            }
            self.displayLayer.enqueue(sampleBuffer)
            self.framesDisplayed += 1

            let now = CACurrentMediaTime()
            let elapsed = now - self.statsWindowStart
            if elapsed >= 2.0 {
                let recvFps = Double(self.framesReceived) / elapsed
                let dispFps = Double(self.framesDisplayed) / elapsed
                print("[decoder] recv_fps=\(String(format: "%.1f", recvFps)) display_fps=\(String(format: "%.1f", dispFps))")
                self.framesReceived = 0
                self.framesDisplayed = 0
                self.statsWindowStart = now
            }
        }
    }
}

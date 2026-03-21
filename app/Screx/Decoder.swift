import Foundation
import AVFoundation
import VideoToolbox
import CoreMedia

enum VideoCodecType {
    case h264
    case h265
}

final class VideoDecoder {
    let displayLayer = AVSampleBufferDisplayLayer()
    var hasReportedFirstFrame = false
    private(set) var videoWidth: Int = 1920
    private(set) var videoHeight: Int = 1080

    private(set) var codec: VideoCodecType = .h264

    private var formatDescription: CMVideoFormatDescription?
    private var sps: Data?
    private var pps: Data?
    private var vps: Data?  // H.265 only
    private var naluCount = 0

    private var framesReceived: UInt64 = 0
    private var framesDisplayed: UInt64 = 0
    private var statsWindowStart = CACurrentMediaTime()

    private let renderQueue = DispatchQueue(label: "screx.render", qos: .userInteractive)
    private var naluBuf = [UInt8]()

    init() {
        displayLayer.videoGravity = .resizeAspect
    }

    func setCodec(_ codec: VideoCodecType) {
        guard codec != self.codec else { return }
        self.codec = codec
        formatDescription = nil
        sps = nil
        pps = nil
        vps = nil
        naluCount = 0
        let codecName = codec == .h264 ? "H.264" : "H.265"
        print("[decoder] codec set to \(codecName)")
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

            naluCount += 1

            switch codec {
            case .h264:
                handleH264Nalu(bytes: bytes, naluStart: naluStart, naluEnd: naluEnd, length: length)
            case .h265:
                handleH265Nalu(bytes: bytes, naluStart: naluStart, naluEnd: naluEnd, length: length)
            }

            i = naluEnd
        }
    }

    // MARK: - H.264

    private func handleH264Nalu(bytes: [UInt8], naluStart: Int, naluEnd: Int, length: Int) {
        let naluType = bytes[naluStart] & 0x1F

        if naluCount <= 5 {
            print("[decoder] H.264 NALU #\(naluCount) type=\(naluType) len=\(length)")
        }

        switch naluType {
        case 7:
            sps = Data(bytes[naluStart..<naluEnd])
            tryBuildH264FormatDescription()
        case 8:
            pps = Data(bytes[naluStart..<naluEnd])
            tryBuildH264FormatDescription()
        case 1, 5:
            if formatDescription != nil {
                enqueueSlice(bytes: bytes, offset: naluStart, length: length)
            }
        default:
            break
        }
    }

    private func tryBuildH264FormatDescription() {
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
                print("[decoder] H.264 format: \(dims.width)x\(dims.height)")
            }
        } else if naluCount <= 20 {
            print("[decoder] H.264 format description failed: \(status)")
        }
    }

    // MARK: - H.265

    private func handleH265Nalu(bytes: [UInt8], naluStart: Int, naluEnd: Int, length: Int) {
        guard length >= 2 else { return }
        let naluType = (bytes[naluStart] >> 1) & 0x3F

        if naluCount <= 5 {
            print("[decoder] H.265 NALU #\(naluCount) type=\(naluType) len=\(length)")
        }

        switch naluType {
        case 32: // VPS
            vps = Data(bytes[naluStart..<naluEnd])
            tryBuildH265FormatDescription()
        case 33: // SPS
            sps = Data(bytes[naluStart..<naluEnd])
            tryBuildH265FormatDescription()
        case 34: // PPS
            pps = Data(bytes[naluStart..<naluEnd])
            tryBuildH265FormatDescription()
        case 0...9, 16...21: // VCL NAL units (TRAIL, TSA, STSA, RADL, RASL, BLA, IDR, CRA)
            if formatDescription != nil {
                enqueueSlice(bytes: bytes, offset: naluStart, length: length)
            }
        default:
            break
        }
    }

    private func tryBuildH265FormatDescription() {
        guard let vps, let sps, let pps else { return }

        let vpsBytes = [UInt8](vps)
        let spsBytes = [UInt8](sps)
        let ppsBytes = [UInt8](pps)

        var newFmt: CMVideoFormatDescription?
        let status = vpsBytes.withUnsafeBufferPointer { vpsBuf in
            spsBytes.withUnsafeBufferPointer { spsBuf in
                ppsBytes.withUnsafeBufferPointer { ppsBuf in
                    var paramPointers: [UnsafePointer<UInt8>] = [
                        vpsBuf.baseAddress!, spsBuf.baseAddress!, ppsBuf.baseAddress!
                    ]
                    var paramSizes: [Int] = [vpsBytes.count, spsBytes.count, ppsBytes.count]
                    return CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                        allocator: kCFAllocatorDefault,
                        parameterSetCount: 3,
                        parameterSetPointers: &paramPointers,
                        parameterSetSizes: &paramSizes,
                        nalUnitHeaderLength: 4,
                        extensions: nil,
                        formatDescriptionOut: &newFmt
                    )
                }
            }
        }

        if status == noErr, let newFmt {
            formatDescription = newFmt
            let dims = CMVideoFormatDescriptionGetDimensions(newFmt)
            videoWidth = Int(dims.width)
            videoHeight = Int(dims.height)
            if naluCount <= 20 {
                print("[decoder] H.265 format: \(dims.width)x\(dims.height)")
            }
        } else if naluCount <= 20 {
            print("[decoder] H.265 format description failed: \(status)")
        }
    }

    // MARK: - Shared slice enqueueing

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

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
    private(set) var isSuspended = false

    private(set) var codec: VideoCodecType = .h264

    private var formatDescription: CMVideoFormatDescription?
    private var sps: Data?
    private var pps: Data?
    private var vps: Data?  // H.265 only
    private var naluCount = 0

    private var framesReceived: UInt64 = 0
    private var framesEnqueued: UInt64 = 0
    private var framesDropped: UInt64 = 0
    private var statsWindowStart = CACurrentMediaTime()

    private let renderQueue = DispatchQueue(label: "screx.render", qos: .userInteractive)
    /// Slice NAL ranges of the access unit being parsed; reused across frames.
    private var vclRanges: [Range<Int>] = []

    var sendPliRequest: (() -> Void)?
    var onFrameReady: ((UInt32) -> Void)?
    // Accessed only on renderQueue. A prior generation cannot acknowledge a replacement.
    private var renderGeneration: UInt32?

    init() {
        displayLayer.videoGravity = .resizeAspect
    }

    func setSuspended(_ suspended: Bool) {
        guard suspended != isSuspended else { return }
        isSuspended = suspended
        renderQueue.async { [weak self] in
            guard let self else { return }
            if suspended {
                self.displayLayer.flushAndRemoveImage()
            } else {
                self.displayLayer.flush()
            }
        }
    }

    func setCodec(_ codec: VideoCodecType) {
        guard codec != self.codec else { return }
        beginStream(codec: codec)
    }

    /// Called on the receive queue before submitting a new authenticated generation.
    /// Clear the prior image so isReadyForDisplay can only acknowledge the new generation.
    /// Other output layers remain untouched throughout topology transitions.
    func beginStream(codec: VideoCodecType, generation: UInt32? = nil) {
        self.codec = codec
        formatDescription = nil
        sps = nil
        pps = nil
        vps = nil
        naluCount = 0
        hasReportedFirstFrame = false
        renderQueue.async { [weak self] in
            guard let self else { return }
            self.renderGeneration = generation
            self.displayLayer.flushAndRemoveImage()
            if let generation { self.checkPresentation(generation: generation, attempts: 0) }
        }
        let codecName = codec == .h264 ? "H.264" : "H.265"
        print("[decoder] codec set to \(codecName)")
    }

    private func checkPresentation(generation: UInt32, attempts: Int) {
        guard renderGeneration == generation else { return }
        let frameReady: Bool
        if #available(iOS 17.4, *) {
            frameReady = displayLayer.isReadyForDisplay
        } else {
            // Earlier iOS exposes only the renderer state, reset by flushAndRemoveImage.
            frameReady = displayLayer.status == .rendering
        }
        if frameReady && displayLayer.status == .rendering {
            onFrameReady?(generation)
            return
        }
        guard attempts < 300 else { return }
        renderQueue.asyncAfter(deadline: .now() + 0.05) { [weak self] in
            self?.checkPresentation(generation: generation, attempts: attempts + 1)
        }
    }

    func decodeAccessUnit(_ data: Data) {
        guard !isSuspended else { return }
        data.withUnsafeBytes { raw in
            guard let base = raw.baseAddress?.assumingMemoryBound(to: UInt8.self) else { return }
            parseNALUnits(base, count: raw.count)
        }
    }

    private func parseNALUnits(_ bytes: UnsafePointer<UInt8>, count: Int) {
        var i = 0
        vclRanges.removeAll(keepingCapacity: true)

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

            let isVCL: Bool
            switch codec {
            case .h264:
                isVCL = handleH264Nalu(bytes: bytes, naluStart: naluStart, naluEnd: naluEnd, length: length)
            case .h265:
                isVCL = handleH265Nalu(bytes: bytes, naluStart: naluStart, naluEnd: naluEnd, length: length)
            }
            if isVCL {
                vclRanges.append(naluStart..<naluEnd)
            }

            i = naluEnd
        }

        if !vclRanges.isEmpty {
            enqueueAccessUnit(bytes: bytes, ranges: vclRanges)
        }
    }

    /// Stores a parameter set, returning true only if it differs from the stored one.
    /// Every keyframe repeats SPS/PPS(/VPS); rebuilding the format description each
    /// time would make the display layer treat every keyframe as a format change.
    private static func storeParameterSet(_ stored: inout Data?, bytes: UnsafePointer<UInt8>, range: Range<Int>) -> Bool {
        let incoming = UnsafeBufferPointer(start: bytes + range.lowerBound, count: range.count)
        if let existing = stored, existing.elementsEqual(incoming) {
            return false
        }
        stored = Data(buffer: incoming)
        return true
    }

    // MARK: - H.264

    /// Returns true for a slice NAL that belongs in the access unit's sample buffer.
    private func handleH264Nalu(bytes: UnsafePointer<UInt8>, naluStart: Int, naluEnd: Int, length: Int) -> Bool {
        let naluType = bytes[naluStart] & 0x1F

        if naluCount <= 5 {
            print("[decoder] H.264 NALU #\(naluCount) type=\(naluType) len=\(length)")
        }

        switch naluType {
        case 7:
            if Self.storeParameterSet(&sps, bytes: bytes, range: naluStart..<naluEnd) {
                tryBuildH264FormatDescription()
            }
            return false
        case 8:
            if Self.storeParameterSet(&pps, bytes: bytes, range: naluStart..<naluEnd) {
                tryBuildH264FormatDescription()
            }
            return false
        case 1, 5:
            return formatDescription != nil
        default:
            return false
        }
    }

    private func tryBuildH264FormatDescription() {
        guard let sps, let pps else { return }

        var newFmt: CMVideoFormatDescription?
        let status = sps.withUnsafeBytes { spsRaw in
            pps.withUnsafeBytes { ppsRaw in
                let spsPtr = spsRaw.baseAddress!.assumingMemoryBound(to: UInt8.self)
                let ppsPtr = ppsRaw.baseAddress!.assumingMemoryBound(to: UInt8.self)
                var paramPointers: [UnsafePointer<UInt8>] = [spsPtr, ppsPtr]
                var paramSizes: [Int] = [spsRaw.count, ppsRaw.count]
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

    /// Returns true for a slice NAL that belongs in the access unit's sample buffer.
    private func handleH265Nalu(bytes: UnsafePointer<UInt8>, naluStart: Int, naluEnd: Int, length: Int) -> Bool {
        guard length >= 2 else { return false }
        let naluType = (bytes[naluStart] >> 1) & 0x3F

        if naluCount <= 5 {
            print("[decoder] H.265 NALU #\(naluCount) type=\(naluType) len=\(length)")
        }

        switch naluType {
        case 32: // VPS
            if Self.storeParameterSet(&vps, bytes: bytes, range: naluStart..<naluEnd) {
                tryBuildH265FormatDescription()
            }
            return false
        case 33: // SPS
            if Self.storeParameterSet(&sps, bytes: bytes, range: naluStart..<naluEnd) {
                tryBuildH265FormatDescription()
            }
            return false
        case 34: // PPS
            if Self.storeParameterSet(&pps, bytes: bytes, range: naluStart..<naluEnd) {
                tryBuildH265FormatDescription()
            }
            return false
        case 0...9, 16...21:
            return formatDescription != nil
        default:
            return false
        }
    }

    private func tryBuildH265FormatDescription() {
        guard let vps, let sps, let pps else { return }

        var newFmt: CMVideoFormatDescription?
        let status = vps.withUnsafeBytes { vpsRaw in
            sps.withUnsafeBytes { spsRaw in
                pps.withUnsafeBytes { ppsRaw in
                    let vpsPtr = vpsRaw.baseAddress!.assumingMemoryBound(to: UInt8.self)
                    let spsPtr = spsRaw.baseAddress!.assumingMemoryBound(to: UInt8.self)
                    let ppsPtr = ppsRaw.baseAddress!.assumingMemoryBound(to: UInt8.self)
                    var paramPointers: [UnsafePointer<UInt8>] = [vpsPtr, spsPtr, ppsPtr]
                    var paramSizes: [Int] = [vpsRaw.count, spsRaw.count, ppsRaw.count]
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

    // MARK: - Access unit enqueueing

    /// Packs every slice NAL of one access unit into a single AVCC-framed sample
    /// buffer (4-byte big-endian length prefixes), written straight into the
    /// CMBlockBuffer's memory. One sample per frame: a multi-slice frame enqueued as
    /// separate display-immediately samples would present partial pictures.
    private func enqueueAccessUnit(bytes: UnsafePointer<UInt8>, ranges: [Range<Int>]) {
        guard let formatDescription else { return }

        let totalLen = ranges.reduce(0) { $0 + 4 + $1.count }

        var blockBufferOut: CMBlockBuffer?
        guard CMBlockBufferCreateWithMemoryBlock(
            allocator: kCFAllocatorDefault,
            memoryBlock: nil,
            blockLength: totalLen,
            blockAllocator: kCFAllocatorDefault,
            customBlockSource: nil,
            offsetToData: 0,
            dataLength: totalLen,
            flags: kCMBlockBufferAssureMemoryNowFlag,
            blockBufferOut: &blockBufferOut
        ) == kCMBlockBufferNoErr, let blockBuffer = blockBufferOut else { return }

        var dataPointerOut: UnsafeMutablePointer<CChar>?
        guard CMBlockBufferGetDataPointer(
            blockBuffer,
            atOffset: 0,
            lengthAtOffsetOut: nil,
            totalLengthOut: nil,
            dataPointerOut: &dataPointerOut
        ) == kCMBlockBufferNoErr, let dataPointer = dataPointerOut else { return }

        let dst = UnsafeMutableRawPointer(dataPointer).assumingMemoryBound(to: UInt8.self)
        var offset = 0
        for range in ranges {
            let len32 = UInt32(range.count)
            dst[offset] = UInt8((len32 >> 24) & 0xFF)
            dst[offset + 1] = UInt8((len32 >> 16) & 0xFF)
            dst[offset + 2] = UInt8((len32 >> 8) & 0xFF)
            dst[offset + 3] = UInt8(len32 & 0xFF)
            (dst + offset + 4).update(from: bytes + range.lowerBound, count: range.count)
            offset += 4 + range.count
        }

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

            if !self.displayLayer.isReadyForMoreMediaData {
                // The display layer is backed up (decode/present can't keep up).
                // Drop the stale backlog and enqueue the freshest frame instead
                // of letting display latency ratchet up. This is NOT loss —
                // there is no reorder/loss evidence — so we deliberately do not
                // request a keyframe here (Moonlight/Parsec: no buffer, drop
                // stale, present newest). PLI is only for confirmed frame_id
                // gaps in StreamClient.
                self.framesDropped += 1
                self.displayLayer.flush()
            }

            self.displayLayer.enqueue(sampleBuffer)
            self.framesEnqueued += 1

            let now = CACurrentMediaTime()
            let elapsed = now - self.statsWindowStart
            if elapsed >= 2.0 {
                let recvFps = Double(self.framesReceived) / elapsed
                let enqFps = Double(self.framesEnqueued) / elapsed
                let dropFps = Double(self.framesDropped) / elapsed
                print("[decoder] recv_fps=\(String(format: "%.1f", recvFps)) enq_fps=\(String(format: "%.1f", enqFps)) drop_fps=\(String(format: "%.1f", dropFps))")
                self.framesReceived = 0
                self.framesEnqueued = 0
                self.framesDropped = 0
                self.statsWindowStart = now
            }
        }
    }
}

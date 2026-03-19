import Foundation
import AVFoundation
import CoreMedia
import VideoToolbox
import Combine

struct DecoderHealth {
    let droppedFrames: Int
    let displayErrors: Int
}

final class DecoderService: ObservableObject {
    let displayLayer: AVSampleBufferDisplayLayer = AVSampleBufferDisplayLayer()
    var onRequestIDR: (() -> Void)?
    var onHealthUpdate: ((DecoderHealth) -> Void)?

    private let decodeQueue = DispatchQueue(label: "screx.decode", qos: .userInteractive)
    private var vps: Data?
    private var sps: Data?
    private var pps: Data?
    private var formatDescription: CMFormatDescription?
    private var accessUnitNalus: [Data] = []
    private var droppedFrames = 0
    private var displayErrors = 0
    private var nalCounters = NalCounters()
    private var nextNalLogAt = Date()

    init() {
        displayLayer.videoGravity = .resizeAspect
    }

    func handleNalu(nalu: Data, timestamp90k: UInt32, isAccessUnitEnd: Bool) {
        decodeQueue.async { [weak self] in
            self?.processNalu(nalu: nalu, timestamp90k: timestamp90k, isAccessUnitEnd: isAccessUnitEnd)
        }
    }

    private func processNalu(nalu: Data, timestamp90k: UInt32, isAccessUnitEnd: Bool) {
        guard nalu.count >= 2 else { return }
        let nalType = (nalu[nalu.startIndex] >> 1) & 0x3F
        nalCounters.record(type: Int(nalType))
        maybeLogNalCounters()

        switch nalType {
        case 32:
            vps = nalu
            rebuildFormatDescriptionIfPossible()
        case 33:
            sps = nalu
            rebuildFormatDescriptionIfPossible()
        case 34:
            pps = nalu
            rebuildFormatDescriptionIfPossible()
        case 35:
            // AUD: delimiter only, not needed in AVCC payload.
            break
        default:
            accessUnitNalus.append(nalu)
        }

        if isAccessUnitEnd {
            enqueueAccessUnit(timestamp90k: timestamp90k)
        }
    }

    private func rebuildFormatDescriptionIfPossible() {
        guard let vps, let sps, let pps else { return }
        formatDescription = makeHEVCFormatDescription(vps: vps, sps: sps, pps: pps)
    }

    private func makeHEVCFormatDescription(vps: Data, sps: Data, pps: Data) -> CMFormatDescription? {
        return vps.withUnsafeBytes { vpsPtr in
            return sps.withUnsafeBytes { spsPtr in
                return pps.withUnsafeBytes { ppsPtr in
                    guard
                        let p0 = vpsPtr.bindMemory(to: UInt8.self).baseAddress,
                        let p1 = spsPtr.bindMemory(to: UInt8.self).baseAddress,
                        let p2 = ppsPtr.bindMemory(to: UInt8.self).baseAddress
                    else {
                        return nil
                    }

                    var formatDesc: CMFormatDescription?
                    let pointers: [UnsafePointer<UInt8>] = [p0, p1, p2]
                    let sizes: [Int] = [vps.count, sps.count, pps.count]
                    let status = CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                        allocator: kCFAllocatorDefault,
                        parameterSetCount: 3,
                        parameterSetPointers: pointers,
                        parameterSetSizes: sizes,
                        nalUnitHeaderLength: 4,
                        extensions: nil,
                        formatDescriptionOut: &formatDesc
                    )
                    guard status == noErr else { return nil }
                    return formatDesc
                }
            }
        }
    }

    private func enqueueAccessUnit(timestamp90k _: UInt32) {
        guard !accessUnitNalus.isEmpty else { return }
        guard let formatDescription else {
            accessUnitNalus.removeAll(keepingCapacity: true)
            onRequestIDR?()
            return
        }

        let avcc = annexBToAvcc(nalus: accessUnitNalus)
        accessUnitNalus.removeAll(keepingCapacity: true)

        var blockBuffer: CMBlockBuffer?
        let createStatus = CMBlockBufferCreateWithMemoryBlock(
            allocator: kCFAllocatorDefault,
            memoryBlock: nil,
            blockLength: avcc.count,
            blockAllocator: nil,
            customBlockSource: nil,
            offsetToData: 0,
            dataLength: avcc.count,
            flags: 0,
            blockBufferOut: &blockBuffer
        )
        guard createStatus == kCMBlockBufferNoErr, let blockBuffer else {
            markDroppedFrame()
            onRequestIDR?()
            return
        }

        let writeStatus = avcc.withUnsafeBytes { bytes -> OSStatus in
            guard let base = bytes.baseAddress else { return -1 }
            return CMBlockBufferReplaceDataBytes(
                with: base,
                blockBuffer: blockBuffer,
                offsetIntoDestination: 0,
                dataLength: avcc.count
            )
        }
        guard writeStatus == kCMBlockBufferNoErr else {
            markDroppedFrame()
            onRequestIDR?()
            return
        }

        var timing = CMSampleTimingInfo(
            duration: CMTime(value: 1, timescale: 60),
            presentationTimeStamp: CMClockGetTime(CMClockGetHostTimeClock()),
            decodeTimeStamp: .invalid
        )
        var sampleBuffer: CMSampleBuffer?
        let sampleSize = [avcc.count]
        let sampleStatus = CMSampleBufferCreateReady(
            allocator: kCFAllocatorDefault,
            dataBuffer: blockBuffer,
            formatDescription: formatDescription,
            sampleCount: 1,
            sampleTimingEntryCount: 1,
            sampleTimingArray: &timing,
            sampleSizeEntryCount: 1,
            sampleSizeArray: sampleSize,
            sampleBufferOut: &sampleBuffer
        )
        guard sampleStatus == noErr, let sampleBuffer else {
            markDroppedFrame()
            onRequestIDR?()
            return
        }

        if let attachments = CMSampleBufferGetSampleAttachmentsArray(
            sampleBuffer,
            createIfNecessary: true
        ) {
            let dict = unsafeBitCast(
                CFArrayGetValueAtIndex(attachments, 0),
                to: CFMutableDictionary.self
            )
            CFDictionarySetValue(
                dict,
                Unmanaged.passUnretained(kCMSampleAttachmentKey_DisplayImmediately).toOpaque(),
                Unmanaged.passUnretained(kCFBooleanTrue).toOpaque()
            )
        }

        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            if self.displayLayer.status == .failed {
                self.displayErrors += 1
                self.publishHealth()
                self.displayLayer.flushAndRemoveImage()
                self.onRequestIDR?()
            }
            if !self.displayLayer.isReadyForMoreMediaData {
                // Drop stale queued frames to avoid seconds-long delayed playback loops.
                self.markDroppedFrame()
                self.displayLayer.flush()
            }
            self.displayLayer.enqueue(sampleBuffer)
            if self.displayLayer.error != nil {
                self.displayErrors += 1
                self.publishHealth()
                self.displayLayer.flushAndRemoveImage()
                self.onRequestIDR?()
            }
        }
    }

    private func annexBToAvcc(nalus: [Data]) -> Data {
        var output = Data(capacity: nalus.reduce(0) { $0 + $1.count + 4 })
        for nalu in nalus {
            var lengthBE = UInt32(nalu.count).bigEndian
            withUnsafeBytes(of: &lengthBE) { output.append(contentsOf: $0) }
            output.append(nalu)
        }
        return output
    }

    private func markDroppedFrame() {
        droppedFrames += 1
        publishHealth()
    }

    private func publishHealth() {
        onHealthUpdate?(DecoderHealth(droppedFrames: droppedFrames, displayErrors: displayErrors))
    }

    private func maybeLogNalCounters() {
        let now = Date()
        guard now >= nextNalLogAt else { return }
        nextNalLogAt = now.addingTimeInterval(1.0)
        print(
            "[decoder] nals vps=\(nalCounters.vps) sps=\(nalCounters.sps) pps=\(nalCounters.pps) idr=\(nalCounters.idr) vcl=\(nalCounters.vcl) aud=\(nalCounters.aud) other=\(nalCounters.other)"
        )
    }
}

private struct NalCounters {
    var vps = 0
    var sps = 0
    var pps = 0
    var idr = 0
    var vcl = 0
    var aud = 0
    var other = 0

    mutating func record(type: Int) {
        switch type {
        case 32: vps += 1
        case 33: sps += 1
        case 34: pps += 1
        case 16...23: idr += 1
        case 0...31: vcl += 1
        case 35: aud += 1
        default: other += 1
        }
    }
}

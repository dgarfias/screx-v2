import AVFoundation
import CoreImage
import ImageIO
import UniformTypeIdentifiers
import UIKit

final class CameraCapture: NSObject, AVCaptureVideoDataOutputSampleBufferDelegate {
    private static let defaultCompressionQuality: CGFloat = 0.4

    enum CaptureProfile: Equatable {
        case network
        case usb

        var sessionPreset: AVCaptureSession.Preset {
            switch self {
            case .network:
                return .hd1280x720
            case .usb:
                return .hd1920x1080
            }
        }

        var outputSize: CGSize {
            switch self {
            case .network:
                return CGSize(width: 1280, height: 720)
            case .usb:
                return CGSize(width: 1920, height: 1080)
            }
        }

        var label: String {
            switch self {
            case .network:
                return "720p"
            case .usb:
                return "1080p"
            }
        }

        var targetFps: Int32 {
            switch self {
            case .network:
                return 30
            case .usb:
                return 60
            }
        }
    }

    private let session = AVCaptureSession()
    private let outputQueue = DispatchQueue(label: "screx.camera", qos: .userInitiated)
    private let ciContext = CIContext(options: [.cacheIntermediates: false])
    private var running = false
    private var frameCount: UInt32 = 0
    private(set) var usingFront = false
    private var configuredFps: Int32 = CaptureProfile.network.targetFps
    var compressionQuality: CGFloat = CameraCapture.defaultCompressionQuality
    var captureProfile: CaptureProfile = .network {
        didSet {
            guard captureProfile != oldValue, running else { return }
            let front = usingFront
            stop()
            startSession(front: front)
        }
    }

    var onJPEG: ((Data) -> Void)?

    func start() {
        guard !running else { return }
        startSession(front: usingFront)
    }

    private func startSession(front: Bool) {
        let profile = captureProfile
        if session.canSetSessionPreset(profile.sessionPreset) {
            session.sessionPreset = profile.sessionPreset
        } else {
            session.sessionPreset = .hd1280x720
            print("[camera] \(profile.label) preset unavailable, falling back to 720p")
        }

        let position: AVCaptureDevice.Position = front ? .front : .back
        guard let device = AVCaptureDevice.default(.builtInWideAngleCamera, for: .video, position: position) else {
            print("[camera] no \(front ? "front" : "back") camera available")
            return
        }

        do {
            let input = try AVCaptureDeviceInput(device: device)
            if session.canAddInput(input) {
                session.addInput(input)
            }
        } catch {
            print("[camera] failed to create input: \(error)")
            return
        }

        let output = AVCaptureVideoDataOutput()
        output.videoSettings = [kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA]
        output.alwaysDiscardsLateVideoFrames = true
        output.setSampleBufferDelegate(self, queue: outputQueue)

        if session.canAddOutput(output) {
            session.addOutput(output)
        }
        if let connection = output.connection(with: .video) {
            applyOrientation(to: connection)
        }

        configuredFps = configureDevice(device, for: profile)

        session.startRunning()
        running = true
        usingFront = front
        print("[camera] capture started (\(profile.label), ~\(configuredFps)fps, \(front ? "front" : "back"))")
    }

    func stop() {
        guard running else { return }
        session.stopRunning()
        session.inputs.forEach { session.removeInput($0) }
        session.outputs.forEach { session.removeOutput($0) }
        running = false
        frameCount = 0
        print("[camera] capture stopped")
    }

    func flipCamera() {
        guard running else {
            usingFront.toggle()
            return
        }
        session.beginConfiguration()
        session.inputs.forEach { session.removeInput($0) }

        let position: AVCaptureDevice.Position = usingFront ? .back : .front
        guard let device = AVCaptureDevice.default(.builtInWideAngleCamera, for: .video, position: position),
              let input = try? AVCaptureDeviceInput(device: device),
              session.canAddInput(input) else {
            session.commitConfiguration()
            print("[camera] flip failed — no \(position == .front ? "front" : "back") camera")
            return
        }
        session.addInput(input)
        session.commitConfiguration()

        configuredFps = configureDevice(device, for: captureProfile)

        usingFront.toggle()
        print("[camera] flipped to \(usingFront ? "front" : "back")")
    }

    var isRunning: Bool { running }

    func captureOutput(_ output: AVCaptureOutput, didOutput sampleBuffer: CMSampleBuffer, from connection: AVCaptureConnection) {
        guard let onJPEG else { return }
        guard let pixelBuffer = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }
        applyOrientation(to: connection)

        autoreleasepool {
            let ciImage = CIImage(cvPixelBuffer: pixelBuffer)
            let outputSize = captureProfile.outputSize
            let targetRect = CGRect(x: 0, y: 0, width: outputSize.width, height: outputSize.height)
            let framedImage = frameImage(ciImage, in: targetRect)
            guard let cgImage = ciContext.createCGImage(framedImage, from: targetRect) else { return }

            let jpegData = NSMutableData()
            guard let destination = CGImageDestinationCreateWithData(
                jpegData as CFMutableData,
                UTType.jpeg.identifier as CFString,
                1,
                nil
            ) else {
                return
            }

            let options = [
                kCGImageDestinationLossyCompressionQuality: compressionQuality
            ] as CFDictionary
            CGImageDestinationAddImage(destination, cgImage, options)

            guard CGImageDestinationFinalize(destination) else { return }
            onJPEG(jpegData as Data)
        }
    }

    private func frameImage(_ image: CIImage, in targetRect: CGRect) -> CIImage {
        let sourceRect = image.extent.integral
        guard sourceRect.width > 0, sourceRect.height > 0 else {
            return CIImage(color: .black).cropped(to: targetRect)
        }

        let scale = min(targetRect.width / sourceRect.width, targetRect.height / sourceRect.height)
        let scaledImage = image.transformed(by: CGAffineTransform(scaleX: scale, y: scale))
        let scaledRect = scaledImage.extent
        let translateX = targetRect.midX - scaledRect.midX
        let translateY = targetRect.midY - scaledRect.midY
        let positionedImage = scaledImage.transformed(
            by: CGAffineTransform(translationX: translateX, y: translateY)
        )
        let background = CIImage(color: .black).cropped(to: targetRect)
        return positionedImage.composited(over: background)
    }

    private func configureDevice(_ device: AVCaptureDevice, for profile: CaptureProfile) -> Int32 {
        do {
            try device.lockForConfiguration()

            if let preferredFormat = preferredFormat(for: device, profile: profile) {
                device.activeFormat = preferredFormat
            }

            let resolvedFps = supportedFps(for: device.activeFormat, preferred: profile.targetFps)
                ?? profile.targetFps
            let frameDuration = CMTime(value: 1, timescale: resolvedFps)
            device.activeVideoMinFrameDuration = frameDuration
            device.activeVideoMaxFrameDuration = frameDuration
            device.unlockForConfiguration()

            if resolvedFps != profile.targetFps {
                print("[camera] \(profile.label) \(profile.targetFps)fps unsupported, falling back to \(resolvedFps)fps")
            }

            return resolvedFps
        } catch {
            print("[camera] failed to configure camera format: \(error)")
            return profile.targetFps
        }
    }

    private func preferredFormat(for device: AVCaptureDevice, profile: CaptureProfile) -> AVCaptureDevice.Format? {
        let targetSize = profile.outputSize
        let targetWidth = Int32(targetSize.width)
        let targetHeight = Int32(targetSize.height)
        let targetArea = Int64(targetWidth * targetHeight)
        let targetFps = Double(profile.targetFps)

        return device.formats
            .compactMap { format -> (AVCaptureDevice.Format, Int64, Double)? in
                let dimensions = CMVideoFormatDescriptionGetDimensions(format.formatDescription)
                guard dimensions.width >= targetWidth, dimensions.height >= targetHeight else {
                    return nil
                }

                let maxFrameRate = format.videoSupportedFrameRateRanges
                    .map(\.maxFrameRate)
                    .max() ?? 0
                guard maxFrameRate >= targetFps else {
                    return nil
                }

                let area = Int64(dimensions.width * dimensions.height)
                return (format, area - targetArea, maxFrameRate)
            }
            .min { lhs, rhs in
                if lhs.1 != rhs.1 {
                    return lhs.1 < rhs.1
                }
                return lhs.2 < rhs.2
            }?
            .0
    }

    private func supportedFps(for format: AVCaptureDevice.Format, preferred preferredFps: Int32) -> Int32? {
        let preferred = Double(preferredFps)
        let ranges = format.videoSupportedFrameRateRanges

        if ranges.contains(where: { preferred >= $0.minFrameRate && preferred <= $0.maxFrameRate }) {
            return preferredFps
        }

        let fallback = ranges
            .map { Int32($0.maxFrameRate.rounded(.down)) }
            .filter { $0 > 0 }
            .max()
        return fallback
    }

    private func applyOrientation(to connection: AVCaptureConnection) {
        guard let orientation = currentVideoOrientation() else { return }

        if connection.isVideoOrientationSupported {
            connection.videoOrientation = orientation
        }
    }

    private func currentVideoOrientation() -> AVCaptureVideoOrientation? {
        guard let scene = UIApplication.shared.connectedScenes
            .compactMap({ $0 as? UIWindowScene })
            .first(where: { $0.activationState == .foregroundActive })
            ?? UIApplication.shared.connectedScenes.compactMap({ $0 as? UIWindowScene }).first
        else {
            return nil
        }

        switch scene.interfaceOrientation {
        case .portrait:
            return .portrait
        case .portraitUpsideDown:
            return .portraitUpsideDown
        case .landscapeLeft:
            return .landscapeLeft
        case .landscapeRight:
            return .landscapeRight
        default:
            return nil
        }
    }
}

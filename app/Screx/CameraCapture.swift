import AVFoundation
import UIKit

final class CameraCapture: NSObject, AVCaptureVideoDataOutputSampleBufferDelegate {
    private let session = AVCaptureSession()
    private let outputQueue = DispatchQueue(label: "screx.camera", qos: .userInitiated)
    private var running = false
    private var frameCount: UInt32 = 0
    private(set) var usingFront = false

    var onJPEG: ((Data) -> Void)?

    func start() {
        guard !running else { return }
        startSession(front: usingFront)
    }

    private func startSession(front: Bool) {
        session.sessionPreset = .hd1280x720

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

        do {
            try device.lockForConfiguration()
            device.activeVideoMinFrameDuration = CMTime(value: 1, timescale: 15)
            device.activeVideoMaxFrameDuration = CMTime(value: 1, timescale: 15)
            device.unlockForConfiguration()
        } catch {
            print("[camera] failed to set frame rate: \(error)")
        }

        session.startRunning()
        running = true
        usingFront = front
        print("[camera] capture started (720p, ~15fps, \(front ? "front" : "back"))")
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

        do {
            try device.lockForConfiguration()
            device.activeVideoMinFrameDuration = CMTime(value: 1, timescale: 15)
            device.activeVideoMaxFrameDuration = CMTime(value: 1, timescale: 15)
            device.unlockForConfiguration()
        } catch {}

        usingFront.toggle()
        print("[camera] flipped to \(usingFront ? "front" : "back")")
    }

    var isRunning: Bool { running }

    func captureOutput(_ output: AVCaptureOutput, didOutput sampleBuffer: CMSampleBuffer, from connection: AVCaptureConnection) {
        guard let onJPEG else { return }
        guard let pixelBuffer = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }

        let ciImage = CIImage(cvPixelBuffer: pixelBuffer)
        let context = CIContext()
        guard let cgImage = context.createCGImage(ciImage, from: ciImage.extent) else { return }

        let uiImage = UIImage(cgImage: cgImage)
        guard let jpegData = uiImage.jpegData(compressionQuality: 0.5) else { return }

        onJPEG(jpegData)
    }
}

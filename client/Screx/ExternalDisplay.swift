import AVFoundation
import UIKit

/// Own only the external scene's video window. Never make it key: input belongs to the iPad.
@MainActor
final class ExternalDisplayCoordinator {
    private unowned let model: StreamViewModel
    private var scene: UIWindowScene?
    private var window: UIWindow?
    private var modeObserver: NSObjectProtocol?

    init(model: StreamViewModel) {
        self.model = model
        model.externalDisplayCoordinator = self
    }

    func connected(_ scene: UIWindowScene) {
        guard scene.session.role == .windowExternalDisplayNonInteractive else { return }
        if self.scene !== scene { removeWindow() }
        self.scene = scene
        if let modeObserver { NotificationCenter.default.removeObserver(modeObserver) }
        modeObserver = NotificationCenter.default.addObserver(forName: UIScreen.modeDidChangeNotification,
                                                             object: scene.screen, queue: .main) { [weak self] _ in
            Task { @MainActor in self?.refreshCapabilities() }
        }
        refreshCapabilities()
        let pixels = scene.screen.currentMode?.size ?? scene.screen.nativeBounds.size
        print("[external] connected: \(Int(pixels.width))x\(Int(pixels.height))")
        model.setExternalDisplayAvailable(true)
        reconcile()
    }

    func disconnected(_ scene: UIWindowScene) {
        guard self.scene === scene else { return }
        print("[external] scene disconnected; restoring internal output")
        removeWindow()
        self.scene = nil
        if let modeObserver { NotificationCenter.default.removeObserver(modeObserver) }
        modeObserver = nil
        model.setExternalDisplayCapabilities(nil)
        model.setExternalDisplayAvailable(false)
    }

    private func refreshCapabilities() {
        guard let screen = scene?.screen else { return }
        func resolution(_ mode: UIScreenMode) -> DisplayResolution {
            DisplayResolution(width: Int(mode.size.width), height: Int(mode.size.height))
        }
        model.setExternalDisplayCapabilities(ExternalDisplayCapabilities(
            resolutions: Array(Set(screen.availableModes.map(resolution))),
            preferredResolution: screen.preferredMode.map(resolution),
            maximumFramesPerSecond: screen.maximumFramesPerSecond > 0 ? screen.maximumFramesPerSecond : 60
        ))
    }

    func reconcile() {
        guard model.displayRouting.externalWindowNeeded, let scene else {
            removeWindow()
            return
        }
        guard window == nil else { return }
        let controller = ExternalVideoController(layer: model.externalDecoder.displayLayer) { [weak model] attached in
            model?.setDisplaySurfaceAttached(.externalDisplay, attached: attached)
        }
        let window = UIWindow(windowScene: scene)
        window.backgroundColor = .black
        window.overrideUserInterfaceStyle = .dark
        window.rootViewController = controller
        self.window = window
        window.isHidden = false
        print("[external] showing independent video window")
    }

    private func removeWindow() {
        guard let window else { return }
        model.setDisplaySurfaceAttached(.externalDisplay, attached: false)
        window.isHidden = true
        model.externalDecoder.displayLayer.removeFromSuperlayer()
        window.rootViewController = nil
        self.window = nil
        // No blank app-owned window: iPadOS is free to resume ordinary mirroring.
        print("[external] removed video window; system mirroring may resume")
    }
}

final class ExternalDisplaySceneDelegate: UIResponder, UIWindowSceneDelegate {
    func scene(_ scene: UIScene, willConnectTo session: UISceneSession,
               options connectionOptions: UIScene.ConnectionOptions) {
        guard let scene = scene as? UIWindowScene,
              let app = UIApplication.shared.delegate as? AppDelegate else { return }
        app.externalDisplay.connected(scene)
    }

    func sceneDidDisconnect(_ scene: UIScene) {
        guard let scene = scene as? UIWindowScene,
              let app = UIApplication.shared.delegate as? AppDelegate else { return }
        app.externalDisplay.disconnected(scene)
    }
    // External scene background/teardown must never disconnect the network session.
}

private final class ExternalVideoController: UIViewController {
    private let videoLayer: AVSampleBufferDisplayLayer
    private let attachmentChanged: (Bool) -> Void

    init(layer: AVSampleBufferDisplayLayer, attachmentChanged: @escaping (Bool) -> Void) {
        self.videoLayer = layer
        self.attachmentChanged = attachmentChanged
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    override func loadView() {
        let surface = ExternalVideoSurface()
        surface.backgroundColor = .black
        surface.isUserInteractionEnabled = false
        surface.attachmentChanged = attachmentChanged
        videoLayer.removeFromSuperlayer()
        surface.layer.addSublayer(videoLayer)
        view = surface
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        videoLayer.frame = view.bounds
        CATransaction.commit()
    }
}

private final class ExternalVideoSurface: UIView {
    var attachmentChanged: ((Bool) -> Void)?
    override func didMoveToWindow() {
        super.didMoveToWindow()
        attachmentChanged?(window != nil)
    }
}

import Combine
import Foundation
import UIKit
import GameController

/// Physical mouse/keyboard/controller passthrough, on-screen trackpad input,
/// and camera/microphone/speaker peripheral toggles for `StreamViewModel`.
@MainActor
extension StreamViewModel {
    // MARK: - Peripheral Monitoring (GCMouse / GCKeyboard / GCController)

    func startPeripheralMonitoring() {
        stopPeripheralMonitoring()

        // iPadOS can report several pointing devices (an external mouse, a keyboard trackpad,
        // a Bluetooth mouse reconnecting after sleep). Drive the desktop from all of them.
        for mouse in GCMouse.mice() {
            attachMouse(mouse)
        }
        if let kb = GCKeyboard.coalesced {
            attachKeyboard(kb)
        }
        refreshControllerPassthrough()

        let mc = NotificationCenter.default.addObserver(
            forName: .GCMouseDidConnect, object: nil, queue: .main
        ) { [weak self] note in
            guard let self, let mouse = note.object as? GCMouse else { return }
            Task { @MainActor in self.attachMouse(mouse) }
        }
        let md = NotificationCenter.default.addObserver(
            forName: .GCMouseDidDisconnect, object: nil, queue: .main
        ) { [weak self] note in
            guard let self, let mouse = note.object as? GCMouse else { return }
            Task { @MainActor in self.detachMouse(mouse) }
        }
        let kc = NotificationCenter.default.addObserver(
            forName: .GCKeyboardDidConnect, object: nil, queue: .main
        ) { [weak self] note in
            guard let self, let kb = note.object as? GCKeyboard else { return }
            Task { @MainActor in self.attachKeyboard(kb) }
        }
        let kd = NotificationCenter.default.addObserver(
            forName: .GCKeyboardDidDisconnect, object: nil, queue: .main
        ) { [weak self] _ in
            guard let self else { return }
            Task { @MainActor in self.detachKeyboard() }
        }
        let cc = NotificationCenter.default.addObserver(
            forName: .GCControllerDidConnect, object: nil, queue: .main
        ) { [weak self] note in
            guard let self, let controller = note.object as? GCController else { return }
            Task { @MainActor in self.attachController(controller) }
        }
        let cd = NotificationCenter.default.addObserver(
            forName: .GCControllerDidDisconnect, object: nil, queue: .main
        ) { [weak self] note in
            guard let self, let controller = note.object as? GCController else { return }
            Task { @MainActor in self.detachController(controller) }
        }
        mouseObservers = [mc, md]
        keyboardObservers = [kc, kd]
        controllerObservers = [cc, cd]
    }

    func stopPeripheralMonitoring() {
        setTrackpadEnabled(false)
        for obs in mouseObservers { NotificationCenter.default.removeObserver(obs) }
        for obs in keyboardObservers { NotificationCenter.default.removeObserver(obs) }
        for obs in controllerObservers { NotificationCenter.default.removeObserver(obs) }
        mouseObservers.removeAll()
        keyboardObservers.removeAll()
        controllerObservers.removeAll()
        for mouse in connectedMice.values { detachMouse(mouse) }
        detachKeyboard()
        detachAllControllers()
    }

    private func attachMouse(_ mouse: GCMouse) {
        let wasConnected = !connectedMice.isEmpty
        connectedMice[ObjectIdentifier(mouse)] = mouse
        if !wasConnected {
            physicalMouseConnected = true
            physicalMouseButtonMask = 0
            physicalMouseMotionRemainder = .zero
            physicalMouseScrollRemainderX = 0
            physicalMouseScrollRemainderY = 0
            sendPeripheral(Data([0x01, 0x01])) // PERIPH_MOUSE, ATTACHED
        }

        guard let input = mouse.mouseInput else { return }

        input.mouseMovedHandler = { [weak self] _, dx, dy in
            guard let self else { return }
            Task { @MainActor in self.handlePhysicalMouseMove(dx: dx, dy: dy) }
        }

        input.leftButton.pressedChangedHandler = { [weak self] _, _, pressed in
            guard let self else { return }
            Task { @MainActor in self.updatePhysicalMouseButton(button: 0x01, pressed: pressed) }
        }

        if let right = input.rightButton {
            right.pressedChangedHandler = { [weak self] _, _, pressed in
                guard let self else { return }
                Task { @MainActor in self.updatePhysicalMouseButton(button: 0x02, pressed: pressed) }
            }
        }

        if let middle = input.middleButton {
            middle.pressedChangedHandler = { [weak self] _, _, pressed in
                guard let self else { return }
                Task { @MainActor in self.updatePhysicalMouseButton(button: 0x04, pressed: pressed) }
            }
        }

        // Side buttons: auxiliaryButtons[0] = Back (BTN_SIDE), [1] = Forward (BTN_EXTRA).
        if let aux = input.auxiliaryButtons {
            for (index, button) in aux.prefix(2).enumerated() {
                let mask: UInt8 = 1 << UInt8(3 + index) // 0x08 (Back), 0x10 (Forward)
                button.pressedChangedHandler = { [weak self] _, _, pressed in
                    guard let self else { return }
                    Task { @MainActor in self.updatePhysicalMouseButton(button: mask, pressed: pressed) }
                }
            }
        }

        input.scroll.valueChangedHandler = { [weak self] _, deltaX, deltaY in
            guard let self else { return }
            Task { @MainActor in self.handlePhysicalMouseScroll(deltaX: deltaX, deltaY: deltaY) }
        }

        print("[periph] mouse attached")
    }

    /// Pointer lock and "mouse connected" state end only when the last pointing device is gone;
    /// another device disconnecting must not release the one still in use.
    private func detachMouse(_ mouse: GCMouse) {
        if let input = mouse.mouseInput {
            input.mouseMovedHandler = nil
            input.leftButton.pressedChangedHandler = nil
            input.rightButton?.pressedChangedHandler = nil
            input.middleButton?.pressedChangedHandler = nil
            if let aux = input.auxiliaryButtons {
                for button in aux.prefix(2) {
                    button.pressedChangedHandler = nil
                }
            }
            input.scroll.valueChangedHandler = nil
        }
        guard connectedMice.removeValue(forKey: ObjectIdentifier(mouse)) != nil,
              connectedMice.isEmpty, physicalMouseConnected else { return }
        let previousMask = physicalMouseButtonMask | trackpadButtonMask
        physicalMouseButtonMask = 0
        physicalMouseMotionRemainder = .zero
        physicalMouseScrollRemainderX = 0
        physicalMouseScrollRemainderY = 0
        physicalMouseConnected = false
        sendMouseButtonChanges(from: previousMask)
        if !isTrackpadEnabled {
            sendPeripheral(Data([0x01, 0x00])) // PERIPH_MOUSE, DETACHED
        }
        print("[periph] mouse detached")
    }

    private func handlePhysicalMouseMove(dx: Float, dy: Float) {
        // GCMouse reports deltas in UIKit points (often fractional on slow movement).
        // Scale by the panel's point-to-pixel factor and accumulate sub-pixel remainders
        // so slow movement isn't truncated to 0 and speed matches the high-DPI display.
        let scale = max(UIScreen.main.scale, 1.0)
        physicalMouseMotionRemainder.width += CGFloat(dx) * scale
        physicalMouseMotionRemainder.height += CGFloat(-dy) * scale
        let x = Int16(clamping: Int(physicalMouseMotionRemainder.width.rounded(.towardZero)))
        let y = Int16(clamping: Int(physicalMouseMotionRemainder.height.rounded(.towardZero)))
        physicalMouseMotionRemainder.width -= CGFloat(x)
        physicalMouseMotionRemainder.height -= CGFloat(y)
        guard x != 0 || y != 0 else { return }
        var data = Data([0x01]) // MOUSE_MOVE
        withUnsafeBytes(of: x.bigEndian) { data.append(contentsOf: $0) }
        withUnsafeBytes(of: y.bigEndian) { data.append(contentsOf: $0) }
        sendMouse(data)
    }

    private func updatePhysicalMouseButton(button: UInt8, pressed: Bool) {
        let previousMask = physicalMouseButtonMask | trackpadButtonMask

        if pressed {
            physicalMouseButtonMask |= button
        } else {
            physicalMouseButtonMask &= ~button
        }

        sendMouseButtonChanges(from: previousMask)
    }

    /// The on-screen trackpad and physical mouse share the host's virtual mouse. Merge their
    /// held buttons, so releasing one source doesn't release a button held by the other.
    func sendMouseButtonChanges(from previousMask: UInt8) {
        let currentMask = physicalMouseButtonMask | trackpadButtonMask
        for code: UInt8 in 0..<5 {
            let bit: UInt8 = 1 << code
            if (previousMask & bit) != (currentMask & bit) {
                sendMouse(Data([0x02, code, currentMask & bit != 0 ? 1 : 0]))
            }
        }
    }

    func setTrackpadEnabled(_ enabled: Bool) {
        guard enabled != isTrackpadEnabled else { return }
        if enabled {
            guard isConnected else { return }
            isTrackpadEnabled = true
            if !physicalMouseConnected {
                sendPeripheral(Data([0x01, 0x01]))
            }
        } else {
            let previousMask = physicalMouseButtonMask | trackpadButtonMask
            trackpadButtonMask = 0
            sendMouseButtonChanges(from: previousMask)
            isTrackpadEnabled = false
            if !physicalMouseConnected {
                sendPeripheral(Data([0x01, 0x00]))
            }
        }
        trackpadMotionRemainder = .zero
        trackpadScrollRemainderX = 0
        trackpadScrollRemainderY = 0
    }

    /// On-screen trackpad input is accepted from the floating trackpad when it's enabled, and
    /// always from laptop mode's built-in trackpad, independent of the floating toggle.
    private var acceptsTrackpadInput: Bool {
        isConnected && (isTrackpadEnabled || isExternalControlPanelVisible)
    }

    func moveTrackpad(dx: CGFloat, dy: CGFloat) {
        guard acceptsTrackpadInput else { return }
        trackpadMotionRemainder.width += dx
        trackpadMotionRemainder.height += dy
        let x = Int16(clamping: Int(trackpadMotionRemainder.width))
        let y = Int16(clamping: Int(trackpadMotionRemainder.height))
        trackpadMotionRemainder.width -= CGFloat(x)
        trackpadMotionRemainder.height -= CGFloat(y)
        guard x != 0 || y != 0 else { return }
        var data = Data([0x01])
        withUnsafeBytes(of: x.bigEndian) { data.append(contentsOf: $0) }
        withUnsafeBytes(of: y.bigEndian) { data.append(contentsOf: $0) }
        sendMouse(data)
    }

    func trackpadButton(_ code: UInt8, pressed: Bool) {
        guard acceptsTrackpadInput, code < 5 else { return }
        let previousMask = physicalMouseButtonMask | trackpadButtonMask
        let bit: UInt8 = 1 << code
        if pressed { trackpadButtonMask |= bit } else { trackpadButtonMask &= ~bit }
        sendMouseButtonChanges(from: previousMask)
    }

    /// Sends smooth high-resolution 2D scroll (`0x05`, Linux `REL_WHEEL_HI_RES` /
    /// `REL_HWHEEL_HI_RES` units where `120` = one standard wheel detent).
    private func sendMouseScrollHiRes(dx: Int16, dy: Int16) {
        guard dx != 0 || dy != 0 else { return }
        var data = Data([0x05]) // MOUSE_SCROLL_HIRES
        withUnsafeBytes(of: dx.bigEndian) { data.append(contentsOf: $0) }
        withUnsafeBytes(of: dy.bigEndian) { data.append(contentsOf: $0) }
        sendMouse(data)
    }

    func scrollTrackpad(deltaX: CGFloat, deltaY: CGFloat) {
        guard acceptsTrackpadInput else { return }
        // Natural scrolling: dragging down moves content down (REL_WHEEL > 0);
        // dragging right moves content right (REL_HWHEEL < 0). 18 pt == 1 detent (120 hi-res units).
        let scale: CGFloat = 120.0 / 18.0
        trackpadScrollRemainderX += -deltaX * scale
        trackpadScrollRemainderY += deltaY * scale
        let hx = Int16(clamping: Int(trackpadScrollRemainderX.rounded(.towardZero)))
        let hy = Int16(clamping: Int(trackpadScrollRemainderY.rounded(.towardZero)))
        trackpadScrollRemainderX -= CGFloat(hx)
        trackpadScrollRemainderY -= CGFloat(hy)
        sendMouseScrollHiRes(dx: hx, dy: hy)
    }

    private func handlePhysicalMouseScroll(deltaX: Float, deltaY: Float) {
        // GCMouse reports scroll in detent fractions; convert directly to
        // Linux hi-res wheel units (120 = 1 detent) with sub-unit accumulation.
        physicalMouseScrollRemainderX += deltaX * 120.0
        physicalMouseScrollRemainderY += deltaY * 120.0

        let rawX = Int(physicalMouseScrollRemainderX.rounded(.towardZero))
        let rawY = Int(physicalMouseScrollRemainderY.rounded(.towardZero))
        guard rawX != 0 || rawY != 0 else { return }

        physicalMouseScrollRemainderX -= Float(rawX)
        physicalMouseScrollRemainderY -= Float(rawY)

        let dx = Int16(clamping: max(-3840, min(rawX, 3840)))
        let dy = Int16(clamping: max(-3840, min(rawY, 3840)))
        sendMouseScrollHiRes(dx: dx, dy: dy)
    }

    func forwardPhysicalMouseScroll(deltaX: CGFloat, deltaY: CGFloat) {
        handlePhysicalMouseScroll(deltaX: Float(deltaX), deltaY: Float(deltaY))
    }

    private func attachKeyboard(_ kb: GCKeyboard) {
        physicalKeyboardConnected = true
        sendPeripheral(Data([0x02, 0x01])) // PERIPH_KEYBOARD, ATTACHED

        guard let input = kb.keyboardInput else { return }

        input.keyChangedHandler = { [weak self] _, _, keyCode, pressed in
            guard let self else { return }
            var data = Data()
            let hid = UInt16(keyCode.rawValue).bigEndian
            withUnsafeBytes(of: hid) { data.append(contentsOf: $0) }
            data.append(pressed ? 1 : 0)
            Task { @MainActor in self.sendRawKey(data) }
        }

        print("[periph] keyboard attached")
    }

    private func detachKeyboard() {
        guard physicalKeyboardConnected else { return }
        physicalKeyboardConnected = false
        sendPeripheral(Data([0x02, 0x00])) // PERIPH_KEYBOARD, DETACHED
        print("[periph] keyboard detached")
    }

    private func attachController(_ controller: GCController) {
        guard isControllerPassthroughEnabled else {
            print("[gamepad] controller passthrough disabled, ignoring attach")
            return
        }
        let id = ObjectIdentifier(controller)
        guard controllerSlots[id] == nil else { return }
        guard controller.extendedGamepad != nil || controller.microGamepad != nil else {
            print("[gamepad] unsupported controller profile ignored")
            return
        }

        guard let slot = nextAvailableControllerSlot() else {
            print("[gamepad] no free virtual gamepad slot (max 4)")
            return
        }

        controllerSlots[id] = slot
        physicalControllerConnectedCount = controllerSlots.count
        sendGamepad(Data([slot, 0x01])) // ATTACHED

        if let gamepad = controller.extendedGamepad {
            gamepad.valueChangedHandler = { [weak self, weak controller] gamepad, _ in
                guard let self, let controller else { return }
                Task { @MainActor in self.sendExtendedGamepadState(controller: controller, gamepad: gamepad) }
            }
            sendExtendedGamepadState(controller: controller, gamepad: gamepad)
            print("[gamepad] controller attached in slot \(slot + 1) (extended)")
        } else if let gamepad = controller.microGamepad {
            gamepad.valueChangedHandler = { [weak self, weak controller] gamepad, _ in
                guard let self, let controller else { return }
                Task { @MainActor in self.sendMicroGamepadState(controller: controller, gamepad: gamepad) }
            }
            sendMicroGamepadState(controller: controller, gamepad: gamepad)
            print("[gamepad] controller attached in slot \(slot + 1) (micro)")
        }
    }

    private func detachController(_ controller: GCController) {
        let id = ObjectIdentifier(controller)
        guard let slot = controllerSlots.removeValue(forKey: id) else { return }

        controller.extendedGamepad?.valueChangedHandler = nil
        controller.microGamepad?.valueChangedHandler = nil

        sendGamepad(Data([slot, 0x00])) // DETACHED
        physicalControllerConnectedCount = controllerSlots.count
        print("[gamepad] controller detached from slot \(slot + 1)")
    }

    private func detachAllControllers() {
        for controller in GCController.controllers() {
            controller.extendedGamepad?.valueChangedHandler = nil
            controller.microGamepad?.valueChangedHandler = nil
        }
        for slot in controllerSlots.values {
            sendGamepad(Data([slot, 0x00]))
        }
        controllerSlots.removeAll()
        physicalControllerConnectedCount = 0
    }

    private func refreshControllerPassthrough() {
        if isControllerPassthroughEnabled {
            for controller in GCController.controllers() {
                attachController(controller)
            }
        } else {
            detachAllControllers()
        }
    }

    private func nextAvailableControllerSlot() -> UInt8? {
        let used = Set(controllerSlots.values)
        for slot in 0..<4 {
            let candidate = UInt8(slot)
            if !used.contains(candidate) {
                return candidate
            }
        }
        return nil
    }

    private func sendExtendedGamepadState(controller: GCController, gamepad: GCExtendedGamepad) {
        let id = ObjectIdentifier(controller)
        guard let slot = controllerSlots[id] else { return }

        var buttons: UInt16 = 0
        if gamepad.buttonA.isPressed { buttons |= 0x0001 }
        if gamepad.buttonB.isPressed { buttons |= 0x0002 }
        if gamepad.buttonX.isPressed { buttons |= 0x0004 }
        if gamepad.buttonY.isPressed { buttons |= 0x0008 }
        if gamepad.leftShoulder.isPressed { buttons |= 0x0010 }
        if gamepad.rightShoulder.isPressed { buttons |= 0x0020 }
        if gamepad.leftThumbstickButton?.isPressed == true { buttons |= 0x0040 }
        if gamepad.rightThumbstickButton?.isPressed == true { buttons |= 0x0080 }
        if gamepad.buttonOptions?.isPressed == true { buttons |= 0x0100 }
        if gamepad.buttonMenu.isPressed { buttons |= 0x0200 }
        if gamepad.buttonHome?.isPressed == true { buttons |= 0x0400 }

        let hatX: Int8 = gamepad.dpad.left.isPressed ? -1 : (gamepad.dpad.right.isPressed ? 1 : 0)
        let hatY: Int8 = gamepad.dpad.up.isPressed ? -1 : (gamepad.dpad.down.isPressed ? 1 : 0)

        sendGamepadState(
            slot: slot,
            buttons: buttons,
            lx: gamepad.leftThumbstick.xAxis.value,
            ly: -gamepad.leftThumbstick.yAxis.value,
            rx: gamepad.rightThumbstick.xAxis.value,
            ry: -gamepad.rightThumbstick.yAxis.value,
            lt: gamepad.leftTrigger.value,
            rt: gamepad.rightTrigger.value,
            hatX: hatX,
            hatY: hatY
        )
    }

    private func sendMicroGamepadState(controller: GCController, gamepad: GCMicroGamepad) {
        let id = ObjectIdentifier(controller)
        guard let slot = controllerSlots[id] else { return }

        var buttons: UInt16 = 0
        if gamepad.buttonA.isPressed { buttons |= 0x0001 }
        if gamepad.buttonX.isPressed { buttons |= 0x0004 }
        if gamepad.buttonMenu.isPressed { buttons |= 0x0200 }

        let hatX: Int8 = gamepad.dpad.left.isPressed ? -1 : (gamepad.dpad.right.isPressed ? 1 : 0)
        let hatY: Int8 = gamepad.dpad.up.isPressed ? -1 : (gamepad.dpad.down.isPressed ? 1 : 0)

        sendGamepadState(
            slot: slot,
            buttons: buttons,
            lx: 0,
            ly: 0,
            rx: 0,
            ry: 0,
            lt: 0,
            rt: 0,
            hatX: hatX,
            hatY: hatY
        )
    }

    private func sendGamepadState(
        slot: UInt8,
        buttons: UInt16,
        lx: Float,
        ly: Float,
        rx: Float,
        ry: Float,
        lt: Float,
        rt: Float,
        hatX: Int8,
        hatY: Int8
    ) {
        var data = Data([slot, 0x02]) // STATE
        withUnsafeBytes(of: buttons.bigEndian) { data.append(contentsOf: $0) }
        appendGamepadAxis(&data, lx)
        appendGamepadAxis(&data, ly)
        appendGamepadAxis(&data, rx)
        appendGamepadAxis(&data, ry)
        appendGamepadTrigger(&data, lt)
        appendGamepadTrigger(&data, rt)
        data.append(UInt8(bitPattern: hatX))
        data.append(UInt8(bitPattern: hatY))
        sendGamepad(data)
    }

    private func appendGamepadAxis(_ data: inout Data, _ value: Float) {
        let clamped = max(-1.0, min(1.0, value))
        let scaled = Int16(clamping: Int((clamped * 32767.0).rounded())).bigEndian
        withUnsafeBytes(of: scaled) { data.append(contentsOf: $0) }
    }

    private func appendGamepadTrigger(_ data: inout Data, _ value: Float) {
        let clamped = max(0.0, min(1.0, value))
        let scaled = UInt16((clamped * 1023.0).rounded()).bigEndian
        withUnsafeBytes(of: scaled) { data.append(contentsOf: $0) }
    }

    // MARK: - Camera

    /// True once `CAPS` reports camera support.
    var isCameraAvailable: Bool { daemonCapabilities?.camera ?? false }

    func toggleCamera() {
        guard isCameraAvailable else { return }
        if cameraCapture.isRunning {
            cameraCapture.stop()
            sendCameraDisable()
        } else {
            sendCameraEnable()
            cameraCapture.onJPEG = { [weak self, weak stream = self.stream] jpeg in
                DispatchQueue.main.async {
                    guard let self, let stream, self.stream === stream, self.cameraCapture.isRunning else { return }
                    let fid = self.camFrameId
                    self.camFrameId = self.camFrameId &+ 1
                    stream.sendCameraFrame(jpeg, frameId: fid)
                }
            }
            cameraCapture.start()
        }
        objectWillChange.send()
    }

    /// Tells the daemon to create the virtual webcam with our capture profile.
    private func sendCameraEnable() {
        let profile = cameraCapture.captureProfile
        let width = UInt16(profile.outputSize.width)
        let height = UInt16(profile.outputSize.height)
        let fps = UInt16(profile.targetFps)
        networkControl?.sendCameraConfig(width: width, height: height, fps: fps)
    }

    /// Tells the daemon to destroy the virtual webcam.
    func sendCameraDisable() {
        networkControl?.sendCameraConfig(width: 0, height: 0, fps: 0)
    }

    var isCameraActive: Bool { cameraCapture.isRunning }
    var isCameraFront: Bool { cameraCapture.usingFront }

    func flipCamera() {
        cameraCapture.flipCamera()
        objectWillChange.send()
    }

    // MARK: - Microphone

    /// True once `CAPS` reports microphone support.
    var isMicAvailable: Bool { daemonCapabilities?.microphone ?? false }

    func toggleMic() {
        guard isMicAvailable else { return }
        let speakerWasEnabled = audioPlayer.isOutputEnabled
        if micCapture.isRunning {
            micCapture.stop()
            sendMicDisable()
            configureAudioSession()
            if speakerWasEnabled {
                resetSpeakerOutput()
            }
        } else {
            configureAudioSession(micActive: true)
            sendMicEnable()
            micCapture.onOpusPacket = { [weak self, weak stream = self.stream] opusData in
                DispatchQueue.main.async {
                    guard let self, let stream, self.stream === stream, self.micCapture.isRunning else { return }
                    let seq = self.micSeq
                    self.micSeq = self.micSeq &+ 1
                    stream.sendMicPacket(opusData, seq: seq)
                }
            }
            micCapture.start()
            if !micCapture.isRunning {
                sendMicDisable()
                configureAudioSession()
            }
            if speakerWasEnabled {
                resetSpeakerOutput()
            }
        }
        objectWillChange.send()
    }

    /// Re-synchronizes playback with the daemon by turning the speaker off and back on.
    /// This produces the off-on transition the daemon needs to restart its audio stream.
    private func resetSpeakerOutput() {
        guard audioPlayer.isOutputEnabled else { return }
        audioPlayer.setOutputEnabled(false)
        sendSpeakerTransportState(isEnabled: false)
        audioPlayer.setOutputEnabled(true)
        sendSpeakerTransportState(isEnabled: true)
    }

    var isMicActive: Bool { micCapture.isRunning }

    /// Tells the daemon to create the virtual microphone device.
    private func sendMicEnable() {
        networkControl?.sendMicState(isEnabled: true)
    }

    /// Tells the daemon to destroy the virtual microphone device.
    func sendMicDisable() {
        networkControl?.sendMicState(isEnabled: false)
    }

    // MARK: - Speakers / Controllers

    /// True once `CAPS` reports speaker/system-audio support.
    var isSpeakerAvailable: Bool { daemonCapabilities?.speaker ?? false }

    func toggleSpeaker() {
        guard isSpeakerAvailable else { return }
        let isEnabled = !audioPlayer.isOutputEnabled
        audioPlayer.setOutputEnabled(isEnabled)
        sendSpeakerTransportState(isEnabled: isEnabled)
        objectWillChange.send()
    }

    var isSpeakerActive: Bool { audioPlayer.isOutputEnabled }

    /// True once `CAPS` reports a non-nil `gamepadMaxControllers` (nil there means
    /// gamepad passthrough is unsupported by this daemon).
    var isControllerAvailable: Bool { daemonCapabilities?.gamepadMaxControllers != nil }

    func toggleControllerPassthrough() {
        guard isControllerAvailable else { return }
        isControllerPassthroughEnabled.toggle()
        refreshControllerPassthrough()
    }
}

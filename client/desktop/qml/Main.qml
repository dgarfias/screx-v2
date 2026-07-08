import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import QtQuick.Window
import Screx 1.0

ApplicationWindow {
    id: root
    width: 1280
    height: 800
    minimumWidth: 800
    minimumHeight: 600
    visible: true
    color: "#000000"
    title: AppState.connected ? "Screx - " + AppState.session_title : "Screx"

    function captureStreamKey(event, pressed) {
        event.accepted = false
    }

    function reserveStreamShortcut(event) {
        event.accepted = false
    }

    function uiFont() {
        if (Qt.platform.os === "osx") return "SF Pro Display"
        if (Qt.platform.os === "windows") return "Segoe UI Variable Display"
        return "Noto Sans"
    }

        StackLayout {
        anchors.fill: parent
        currentIndex: AppState.connected ? 1 : 0

        Item {
            Layout.fillWidth: true
            Layout.fillHeight: true

            Rectangle {
                anchors.fill: parent
                color: "#f2f2f7"
            }

            Flickable {
                anchors.centerIn: parent
                width: Math.min(parent.width - 48, 420)
                height: Math.min(contentHeight, parent.height - 48)
                contentHeight: connectionCard.implicitHeight
                clip: true
                interactive: contentHeight > height

                Rectangle {
                    id: connectionCard
                    width: parent.width
                    implicitHeight: cardCol.implicitHeight + 48
                    radius: 16
                    color: "white"
                    border.color: "#e0e0e0"
                    border.width: 1

                    property var allConnections: {
                        try { return JSON.parse(AppState.connections_json) }
                        catch(e) { return [] }
                    }
                    property var pinnedList: allConnections.filter(function(c) { return c.pinned })
                    property var recentList: allConnections.filter(function(c) { return !c.pinned })

                    ColumnLayout {
                        id: cardCol
                        anchors.left: parent.left
                        anchors.right: parent.right
                        anchors.top: parent.top
                        anchors.margins: 24
                        spacing: 16

                        // ── Header ──
                        ColumnLayout {
                            spacing: 4
                            Layout.fillWidth: true

                            Label {
                                text: "Screx"
                                font.family: uiFont()
                                font.pixelSize: 28
                                font.weight: 700
                                color: "#1c1c1e"
                            }

                            Label {
                                text: AppState.connecting ? "Connecting" : "Idle"
                                font.family: uiFont()
                                font.pixelSize: 17
                                font.weight: 600
                                color: "#3a3a3c"
                            }

                            Label {
                                text: AppState.status_text
                                font.family: uiFont()
                                font.pixelSize: 14
                                color: "#8e8e93"
                                wrapMode: Text.WordWrap
                                Layout.fillWidth: true
                            }
                        }

                        // ── Host input + Connect ──
                        RowLayout {
                            Layout.fillWidth: true
                            spacing: 10

                            TextField {
                                id: hostField
                                Layout.fillWidth: true
                                placeholderText: "Daemon host or IP[:port]"
                                font.family: uiFont()
                                font.pixelSize: 16
                                padding: 12
                                color: "#1c1c1e"
                                placeholderTextColor: "#8e8e93"
                                enabled: !AppState.connecting
                                background: Rectangle {
                                    radius: 10
                                    color: "#f2f2f7"
                                    border.color: hostField.activeFocus ? "#007aff" : "#d1d1d6"
                                    border.width: hostField.activeFocus ? 2 : 1
                                }
                                onAccepted: AppState.connect_to_host(text)
                            }

                            Button {
                                text: AppState.connecting ? "Connecting..." : "Connect"
                                enabled: !AppState.connecting && hostField.text.trim().length > 0
                                font.family: uiFont()
                                font.pixelSize: 16
                                font.weight: 600
                                onClicked: AppState.connect_to_host(hostField.text)
                                background: Rectangle {
                                    radius: 10
                                    color: parent.enabled ? (parent.down ? "#0056b3" : "#007aff") : "#b0b0b8"
                                }
                                contentItem: Text {
                                    text: parent.text
                                    color: "white"
                                    font: parent.font
                                    horizontalAlignment: Text.AlignHCenter
                                    verticalAlignment: Text.AlignVCenter
                                }
                                padding: 12
                            }
                        }

                        // ── Stream settings entry point ──
                        // Opened before connecting (not a blocking mid-connection
                        // step) — the chosen resolution/framerate/codec/bitrate
                        // presets are persisted and applied, clamped against the
                        // daemon's advertised CAPS, once a session is established.
                        RowLayout {
                            Layout.fillWidth: true
                            spacing: 6

                            Text {
                                text: "Stream settings: " + streamSettingsPopup.summaryText()
                                color: "#8e8e93"
                                font.family: uiFont()
                                font.pixelSize: 12
                                elide: Text.ElideRight
                                Layout.fillWidth: true
                            }

                            Button {
                                text: "Edit"
                                enabled: !AppState.connecting
                                font.family: uiFont()
                                font.pixelSize: 12
                                font.weight: 600
                                onClicked: streamSettingsPopup.open()
                                background: Rectangle {
                                    radius: 8
                                    color: "transparent"
                                    border.color: "#d1d1d6"
                                    border.width: 1
                                }
                                contentItem: Text {
                                    text: parent.text
                                    color: "#007aff"
                                    font: parent.font
                                    horizontalAlignment: Text.AlignHCenter
                                    verticalAlignment: Text.AlignVCenter
                                }
                                leftPadding: 10; rightPadding: 10
                                topPadding: 4; bottomPadding: 4
                            }
                        }

                        // ── Pinned ──
                        ColumnLayout {
                            Layout.fillWidth: true
                            spacing: 6
                            visible: connectionCard.pinnedList.length > 0

                            Label {
                                text: "PINNED"
                                font.family: uiFont()
                                font.pixelSize: 11
                                font.weight: 700
                                color: "#8e8e93"
                            }

                            Repeater {
                                model: connectionCard.pinnedList
                                delegate: connectionRowDelegate
                            }
                        }

                        // ── Recent ──
                        ColumnLayout {
                            Layout.fillWidth: true
                            spacing: 6
                            visible: connectionCard.recentList.length > 0

                            RowLayout {
                                Layout.fillWidth: true
                                Label {
                                    text: "RECENT"
                                    font.family: uiFont()
                                    font.pixelSize: 11
                                    font.weight: 700
                                    color: "#8e8e93"
                                }
                                Item { Layout.fillWidth: true }
                                Button {
                                    text: "Clear All"
                                    font.family: uiFont()
                                    font.pixelSize: 11
                                    font.weight: 600
                                    onClicked: AppState.clear_recent_connections()
                                    background: Rectangle {
                                        radius: 10
                                        color: parent.down ? "#c0392b" : "#e74c3c"
                                    }
                                    contentItem: Text {
                                        text: parent.text
                                        color: "white"
                                        font: parent.font
                                        horizontalAlignment: Text.AlignHCenter
                                        verticalAlignment: Text.AlignVCenter
                                    }
                                    leftPadding: 10; rightPadding: 10
                                    topPadding: 4; bottomPadding: 4
                                }
                            }

                            Repeater {
                                model: connectionCard.recentList
                                delegate: connectionRowDelegate
                            }
                        }
                    }
                }
            }
        }

            Item {
                Layout.fillWidth: true
                Layout.fillHeight: true

                Component.onCompleted: keyGrabber.forceActiveFocus()
                onVisibleChanged: {
                    if (visible && AppState.connected && AppState.keyboard_enabled)
                        keyGrabber.forceActiveFocus()
                }

                ColumnLayout {
                    anchors.fill: parent
                    spacing: 0

                    // ── Fixed top toolbar ──
                    Rectangle {
                        Layout.fillWidth: true
                        Layout.preferredHeight: 40
                        color: "#1c1c1e"

                        RowLayout {
                            anchors.fill: parent
                            anchors.leftMargin: 12
                            anchors.rightMargin: 12
                            spacing: 6

                            Text {
                                text: AppState.session_title
                                color: "#ffffff"
                                font.family: uiFont()
                                font.pixelSize: 13
                                font.weight: 600
                                Layout.rightMargin: 8
                            }

                            Rectangle { width: 1; Layout.fillHeight: true; Layout.topMargin: 8; Layout.bottomMargin: 8; color: "#3a3a3c" }

                            Repeater {
                                model: [
                                    { label: "Speaker", active: AppState.speaker_enabled, available: AppState.speaker_available, action: function() { AppState.toggle_speaker() } },
                                    { label: "Mic", active: AppState.mic_enabled, available: AppState.mic_available, action: function() { AppState.toggle_mic() } },
                                    { label: "Cam", active: AppState.camera_enabled, available: AppState.camera_available, action: function() {
                                        if (AppState.camera_enabled) {
                                            AppState.toggle_camera()
                                        } else {
                                            camPopup.open()
                                        }
                                    } },
                                    { label: "KB", active: AppState.keyboard_enabled, available: true, action: function() { AppState.toggle_keyboard() } }
                                ]

                                delegate: Button {
                                    text: modelData.label
                                    font.family: uiFont()
                                    font.pixelSize: 12
                                    font.weight: 600
                                    // Daemon-advertised capabilities (CAPS) gate visibility: a
                                    // feature the daemon reported as unavailable simply doesn't
                                    // show a toggle, rather than showing one that silently does
                                    // nothing.
                                    visible: modelData.available
                                    enabled: modelData.available
                                    onClicked: modelData.action()
                                    background: Rectangle {
                                        radius: 4
                                        color: modelData.active ? "#007aff" : "#2c2c2e"
                                    }
                                    contentItem: Text {
                                        text: parent.text
                                        color: modelData.active ? "white" : "#8e8e93"
                                        font: parent.font
                                        horizontalAlignment: Text.AlignHCenter
                                        verticalAlignment: Text.AlignVCenter
                                    }
                                    leftPadding: 10
                                    rightPadding: 10
                                    topPadding: 4
                                    bottomPadding: 4
                                }
                            }

                            Item { Layout.fillWidth: true }

                            Text {
                                text: AppState.resolution_label + "  ·  " + AppState.fps + " fps  ·  " + AppState.bitrate_mbps.toFixed(1) + " Mbps"
                                color: "#8e8e93"
                                font.family: uiFont()
                                font.pixelSize: 11
                            }

                            Rectangle { width: 1; Layout.fillHeight: true; Layout.topMargin: 8; Layout.bottomMargin: 8; color: "#3a3a3c" }

                            Button {
                                text: "Disconnect"
                                font.family: uiFont()
                                font.pixelSize: 12
                                font.weight: 700
                                onClicked: AppState.disconnect_session()
                                background: Rectangle {
                                    radius: 4
                                    color: parent.down ? "#c0392b" : "#e74c3c"
                                }
                                contentItem: Text {
                                    text: parent.text
                                    color: "white"
                                    font: parent.font
                                    horizontalAlignment: Text.AlignHCenter
                                    verticalAlignment: Text.AlignVCenter
                                }
                                leftPadding: 10
                                rightPadding: 10
                                topPadding: 4
                                bottomPadding: 4
                            }
                        }
                    }

                    // ── Stream area ──
                    Item {
                        Layout.fillWidth: true
                        Layout.fillHeight: true

                        Rectangle {
                            anchors.fill: parent
                            color: "#000000"
                        }

                        VideoSurface {
                            id: streamView
                            anchors.fill: parent
                            focus: AppState.connected && AppState.keyboard_enabled

                            function normalizedPoint(px, py) {
                                const cw = content_width
                                const ch = content_height
                                if (cw <= 0 || ch <= 0) {
                                    return null
                                }

                                const nx = (px - content_x) / cw
                                const ny = (py - content_y) / ch
                                return {
                                    x: Math.max(0, Math.min(1, nx)),
                                    y: Math.max(0, Math.min(1, ny))
                                }
                            }

                            MouseArea {
                                id: streamMouse
                                anchors.fill: parent
                                hoverEnabled: AppState.connected
                                acceptedButtons: Qt.LeftButton | Qt.RightButton | Qt.MiddleButton
                                cursorShape: AppState.connected ? Qt.BlankCursor : Qt.ArrowCursor

                                onPositionChanged: function(mouse) {
                                    if (!AppState.connected) return
                                    AppState.send_mouse_move_raw(
                                        mouse.x, mouse.y,
                                        streamView.content_x, streamView.content_y,
                                        streamView.content_width, streamView.content_height
                                    )
                                }

                                onPressed: function(mouse) {
                                    if (!AppState.connected) return
                                    keyGrabber.forceActiveFocus()
                                    AppState.send_mouse_button(mouse.button, true)
                                }

                                onReleased: function(mouse) {
                                    if (!AppState.connected) return
                                    AppState.send_mouse_button(mouse.button, false)
                                }

                                onWheel: function(wheel) {
                                    if (!AppState.connected) return
                                    AppState.send_mouse_scroll(wheel.angleDelta.y)
                                }
                            }
                        }

                        // Timer-driven frame polling: calls VideoSurface.poll_frame()
                        // at ~60Hz while connected. This decouples frame production
                        // (decoder thread) from frame display (Qt render loop) and
                        // prevents frame coalescing from causing stutter.
                        Timer {
                            interval: 16  // ~60 Hz
                            running: AppState.connected
                            repeat: true
                            onTriggered: streamView.poll_frame()
                        }

                        Item {
                            id: keyGrabber
                            anchors.fill: parent
                            focus: AppState.connected && AppState.keyboard_enabled
                            activeFocusOnTab: false

                            Keys.onPressed: {
                                if (!AppState.connected || !AppState.keyboard_enabled) {
                                    event.accepted = false
                                    return
                                }
                                if ((event.modifiers & Qt.ControlModifier) &&
                                    (event.modifiers & Qt.AltModifier) &&
                                    event.key === Qt.Key_G) {
                                    AppState.toggle_keyboard()
                                    event.accepted = true
                                    return
                                }
                                AppState.send_raw_key_event(event.key, true)
                                event.accepted = true
                            }
                            Keys.onReleased: {
                                if (!AppState.connected || !AppState.keyboard_enabled) {
                                    event.accepted = false
                                    return
                                }
                                AppState.send_raw_key_event(event.key, false)
                                event.accepted = true
                            }
                        }
                    }
                }
            }
        }

    // ── Shared connection row delegate (outside StackLayout) ──
    Component {
        id: connectionRowDelegate

        Rectangle {
            Layout.fillWidth: true
            height: 44
            radius: 10
            color: rowMouse.containsMouse ? "#e8e8ed" : "#f2f2f7"

            MouseArea {
                id: rowMouse
                anchors.fill: parent
                hoverEnabled: true
                onClicked: AppState.connect_recent(modelData.host, modelData.port)
            }

            RowLayout {
                anchors.fill: parent
                anchors.leftMargin: 4
                anchors.rightMargin: 8
                spacing: 6

                Button {
                    text: modelData.pinned ? "\u2605" : "\u2606"
                    font.pixelSize: 18
                    z: 1
                    onClicked: AppState.toggle_pinned_connection(modelData.host, modelData.port)
                    background: Rectangle { color: "transparent" }
                    contentItem: Text {
                        text: parent.text
                        color: modelData.pinned ? "#ff9500" : "#c7c7cc"
                        font: parent.font
                        horizontalAlignment: Text.AlignHCenter
                        verticalAlignment: Text.AlignVCenter
                    }
                    implicitWidth: 32
                    implicitHeight: 32
                }

                ColumnLayout {
                    spacing: 1
                    Layout.fillWidth: true
                    Text {
                        text: modelData.name || modelData.host
                        color: "#1c1c1e"
                        font.family: uiFont()
                        font.pixelSize: 14
                        font.weight: 600
                        elide: Text.ElideRight
                        Layout.fillWidth: true
                    }
                    Text {
                        text: modelData.host + (modelData.port !== 42069 ? ":" + modelData.port : "")
                        color: "#8e8e93"
                        font.family: uiFont()
                        font.pixelSize: 11
                    }
                }

                Button {
                    text: "\u2715"
                    font.pixelSize: 16
                    z: 1
                    onClicked: AppState.delete_connection(modelData.host, modelData.port)
                    background: Rectangle { color: "transparent" }
                    contentItem: Text {
                        text: parent.text
                        color: "#ff3b30"
                        font: parent.font
                        horizontalAlignment: Text.AlignHCenter
                        verticalAlignment: Text.AlignVCenter
                    }
                    implicitWidth: 32
                    implicitHeight: 32
                }

                Text {
                    text: "\u203A"
                    font.pixelSize: 18
                    color: "#c7c7cc"
                }
            }
        }
    }

    // ── Camera resolution popup ──
    Popup {
        id: camPopup
        anchors.centerIn: parent
        width: Math.min(root.width - 40, 340)
        height: camPopupContent.implicitHeight + 32
        modal: true
        focus: true
        closePolicy: Popup.CloseOnEscape | Popup.CloseOnPressOutside
        background: Rectangle {
            radius: 12
            color: "#1c1c1e"
            border.color: "#3a3a3c"
            border.width: 1
        }

        ColumnLayout {
            id: camPopupContent
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.top: parent.top
            anchors.margins: 16
            spacing: 12

            Text {
                text: "Select Camera Resolution"
                color: "#ffffff"
                font.family: uiFont()
                font.pixelSize: 16
                font.weight: 700
            }

            Repeater {
                model: [
                    "Auto \u00b7 1280 x 720 @ 30",
                    "720p \u00b7 1280 x 720 @ 60",
                    "1080p \u00b7 1920 x 1080 @ 30",
                    "1080p \u00b7 1920 x 1080 @ 60"
                ]

                delegate: Button {
                    Layout.fillWidth: true
                    text: modelData
                    font.family: uiFont()
                    font.pixelSize: 13
                    font.weight: 600
                    onClicked: {
                        AppState.select_camera_mode(modelData)
                        AppState.toggle_camera()
                        camPopup.close()
                    }
                    background: Rectangle {
                        radius: 6
                        color: parent.down ? "#3a3a3c" : "#2c2c2e"
                    }
                    contentItem: Text {
                        text: parent.text
                        color: "#ffffff"
                        font: parent.font
                        horizontalAlignment: Text.AlignLeft
                        verticalAlignment: Text.AlignVCenter
                    }
                    leftPadding: 12
                    rightPadding: 12
                    topPadding: 8
                    bottomPadding: 8
                }
            }

            Button {
                Layout.fillWidth: true
                text: "Cancel"
                font.family: uiFont()
                font.pixelSize: 13
                onClicked: camPopup.close()
                background: Rectangle {
                    radius: 6
                    color: "transparent"
                    border.color: "#3a3a3c"
                    border.width: 1
                }
                contentItem: Text {
                    text: parent.text
                    color: "#8e8e93"
                    font: parent.font
                    horizontalAlignment: Text.AlignHCenter
                    verticalAlignment: Text.AlignVCenter
                }
                leftPadding: 12
                rightPadding: 12
                topPadding: 6
                bottomPadding: 6
            }
        }
    }

    // ── Stream settings popup ──
    // Styled after camPopup. Shown from the connection card before
    // connecting; "Save" persists the picks (AppState.set_stream_settings)
    // and closes. Each group is a segmented list of presets, highlighting
    // the value currently selected within this open popup session (not
    // applied until Save is pressed).
    Popup {
        id: streamSettingsPopup
        anchors.centerIn: parent
        width: Math.min(root.width - 40, 380)
        height: streamSettingsContent.implicitHeight + 32
        modal: true
        focus: true
        closePolicy: Popup.CloseOnEscape | Popup.CloseOnPressOutside

        property string selResolution: AppState.stream_resolution_choice
        property string selFramerate: AppState.stream_framerate_choice
        property string selCodec: AppState.stream_codec_choice
        property string selBitrate: AppState.stream_bitrate_choice
        property bool customBitrateActive: false
        property string customBitrateMbpsText: ""

        // True for "default" or one of the curated preset bps strings.
        // Anything else is a user-typed custom bitrate.
        function isPresetBitrate(v) {
            return v === "default" || v === "3000000" || v === "8000000"
                || v === "15000000" || v === "20000000"
        }

        // Format a bps string as a trimmed Mbps string ("12500000" -> "12.5",
        // "20000000" -> "20"). Returns "" if not a positive number.
        function formatMbps(bpsStr) {
            var n = parseInt(bpsStr, 10)
            if (isNaN(n) || n <= 0) return ""
            var mbps = Math.round(n / 100000) / 10
            return (mbps % 1 === 0) ? mbps.toFixed(0) : mbps.toFixed(1)
        }

        function summaryText() {
            var res = selResolution === "default" ? "Auto res" : selResolution
            var fps = selFramerate === "default" ? "auto fps" : (selFramerate + " fps")
            return res + ", " + fps
        }

        onOpened: {
            selResolution = AppState.stream_resolution_choice
            selFramerate = AppState.stream_framerate_choice
            selCodec = AppState.stream_codec_choice
            selBitrate = AppState.stream_bitrate_choice

            if (isPresetBitrate(selBitrate)) {
                customBitrateActive = false
                customBitrateMbpsText = ""
            } else {
                var mbpsLabel = formatMbps(selBitrate)
                customBitrateActive = mbpsLabel !== ""
                customBitrateMbpsText = mbpsLabel
            }
        }

        background: Rectangle {
            radius: 12
            color: "#1c1c1e"
            border.color: "#3a3a3c"
            border.width: 1
        }

        ColumnLayout {
            id: streamSettingsContent
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.top: parent.top
            anchors.margins: 16
            spacing: 10

            Text {
                text: "Stream Settings"
                color: "#ffffff"
                font.family: uiFont()
                font.pixelSize: 16
                font.weight: 700
            }

            Text {
                text: "Applied — clamped to what the daemon supports — the next time you connect."
                color: "#8e8e93"
                font.family: uiFont()
                font.pixelSize: 11
                wrapMode: Text.WordWrap
                Layout.fillWidth: true
            }

            // ── Resolution ──
            Text { text: "Resolution"; color: "#8e8e93"; font.family: uiFont(); font.pixelSize: 11; font.weight: 700 }
            Flow {
                Layout.fillWidth: true
                spacing: 6
                Repeater {
                    model: [
                        { label: "Daemon default", value: "default" },
                        { label: "1280 × 720", value: "1280x720" },
                        { label: "1920 × 1080", value: "1920x1080" },
                        { label: "2560 × 1440", value: "2560x1440" },
                        { label: "3840 × 2160", value: "3840x2160" }
                    ]
                    delegate: Button {
                        text: modelData.label
                        font.family: uiFont()
                        font.pixelSize: 12
                        onClicked: streamSettingsPopup.selResolution = modelData.value
                        background: Rectangle {
                            radius: 6
                            color: streamSettingsPopup.selResolution === modelData.value ? "#007aff" : "#2c2c2e"
                        }
                        contentItem: Text {
                            text: parent.text
                            color: "#ffffff"
                            font: parent.font
                            horizontalAlignment: Text.AlignHCenter
                            verticalAlignment: Text.AlignVCenter
                        }
                        leftPadding: 8; rightPadding: 8
                        topPadding: 6; bottomPadding: 6
                    }
                }
            }

            // ── Framerate ──
            Text { text: "Framerate"; color: "#8e8e93"; font.family: uiFont(); font.pixelSize: 11; font.weight: 700 }
            Flow {
                Layout.fillWidth: true
                spacing: 6
                Repeater {
                    model: [
                        { label: "Daemon default", value: "default" },
                        { label: "30 fps", value: "30" },
                        { label: "60 fps", value: "60" }
                    ]
                    delegate: Button {
                        text: modelData.label
                        font.family: uiFont()
                        font.pixelSize: 12
                        onClicked: streamSettingsPopup.selFramerate = modelData.value
                        background: Rectangle {
                            radius: 6
                            color: streamSettingsPopup.selFramerate === modelData.value ? "#007aff" : "#2c2c2e"
                        }
                        contentItem: Text {
                            text: parent.text
                            color: "#ffffff"
                            font: parent.font
                            horizontalAlignment: Text.AlignHCenter
                            verticalAlignment: Text.AlignVCenter
                        }
                        leftPadding: 8; rightPadding: 8
                        topPadding: 6; bottomPadding: 6
                    }
                }
            }

            // ── Codec ──
            Text { text: "Codec"; color: "#8e8e93"; font.family: uiFont(); font.pixelSize: 11; font.weight: 700 }
            Flow {
                Layout.fillWidth: true
                spacing: 6
                Repeater {
                    model: [
                        { label: "Daemon default", value: "default" },
                        { label: "H.264", value: "h264" },
                        { label: "H.265", value: "h265" }
                    ]
                    delegate: Button {
                        text: modelData.label
                        font.family: uiFont()
                        font.pixelSize: 12
                        onClicked: streamSettingsPopup.selCodec = modelData.value
                        background: Rectangle {
                            radius: 6
                            color: streamSettingsPopup.selCodec === modelData.value ? "#007aff" : "#2c2c2e"
                        }
                        contentItem: Text {
                            text: parent.text
                            color: "#ffffff"
                            font: parent.font
                            horizontalAlignment: Text.AlignHCenter
                            verticalAlignment: Text.AlignVCenter
                        }
                        leftPadding: 8; rightPadding: 8
                        topPadding: 6; bottomPadding: 6
                    }
                }
            }

            // ── Bitrate ──
            Text { text: "Bitrate"; color: "#8e8e93"; font.family: uiFont(); font.pixelSize: 11; font.weight: 700 }
            Flow {
                Layout.fillWidth: true
                spacing: 6
                Repeater {
                    model: [
                        { label: "Default", value: "default" },
                        { label: "Low (3 Mbps)", value: "3000000" },
                        { label: "Medium (8 Mbps)", value: "8000000" },
                        { label: "High (15 Mbps)", value: "15000000" },
                        { label: "Very high (20 Mbps)", value: "20000000" }
                    ]
                    delegate: Button {
                        text: modelData.label
                        font.family: uiFont()
                        font.pixelSize: 12
                        onClicked: {
                            streamSettingsPopup.selBitrate = modelData.value
                            streamSettingsPopup.customBitrateActive = false
                        }
                        background: Rectangle {
                            radius: 6
                            color: (!streamSettingsPopup.customBitrateActive && streamSettingsPopup.selBitrate === modelData.value) ? "#007aff" : "#2c2c2e"
                        }
                        contentItem: Text {
                            text: parent.text
                            color: "#ffffff"
                            font: parent.font
                            horizontalAlignment: Text.AlignHCenter
                            verticalAlignment: Text.AlignVCenter
                        }
                        leftPadding: 8; rightPadding: 8
                        topPadding: 6; bottomPadding: 6
                    }
                }
                Button {
                    text: "Custom"
                    font.family: uiFont()
                    font.pixelSize: 12
                    onClicked: streamSettingsPopup.customBitrateActive = true
                    background: Rectangle {
                        radius: 6
                        color: streamSettingsPopup.customBitrateActive ? "#007aff" : "#2c2c2e"
                    }
                    contentItem: Text {
                        text: parent.text
                        color: "#ffffff"
                        font: parent.font
                        horizontalAlignment: Text.AlignHCenter
                        verticalAlignment: Text.AlignVCenter
                    }
                    leftPadding: 8; rightPadding: 8
                    topPadding: 6; bottomPadding: 6
                }
            }

            TextField {
                id: customBitrateField
                visible: streamSettingsPopup.customBitrateActive
                Layout.fillWidth: true
                placeholderText: "Custom Mbps (e.g. 12.5)"
                text: streamSettingsPopup.customBitrateMbpsText
                font.family: uiFont()
                font.pixelSize: 13
                padding: 8
                color: "#ffffff"
                placeholderTextColor: "#8e8e93"
                inputMethodHints: Qt.ImhFormattedNumbersOnly
                validator: DoubleValidator { bottom: 0; decimals: 1; notation: DoubleValidator.StandardNotation }
                onTextChanged: streamSettingsPopup.customBitrateMbpsText = text
                background: Rectangle {
                    radius: 8
                    color: "#2c2c2e"
                    border.color: customBitrateField.activeFocus ? "#007aff" : "#3a3a3c"
                    border.width: customBitrateField.activeFocus ? 2 : 1
                }
            }

            RowLayout {
                Layout.fillWidth: true
                Layout.topMargin: 6
                spacing: 12

                Button {
                    Layout.fillWidth: true
                    text: "Cancel"
                    font.family: uiFont()
                    font.pixelSize: 13
                    onClicked: streamSettingsPopup.close()
                    background: Rectangle {
                        radius: 6
                        color: "transparent"
                        border.color: "#3a3a3c"
                        border.width: 1
                    }
                    contentItem: Text {
                        text: parent.text
                        color: "#8e8e93"
                        font: parent.font
                        horizontalAlignment: Text.AlignHCenter
                        verticalAlignment: Text.AlignVCenter
                    }
                    leftPadding: 12; rightPadding: 12
                    topPadding: 6; bottomPadding: 6
                }

                Button {
                    Layout.fillWidth: true
                    text: "Save"
                    font.family: uiFont()
                    font.pixelSize: 13
                    font.weight: 700
                    onClicked: {
                        var bitrateValue = streamSettingsPopup.selBitrate
                        if (streamSettingsPopup.customBitrateActive) {
                            var mbps = parseFloat(streamSettingsPopup.customBitrateMbpsText)
                            bitrateValue = (!isNaN(mbps) && mbps > 0)
                                ? Math.round(mbps * 1000000).toString()
                                : "default"
                        }
                        AppState.set_stream_settings(
                            streamSettingsPopup.selResolution,
                            streamSettingsPopup.selFramerate,
                            streamSettingsPopup.selCodec,
                            bitrateValue
                        )
                        streamSettingsPopup.close()
                    }
                    background: Rectangle {
                        radius: 6
                        color: "#007aff"
                    }
                    contentItem: Text {
                        text: parent.text
                        color: "white"
                        font: parent.font
                        horizontalAlignment: Text.AlignHCenter
                        verticalAlignment: Text.AlignVCenter
                    }
                    leftPadding: 12; rightPadding: 12
                    topPadding: 6; bottomPadding: 6
                }
            }
        }
    }

    Rectangle {
        anchors.fill: parent
        z: 1000
        visible: AppState.pin_prompt_visible
        color: "#66000000"

        MouseArea {
            anchors.fill: parent
        }

        Rectangle {
            anchors.centerIn: parent
            width: Math.min(root.width - 40, 380)
            radius: 16
            color: "white"
            border.color: "#d1d1d6"
            border.width: 1
            implicitHeight: pinDialogContent.implicitHeight + 40

            ColumnLayout {
                id: pinDialogContent
                anchors.fill: parent
                anchors.margins: 20
                spacing: 16

                Label {
                    text: "Pairing Required"
                    font.family: uiFont()
                    font.pixelSize: 20
                    font.weight: 700
                    color: "#1c1c1e"
                }

                Label {
                    text: AppState.pin_prompt_text
                    wrapMode: Text.WordWrap
                    Layout.fillWidth: true
                    font.family: uiFont()
                    font.pixelSize: 14
                    color: "#636366"
                }

                TextField {
                    id: pinDialogField
                    Layout.fillWidth: true
                    placeholderText: "000000"
                    font.family: uiFont()
                    font.pixelSize: 28
                    font.weight: 700
                    color: "#1c1c1e"
                    placeholderTextColor: "#8e8e93"
                    horizontalAlignment: Text.AlignHCenter
                    inputMethodHints: Qt.ImhDigitsOnly
                    maximumLength: 6
                    onVisibleChanged: {
                        if (visible) {
                            text = ""
                            forceActiveFocus()
                        }
                    }
                    onAccepted: {
                        if (text.length === 6)
                            AppState.submit_pin(text)
                    }
                }

                RowLayout {
                    Layout.fillWidth: true
                    spacing: 12

                    Button {
                        text: "Cancel"
                        Layout.fillWidth: true
                        font.family: uiFont()
                        font.pixelSize: 15
                        onClicked: AppState.disconnect_session()
                    }

                    Button {
                        text: AppState.connecting ? "Pairing..." : "Pair"
                        Layout.fillWidth: true
                        font.family: uiFont()
                        font.pixelSize: 15
                        font.weight: 600
                        enabled: pinDialogField.text.length === 6 && !AppState.connecting
                        onClicked: AppState.submit_pin(pinDialogField.text)
                        background: Rectangle {
                            radius: 8
                            color: parent.enabled ? (parent.down ? "#0056b3" : "#007aff") : "#b0b0b8"
                        }
                        contentItem: Text {
                            text: parent.text
                            color: "white"
                            font: parent.font
                            horizontalAlignment: Text.AlignHCenter
                            verticalAlignment: Text.AlignVCenter
                        }
                    }
                }
            }
        }
    }
}

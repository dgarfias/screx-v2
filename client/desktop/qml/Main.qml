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
                                    { label: "Speaker", active: AppState.speaker_enabled, action: function() { AppState.toggle_speaker() } },
                                    { label: "Mic", active: AppState.mic_enabled, action: function() { AppState.toggle_mic() } },
                                    { label: "Cam", active: AppState.camera_enabled, action: function() {
                                        if (AppState.camera_enabled) {
                                            AppState.toggle_camera()
                                        } else {
                                            camPopup.open()
                                        }
                                    } },
                                    { label: "KB", active: AppState.keyboard_enabled, action: function() { AppState.toggle_keyboard() } }
                                ]

                                delegate: Button {
                                    text: modelData.label
                                    font.family: uiFont()
                                    font.pixelSize: 12
                                    font.weight: 600
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

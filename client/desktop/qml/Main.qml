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
    title: appState.connected ? "Screx — " + appState.session_title : "Screx"
    color: "#000000"

    function uiFont() {
        if (Qt.platform.os === "osx") return "SF Pro Display"
        if (Qt.platform.os === "windows") return "Segoe UI Variable Display"
        return "Noto Sans"
    }

    StackLayout {
        anchors.fill: parent
        currentIndex: appState.connected ? 1 : 0

        // ──────────────────────────────────────────────────
        // DISCONNECTED — simple centered card like iPad
        // ──────────────────────────────────────────────────
        Item {
            anchors.fill: parent

            Rectangle {
                anchors.fill: parent
                color: "#f2f2f7"
            }

            Rectangle {
                id: connectionCard
                anchors.centerIn: parent
                width: Math.min(parent.width - 48, 420)
                radius: 16
                color: "white"
                border.color: "#e0e0e0"
                border.width: 1

                // Let the card height be driven by content
                implicitHeight: cardContent.implicitHeight + 48

                ColumnLayout {
                    id: cardContent
                    anchors.left: parent.left
                    anchors.right: parent.right
                    anchors.top: parent.top
                    anchors.margins: 24
                    spacing: 16

                    // Title
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
                            text: appState.connecting ? "Connecting" : "Idle"
                            font.family: uiFont()
                            font.pixelSize: 17
                            font.weight: 600
                            color: "#3a3a3c"
                        }

                        Label {
                            text: appState.status_text
                            font.family: uiFont()
                            font.pixelSize: 14
                            color: "#8e8e93"
                            wrapMode: Text.WordWrap
                            Layout.fillWidth: true
                        }
                    }

                    // Host field + Connect button
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
                            enabled: !appState.connecting
                            background: Rectangle {
                                radius: 10
                                color: "#f2f2f7"
                                border.color: hostField.activeFocus ? "#007aff" : "#d1d1d6"
                                border.width: hostField.activeFocus ? 2 : 1
                            }
                            onAccepted: appState.connect_to_host(text)
                        }

                        Button {
                            text: appState.connecting ? "Connecting…" : "Connect"
                            font.family: uiFont()
                            font.pixelSize: 16
                            font.weight: 600
                            enabled: !appState.connecting && hostField.text.trim().length > 0
                            onClicked: appState.connect_to_host(hostField.text)
                            background: Rectangle {
                                radius: 10
                                color: parent.enabled
                                    ? (parent.down ? "#0056b3" : "#007aff")
                                    : "#b0b0b8"
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

                    // PIN entry — shown only when pairing is required
                    Rectangle {
                        visible: appState.pin_prompt_text.length > 0
                        Layout.fillWidth: true
                        implicitHeight: pinColumn.implicitHeight + 32
                        radius: 12
                        color: "#fff8f0"
                        border.color: "#f0c78a"
                        border.width: 1

                        ColumnLayout {
                            id: pinColumn
                            anchors.left: parent.left
                            anchors.right: parent.right
                            anchors.top: parent.top
                            anchors.margins: 16
                            spacing: 12

                            Label {
                                text: "Pairing Required"
                                font.family: uiFont()
                                font.pixelSize: 17
                                font.weight: 700
                                color: "#1c1c1e"
                            }

                            Label {
                                text: appState.pin_prompt_text
                                wrapMode: Text.WordWrap
                                Layout.fillWidth: true
                                font.family: uiFont()
                                font.pixelSize: 14
                                color: "#636366"
                            }

                            RowLayout {
                                Layout.fillWidth: true
                                spacing: 10

                                TextField {
                                    id: pinField
                                    Layout.fillWidth: true
                                    placeholderText: "000000"
                                    font.family: uiFont()
                                    font.pixelSize: 24
                                    font.weight: 700
                                    horizontalAlignment: Text.AlignHCenter
                                    inputMethodHints: Qt.ImhDigitsOnly
                                    maximumLength: 6
                                    padding: 10
                                    onAccepted: {
                                        if (text.length === 6) appState.submit_pin(text)
                                    }
                                    background: Rectangle {
                                        radius: 10
                                        color: "white"
                                        border.color: pinField.activeFocus ? "#007aff" : "#d1d1d6"
                                        border.width: pinField.activeFocus ? 2 : 1
                                    }
                                }

                                Button {
                                    text: "Pair"
                                    font.family: uiFont()
                                    font.pixelSize: 16
                                    font.weight: 600
                                    enabled: pinField.text.length === 6 && !appState.connecting
                                    onClicked: appState.submit_pin(pinField.text)
                                    background: Rectangle {
                                        radius: 10
                                        color: parent.enabled
                                            ? (parent.down ? "#0056b3" : "#007aff")
                                            : "#b0b0b8"
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
                        }
                    }
                }
            }
        }

        // ──────────────────────────────────────────────────
        // CONNECTED — full streaming surface + overlays
        // ──────────────────────────────────────────────────
        Item {
            anchors.fill: parent

            Rectangle {
                anchors.fill: parent
                color: "#000000"
            }

            VideoSurface {
                id: streamView
                anchors.fill: parent
                focus: appState.connected && appState.keyboard_enabled

                Timer {
                    running: appState.connected
                    repeat: true
                    interval: 16
                    onTriggered: streamView.update()
                }

                MouseArea {
                    id: mouseArea
                    anchors.fill: parent
                    hoverEnabled: true
                    acceptedButtons: Qt.LeftButton | Qt.RightButton | Qt.MiddleButton

                    onPositionChanged: function(mouse) {
                        if (!appState.connected) return
                        var nx = mouse.x / width
                        var ny = mouse.y / height
                        appState.send_mouse_move(nx, ny)
                    }

                    onPressed: function(mouse) {
                        if (!appState.connected) return
                        appState.send_mouse_button(mouse.button, true)
                    }

                    onReleased: function(mouse) {
                        if (!appState.connected) return
                        appState.send_mouse_button(mouse.button, false)
                    }

                    onWheel: function(wheel) {
                        if (!appState.connected) return
                        appState.send_mouse_scroll(wheel.angleDelta.y)
                    }
                }
            }

            // Keyboard input capture
            Item {
                id: keyGrabber
                anchors.fill: parent
                focus: appState.connected && appState.keyboard_enabled

                Keys.onPressed: function(event) {
                    if (!appState.connected || !appState.keyboard_enabled) return
                    appState.send_key_event(event.key, true)
                    event.accepted = true
                }

                Keys.onReleased: function(event) {
                    if (!appState.connected || !appState.keyboard_enabled) return
                    appState.send_key_event(event.key, false)
                    event.accepted = true
                }
            }

            // Info overlay — top left
            Rectangle {
                visible: appState.info_visible
                anchors.left: parent.left
                anchors.top: parent.top
                anchors.margins: 16
                width: 260
                radius: 14
                color: "#cc1c1c1e"
                implicitHeight: infoCol.implicitHeight + 24

                Column {
                    id: infoCol
                    anchors.fill: parent
                    anchors.margins: 12
                    spacing: 6

                    Text {
                        text: appState.session_title
                        color: "#ffffff"
                        font.family: uiFont()
                        font.pixelSize: 16
                        font.weight: 650
                    }

                    Text {
                        text: appState.transport_label + "  ·  " + appState.codec_label
                        color: "#aeaeb2"
                        font.family: uiFont()
                        font.pixelSize: 12
                    }

                    Rectangle { width: parent.width; height: 1; color: "#3a3a3c" }

                    Text {
                        text: appState.resolution_label
                        color: "#ffffff"
                        font.family: uiFont()
                        font.pixelSize: 13
                    }

                    Text {
                        text: appState.fps + " fps  ·  " + appState.latency_ms + " ms  ·  " + appState.bitrate_mbps.toFixed(1) + " Mbps"
                        color: "#ffffff"
                        font.family: uiFont()
                        font.pixelSize: 13
                    }

                    Text {
                        text: "Dropped: " + appState.dropped_frames
                        color: appState.dropped_frames > 0 ? "#ff6961" : "#aeaeb2"
                        font.family: uiFont()
                        font.pixelSize: 12
                    }
                }
            }

            // Top center pill toolbar
            Rectangle {
                id: toolbar
                anchors.top: parent.top
                anchors.horizontalCenter: parent.horizontalCenter
                anchors.topMargin: 12
                implicitWidth: toolbarRow.implicitWidth + 20
                implicitHeight: toolbarRow.implicitHeight + 12
                radius: 22
                color: "#cc1c1c1e"

                RowLayout {
                    id: toolbarRow
                    anchors.centerIn: parent
                    spacing: 6

                    Repeater {
                        model: [
                            { label: "Speaker", active: appState.speaker_enabled, action: function() { appState.toggle_speaker() } },
                            { label: "Mic", active: appState.mic_enabled, action: function() { appState.toggle_mic() } },
                            { label: "Camera", active: appState.camera_enabled, action: function() { appState.toggle_camera() } },
                            { label: "Keyboard", active: appState.keyboard_enabled, action: function() { appState.toggle_keyboard() } },
                            { label: "Info", active: appState.info_visible, action: function() { appState.toggle_info() } }
                        ]

                        delegate: Button {
                            text: modelData.label
                            font.family: uiFont()
                            font.pixelSize: 13
                            font.weight: 600
                            onClicked: modelData.action()
                            background: Rectangle {
                                radius: 16
                                color: modelData.active ? "#007aff" : "#2c2c2e"
                            }
                            contentItem: Text {
                                text: parent.text
                                color: modelData.active ? "white" : "#aeaeb2"
                                font: parent.font
                                horizontalAlignment: Text.AlignHCenter
                                verticalAlignment: Text.AlignVCenter
                            }
                            leftPadding: 14
                            rightPadding: 14
                            topPadding: 7
                            bottomPadding: 7
                        }
                    }

                    // Camera mode — only on connected screen
                    ComboBox {
                        id: activeModeBox
                        model: [
                            "Auto · 1280 x 720 @ 30",
                            "720p · 1280 x 720 @ 60",
                            "1080p · 1920 x 1080 @ 30",
                            "1080p · 1920 x 1080 @ 60"
                        ]
                        currentIndex: Math.max(0, model.indexOf(appState.selected_camera_mode))
                        font.family: uiFont()
                        font.pixelSize: 12
                        Layout.preferredWidth: 210
                        onActivated: appState.select_camera_mode(currentText)
                    }

                    Button {
                        text: "Disconnect"
                        font.family: uiFont()
                        font.pixelSize: 13
                        font.weight: 700
                        onClicked: appState.disconnect_session()
                        background: Rectangle {
                            radius: 16
                            color: parent.down ? "#c0392b" : "#e74c3c"
                        }
                        contentItem: Text {
                            text: parent.text
                            color: "white"
                            font: parent.font
                            horizontalAlignment: Text.AlignHCenter
                            verticalAlignment: Text.AlignVCenter
                        }
                        leftPadding: 14
                        rightPadding: 14
                        topPadding: 7
                        bottomPadding: 7
                    }
                }
            }

            // Bottom-left status
            Text {
                anchors.left: parent.left
                anchors.bottom: parent.bottom
                anchors.margins: 16
                text: appState.status_text
                color: "#8e8e93"
                font.family: uiFont()
                font.pixelSize: 13
            }
        }
    }
}

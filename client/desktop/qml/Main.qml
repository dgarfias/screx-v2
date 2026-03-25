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

            Rectangle {
                anchors.centerIn: parent
                width: Math.min(parent.width - 48, 420)
                implicitHeight: cardContent.implicitHeight + 48
                radius: 16
                color: "white"
                border.color: "#e0e0e0"
                border.width: 1

                ColumnLayout {
                    id: cardContent
                    anchors.fill: parent
                    anchors.margins: 24
                    spacing: 16

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
                }
            }
        }

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

                Timer {
                    running: AppState.connected
                    repeat: true
                    interval: 16
                    onTriggered: streamView.update()
                }

                MouseArea {
                    anchors.fill: parent
                    hoverEnabled: true
                    acceptedButtons: Qt.LeftButton | Qt.RightButton | Qt.MiddleButton

                    onPositionChanged: function(mouse) {
                        if (!AppState.connected) return
                        AppState.send_mouse_move(mouse.x / width, mouse.y / height)
                    }

                    onPressed: function(mouse) {
                        if (!AppState.connected) return
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

            Item {
                anchors.fill: parent
                focus: AppState.connected && AppState.keyboard_enabled

                Keys.onPressed: function(event) {
                    if (!AppState.connected || !AppState.keyboard_enabled) return
                    AppState.send_key_event(event.key, true)
                    event.accepted = true
                }

                Keys.onReleased: function(event) {
                    if (!AppState.connected || !AppState.keyboard_enabled) return
                    AppState.send_key_event(event.key, false)
                    event.accepted = true
                }
            }

            Rectangle {
                visible: AppState.info_visible
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
                        text: AppState.session_title
                        color: "#ffffff"
                        font.family: uiFont()
                        font.pixelSize: 16
                        font.weight: 650
                    }

                    Text {
                        text: AppState.transport_label + "  ·  " + AppState.codec_label
                        color: "#aeaeb2"
                        font.family: uiFont()
                        font.pixelSize: 12
                    }

                    Rectangle { width: parent.width; height: 1; color: "#3a3a3c" }

                    Text {
                        text: AppState.resolution_label
                        color: "#ffffff"
                        font.family: uiFont()
                        font.pixelSize: 13
                    }

                    Text {
                        text: AppState.fps + " fps  ·  " + AppState.latency_ms + " ms  ·  " + AppState.bitrate_mbps.toFixed(1) + " Mbps"
                        color: "#ffffff"
                        font.family: uiFont()
                        font.pixelSize: 13
                    }

                    Text {
                        text: "Dropped: " + AppState.dropped_frames
                        color: AppState.dropped_frames > 0 ? "#ff6961" : "#aeaeb2"
                        font.family: uiFont()
                        font.pixelSize: 12
                    }
                }
            }

            Rectangle {
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
                            { label: "Speaker", active: AppState.speaker_enabled, action: function() { AppState.toggle_speaker() } },
                            { label: "Mic", active: AppState.mic_enabled, action: function() { AppState.toggle_mic() } },
                            { label: "Camera", active: AppState.camera_enabled, action: function() { AppState.toggle_camera() } },
                            { label: "Keyboard", active: AppState.keyboard_enabled, action: function() { AppState.toggle_keyboard() } },
                            { label: "Info", active: AppState.info_visible, action: function() { AppState.toggle_info() } }
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

                    ComboBox {
                        model: [
                            "Auto · 1280 x 720 @ 30",
                            "720p · 1280 x 720 @ 60",
                            "1080p · 1920 x 1080 @ 30",
                            "1080p · 1920 x 1080 @ 60"
                        ]
                        currentIndex: Math.max(0, model.indexOf(AppState.selected_camera_mode))
                        font.family: uiFont()
                        font.pixelSize: 12
                        Layout.preferredWidth: 210
                        onActivated: AppState.select_camera_mode(currentText)
                    }

                    Button {
                        text: "Disconnect"
                        font.family: uiFont()
                        font.pixelSize: 13
                        font.weight: 700
                        onClicked: AppState.disconnect_session()
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

            Text {
                anchors.left: parent.left
                anchors.bottom: parent.bottom
                anchors.margins: 16
                text: AppState.status_text
                color: "#8e8e93"
                font.family: uiFont()
                font.pixelSize: 13
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

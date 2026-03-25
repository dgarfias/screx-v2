import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import QtQuick.Window
import Screx 1.0

ApplicationWindow {
    id: root
    width: 1480
    height: 920
    minimumWidth: 980
    minimumHeight: 680
    visible: true
    title: appState.connected ? "Screx Desktop - " + appState.session_title : "Screx Desktop"
    color: disconnectedPalette.base

    QtObject {
        id: disconnectedPalette
        property color base: "#f4efe5"
        property color ink: "#172033"
        property color accent: "#c86935"
        property color accentSoft: "#e6a77a"
        property color card: "#fffaf2"
        property color line: "#d9c7b3"
    }

    QtObject {
        id: connectedPalette
        property color base: "#09111c"
        property color surface: "#0d1825"
        property color surfaceSoft: "#142538"
        property color line: "#274159"
        property color ink: "#ebf2f7"
        property color muted: "#93a9bd"
        property color accent: "#6fd1c5"
        property color accentWarm: "#ff9a5c"
    }

    FontLoader { id: interopFont }

    function uiFont() {
        if (Qt.platform.os === "osx") return "SF Pro Display"
        if (Qt.platform.os === "windows") return "Segoe UI Variable Display"
        return "Noto Sans"
    }

    background: Rectangle {
        gradient: Gradient {
            GradientStop {
                position: 0.0
                color: appState.connected ? "#09111c" : "#f6f0e7"
            }
            GradientStop {
                position: 0.65
                color: appState.connected ? "#0e1b2a" : "#efe0cf"
            }
            GradientStop {
                position: 1.0
                color: appState.connected ? "#122132" : "#f5ede2"
            }
        }

        Rectangle {
            anchors.top: parent.top
            anchors.right: parent.right
            width: parent.width * 0.42
            height: parent.height * 0.35
            radius: width / 2
            color: appState.connected ? "#1a3958" : "#f4c7a2"
            opacity: appState.connected ? 0.22 : 0.34
            x: parent.width * 0.66
            y: -height * 0.35
        }

        Rectangle {
            anchors.bottom: parent.bottom
            anchors.left: parent.left
            width: parent.width * 0.34
            height: parent.height * 0.26
            radius: width / 2
            color: appState.connected ? "#0f6d83" : "#df8c57"
            opacity: appState.connected ? 0.14 : 0.18
            x: -width * 0.18
            y: parent.height - height * 0.55
        }
    }

    StackLayout {
        anchors.fill: parent
        currentIndex: appState.connected ? 1 : 0

        Item {
            anchors.fill: parent

            ColumnLayout {
                anchors.centerIn: parent
                width: Math.min(parent.width * 0.78, 1060)
                spacing: 26

                ColumnLayout {
                    Layout.alignment: Qt.AlignHCenter
                    spacing: 8

                    Label {
                        text: "Screx Desktop"
                        font.family: uiFont()
                        font.pixelSize: 50
                        font.weight: 700
                        color: disconnectedPalette.ink
                        Layout.alignment: Qt.AlignHCenter
                    }

                    Label {
                        text: "Low-latency Linux display control with a real desktop shell."
                        font.family: uiFont()
                        font.pixelSize: 18
                        color: "#4f6178"
                        Layout.alignment: Qt.AlignHCenter
                    }
                }

                Rectangle {
                    Layout.fillWidth: true
                    Layout.preferredHeight: 410
                    radius: 30
                    color: disconnectedPalette.card
                    border.color: disconnectedPalette.line
                    border.width: 1

                    RowLayout {
                        anchors.fill: parent
                        anchors.margins: 28
                        spacing: 30

                        ColumnLayout {
                            Layout.fillWidth: true
                            Layout.fillHeight: true
                            spacing: 18

                            Label {
                                text: "Connect"
                                font.family: uiFont()
                                font.pixelSize: 30
                                font.weight: 650
                                color: disconnectedPalette.ink
                            }

                            Label {
                                text: "Start with a network host, then switch into a dedicated control surface with a top pill toolbar."
                                wrapMode: Text.WordWrap
                                Layout.fillWidth: true
                                font.family: uiFont()
                                font.pixelSize: 15
                                lineHeight: 1.35
                                color: "#5a6a7c"
                            }

                            TextField {
                                id: hostField
                                Layout.fillWidth: true
                                placeholderText: "Hostname or IP"
                                font.family: uiFont()
                                font.pixelSize: 18
                                padding: 16
                                background: Rectangle {
                                    radius: 18
                                    color: "#fffdf9"
                                    border.color: hostField.activeFocus ? disconnectedPalette.accent : disconnectedPalette.line
                                    border.width: hostField.activeFocus ? 2 : 1
                                }
                                onAccepted: appState.connect_to_host(text)
                            }

                            Rectangle {
                                visible: appState.pin_prompt_text.length > 0
                                Layout.fillWidth: true
                                radius: 18
                                color: "#fff4e8"
                                border.color: "#e3b590"
                                border.width: 1

                                ColumnLayout {
                                    anchors.fill: parent
                                    anchors.margins: 16
                                    spacing: 10

                                    Label {
                                        text: "Pairing PIN"
                                        font.family: uiFont()
                                        font.pixelSize: 16
                                        font.weight: 700
                                        color: disconnectedPalette.ink
                                    }

                                    Label {
                                        text: appState.pin_prompt_text
                                        wrapMode: Text.WordWrap
                                        Layout.fillWidth: true
                                        font.family: uiFont()
                                        font.pixelSize: 14
                                        color: "#5d6673"
                                    }

                                    RowLayout {
                                        Layout.fillWidth: true
                                        spacing: 10

                                        TextField {
                                            id: pinField
                                            Layout.preferredWidth: 180
                                            placeholderText: "6-digit PIN"
                                            font.family: uiFont()
                                            font.pixelSize: 18
                                            inputMethodHints: Qt.ImhDigitsOnly
                                            maximumLength: 6
                                            onAccepted: appState.submit_pin(text)
                                            background: Rectangle {
                                                radius: 14
                                                color: "white"
                                                border.color: pinField.activeFocus ? disconnectedPalette.accent : disconnectedPalette.line
                                                border.width: pinField.activeFocus ? 2 : 1
                                            }
                                        }

                                        Button {
                                            text: appState.connecting ? "Submitting..." : "Submit PIN"
                                            font.family: uiFont()
                                            font.pixelSize: 15
                                            enabled: pinField.text.length === 6 && !appState.connecting
                                            onClicked: appState.submit_pin(pinField.text)
                                            background: Rectangle {
                                                radius: 14
                                                color: parent.enabled ? disconnectedPalette.accent : "#c7b39f"
                                            }
                                            contentItem: Text {
                                                text: parent.text
                                                color: "white"
                                                horizontalAlignment: Text.AlignHCenter
                                                verticalAlignment: Text.AlignVCenter
                                                font.family: uiFont()
                                                font.pixelSize: 15
                                                font.weight: 600
                                            }
                                        }
                                    }
                                }
                            }

                            RowLayout {
                                Layout.fillWidth: true
                                spacing: 12

                                Button {
                                    Layout.fillWidth: true
                                    text: appState.connecting ? "Connecting..." : "Connect"
                                    font.family: uiFont()
                                    font.pixelSize: 17
                                    enabled: !appState.connecting
                                    onClicked: appState.connect_to_host(hostField.text)
                                    background: Rectangle {
                                        radius: 18
                                        color: parent.enabled
                                            ? (parent.down ? "#af5322" : disconnectedPalette.accent)
                                            : "#c7b39f"
                                    }
                                    contentItem: Text {
                                        text: parent.text
                                        color: "white"
                                        horizontalAlignment: Text.AlignHCenter
                                        verticalAlignment: Text.AlignVCenter
                                        font.family: uiFont()
                                        font.pixelSize: 17
                                        font.weight: 600
                                    }
                                }

                                ComboBox {
                                    id: cameraModeBox
                                    Layout.preferredWidth: 280
                                    model: [
                                        "Auto · 1280 x 720 @ 30",
                                        "Balanced · 1920 x 1080 @ 30",
                                        "Sharp · 1920 x 1080 @ 60"
                                    ]
                                    font.family: uiFont()
                                    font.pixelSize: 15
                                    onActivated: appState.select_camera_mode(currentText)
                                }
                            }

                            Label {
                                text: appState.status_text
                                Layout.fillWidth: true
                                wrapMode: Text.WordWrap
                                font.family: uiFont()
                                font.pixelSize: 14
                                color: "#66768a"
                            }
                        }

                        Rectangle {
                            Layout.preferredWidth: 330
                            Layout.fillHeight: true
                            radius: 24
                            color: "#162234"

                            ColumnLayout {
                                anchors.fill: parent
                                anchors.margins: 24
                                spacing: 14

                                Label {
                                    text: "Session targets"
                                    font.family: uiFont()
                                    font.pixelSize: 22
                                    font.weight: 650
                                    color: "#f1f4f8"
                                }

                                Repeater {
                                    model: [
                                        ["Network only", "No USB transport for desktop sessions."],
                                        ["Absolute mouse", "Desktop pointer maps directly to the host display."],
                                        ["Hardware video", "Rust media core will feed a real Qt Quick render path."],
                                        ["Camera modes", "Client-selected webcam resolution and fps via CAMCFG."]
                                    ]

                                    delegate: Rectangle {
                                        Layout.fillWidth: true
                                        implicitHeight: 72
                                        radius: 18
                                        color: "#1e3048"
                                        border.color: "#2d4765"

                                        Column {
                                            anchors.fill: parent
                                            anchors.margins: 14
                                            spacing: 4

                                            Text {
                                                text: modelData[0]
                                                color: "#ffffff"
                                                font.family: uiFont()
                                                font.pixelSize: 15
                                                font.weight: 600
                                            }

                                            Text {
                                                text: modelData[1]
                                                color: "#9cb0c3"
                                                font.family: uiFont()
                                                font.pixelSize: 13
                                                wrapMode: Text.WordWrap
                                                width: parent.width
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Item {
            anchors.fill: parent

            Rectangle {
                id: videoSurface
                anchors.fill: parent
                anchors.margins: 18
                radius: 26
                color: connectedPalette.surface
                border.color: connectedPalette.line
                border.width: 1

                Rectangle {
                    anchors.fill: parent
                    anchors.margins: 10
                    radius: 22
                    gradient: Gradient {
                        GradientStop { position: 0.0; color: "#13253a" }
                        GradientStop { position: 0.45; color: "#0b1521" }
                        GradientStop { position: 1.0; color: "#08111b" }
                    }
                }

                VideoSurface {
                    id: streamView
                    anchors.fill: parent
                    anchors.margins: 4
                    focus: appState.connected && appState.keyboard_enabled

                    // Repaint at display rate while connected.
                    Timer {
                        running: appState.connected
                        repeat: true
                        interval: 16
                        onTriggered: streamView.update()
                    }

                    // Mouse area for absolute pointer + buttons + scroll.
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

                // Keyboard input capture — an invisible Item that grabs key focus.
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

                Rectangle {
                    visible: appState.info_visible
                    anchors.left: parent.left
                    anchors.top: parent.top
                    anchors.margins: 28
                    width: 270
                    radius: 20
                    color: "#102133"
                    opacity: 0.94
                    border.color: "#29455f"

                    Column {
                        anchors.fill: parent
                        anchors.margins: 18
                        spacing: 8

                        Text {
                            text: appState.session_title
                            color: connectedPalette.ink
                            font.family: uiFont()
                            font.pixelSize: 18
                            font.weight: 650
                        }

                        Text {
                            text: appState.transport_label + "  |  " + appState.codec_label
                            color: connectedPalette.muted
                            font.family: uiFont()
                            font.pixelSize: 13
                        }

                        Rectangle { width: parent.width; height: 1; color: "#274159" }

                        Text {
                            text: "Resolution: " + appState.resolution_label
                            color: connectedPalette.ink
                            font.family: uiFont()
                            font.pixelSize: 14
                        }

                        Text {
                            text: "FPS: " + appState.fps + "   RTT: " + appState.latency_ms + " ms"
                            color: connectedPalette.ink
                            font.family: uiFont()
                            font.pixelSize: 14
                        }

                        Text {
                            text: "Bitrate: " + appState.bitrate_mbps.toFixed(1) + " Mbps"
                            color: connectedPalette.ink
                            font.family: uiFont()
                            font.pixelSize: 14
                        }

                        Text {
                            text: "Dropped: " + appState.dropped_frames
                            color: connectedPalette.ink
                            font.family: uiFont()
                            font.pixelSize: 14
                        }
                    }
                }

                Rectangle {
                    anchors.top: parent.top
                    anchors.horizontalCenter: parent.horizontalCenter
                    anchors.topMargin: 24
                    radius: 24
                    color: "#102133"
                    opacity: 0.97
                    border.color: "#28435f"

                    RowLayout {
                        anchors.fill: parent
                        anchors.margins: 10
                        spacing: 8

                        function pillFill(active, warm) {
                            if (!active)
                                return "#15293d"
                            return warm ? connectedPalette.accentWarm : connectedPalette.accent
                        }

                        function pillText(active) {
                            return active ? "#08111b" : connectedPalette.ink
                        }

                        Repeater {
                            model: [
                                ["Speaker", appState.speaker_enabled, function() { appState.toggle_speaker() }, false],
                                ["Mic", appState.mic_enabled, function() { appState.toggle_mic() }, true],
                                ["Camera", appState.camera_enabled, function() { appState.toggle_camera() }, true],
                                ["Keyboard", appState.keyboard_enabled, function() { appState.toggle_keyboard() }, false],
                                ["Info", appState.info_visible, function() { appState.toggle_info() }, false]
                            ]

                            delegate: Button {
                                text: modelData[0]
                                font.family: uiFont()
                                font.pixelSize: 14
                                onClicked: modelData[2]()
                                background: Rectangle {
                                    radius: 18
                                    color: parent.down
                                        ? "#d6844f"
                                        : parent.RowLayout.pillFill(modelData[1], modelData[3])
                                }
                                contentItem: Text {
                                    text: parent.text
                                    color: parent.RowLayout.pillText(modelData[1])
                                    font.family: uiFont()
                                    font.pixelSize: 14
                                    font.weight: 600
                                    horizontalAlignment: Text.AlignHCenter
                                    verticalAlignment: Text.AlignVCenter
                                }
                            }
                        }

                        ComboBox {
                            id: activeModeBox
                            model: [
                                "Auto · 1280 x 720 @ 30",
                                "Balanced · 1920 x 1080 @ 30",
                                "Sharp · 1920 x 1080 @ 60"
                            ]
                            currentIndex: Math.max(0, model.indexOf(appState.selected_camera_mode))
                            font.family: uiFont()
                            font.pixelSize: 13
                            Layout.preferredWidth: 240
                            onActivated: appState.select_camera_mode(currentText)
                        }

                        Button {
                            text: "Disconnect"
                            font.family: uiFont()
                            font.pixelSize: 14
                            onClicked: appState.disconnect_session()
                            background: Rectangle {
                                radius: 18
                                color: parent.down ? "#c75f3b" : "#e3744d"
                            }
                            contentItem: Text {
                                text: parent.text
                                color: "white"
                                font.family: uiFont()
                                font.pixelSize: 14
                                font.weight: 700
                                horizontalAlignment: Text.AlignHCenter
                                verticalAlignment: Text.AlignVCenter
                            }
                        }
                    }
                }

                Text {
                    anchors.left: parent.left
                    anchors.bottom: parent.bottom
                    anchors.margins: 30
                    text: appState.status_text
                    color: connectedPalette.muted
                    font.family: uiFont()
                    font.pixelSize: 14
                }
            }
        }
    }
}

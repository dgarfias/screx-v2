# Screx

Screx turns a Linux, Windows, or macOS machine into a low-latency remote display host for iPad and desktop clients.

You run a daemon that creates a virtual monitor, then connect from either the iPad app or the desktop client over Wi-Fi (or USB, for the iPad app). Input and peripheral forwarding depends on the client, daemon platform, and installed host drivers.

For implementation details and protocol documentation, see [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Daemon comparison

| Feature | Linux daemon | Windows daemon | macOS daemon |
|---|---|---|---|
| Virtual display and capture | Yes | Yes | Yes |
| USB transport for iPad | Yes | Yes | Yes |
| Keyboard and mouse | Yes | Yes | Yes |
| Touch input | Yes | Yes | Yes, translated to pointer and scroll gestures |
| Game controllers | Yes | Yes | No |
| Client speaker audio | Yes | Yes | Yes |
| Client microphone | Yes | Yes, with VB-Audio VB-CABLE | No |
| Client camera | Yes | Yes | No |

See the [Linux](docs/DAEMON_LINUX.md), [Windows](docs/DAEMON_WINDOWS.md), and [macOS](docs/DAEMON_MACOS.md) daemon guides for setup requirements.

## Client comparison

| Feature | Native iPad client | Desktop client |
|---|---|---|
| USB transport | Yes | No |
| H.264 and H.265 playback | Yes | Yes, with platform hardware acceleration where available |
| Touch forwarding | Yes | No |
| Mouse forwarding | Yes, including an external iPad pointer | Yes |
| Keyboard forwarding | Yes, software and external | Focused application input |
| Game controller forwarding | Yes | No |
| Speaker playback | Yes | Yes |
| Microphone forwarding | Yes | Yes |
| Camera forwarding | Yes | Yes |

## Daemons

- [`docs/DAEMON_LINUX.md`](docs/DAEMON_LINUX.md) — build and run the Linux daemon
- [`docs/DAEMON_WINDOWS.md`](docs/DAEMON_WINDOWS.md) — build and run the Windows daemon
- [`docs/DAEMON_MACOS.md`](docs/DAEMON_MACOS.md) — build and run the macOS daemon

## Clients

- [`docs/CLIENT_IPAD.md`](docs/CLIENT_IPAD.md) — build and run the native iPad app (`client/ipad`)
- [`docs/CLIENT_DESKTOP.md`](docs/CLIENT_DESKTOP.md) — build and run the desktop client for macOS, Windows, and Linux (`client/desktop`)

Either client works over the network with any daemon platform. USB transport is available only to the iPad client.

## Use

1. Start the daemon (Linux, Windows, or macOS).
2. Open Screx on the iPad or launch the desktop client on macOS, Windows, or Linux.
3. Choose one transport:
   - **Network**: enter the host/IP and tap `Connect`
   - **USB** (iPad only): tap `Connect via USB`
4. If it is the first network connection, enter the PIN shown by the daemon.
5. On Linux hosts, enable the `Screx Virtual` display in GNOME Settings if the virtual monitor is not already active.

The clients remember recent and pinned network targets, and stream settings (resolution, framerate, codec, and bitrate) are chosen from the connect screen before connecting. In-session controls expose keyboard, audio, camera, controller, and connection features supported by the active client and daemon.

# Screx

Screx turns a Linux or Windows machine into a low-latency remote display host for iPad and desktop clients.

You run a daemon that creates a virtual monitor, then connect from either the iPad app or the desktop client over Wi-Fi (or USB, for the iPad app). The clients can also forward touch, keyboard, mouse, controllers, microphone, speakers, and camera.

For implementation details and protocol documentation, see [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Daemons

- [`docs/DAEMON_LINUX.md`](docs/DAEMON_LINUX.md) — build and run the Linux daemon
- [`docs/DAEMON_WINDOWS.md`](docs/DAEMON_WINDOWS.md) — build and run the Windows daemon

## Clients

- [`docs/CLIENT_IPAD.md`](docs/CLIENT_IPAD.md) — build and run the native iPad app (`client/ipad`)
- [`docs/CLIENT_DESKTOP.md`](docs/CLIENT_DESKTOP.md) — build and run the desktop client for macOS, Windows, and Linux (`client/desktop`)

Either client works with either daemon — the wire protocol is identical on both host platforms.

## Use

1. Start the daemon (Linux or Windows).
2. Open Screx on the iPad or launch the desktop client on macOS, Windows, or Linux.
3. Choose one transport:
    - **Network**: enter the host/IP and tap `Connect`
    - **USB** (iPad only): tap `Connect via USB`
4. If it is the first network connection, enter the PIN shown by the daemon.
5. On Linux hosts, enable the `Screx Virtual` display in GNOME Settings if the virtual monitor is not already active.

The clients remember recent and pinned network targets, and stream settings (resolution, framerate, codec, and bitrate) are chosen from the connect screen before connecting. The in-session controls give access to keyboard, audio, camera, controllers, and connection info.

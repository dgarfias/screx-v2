# Screx

Screx turns a Linux machine into a low-latency remote display host for the native iPad client.

You run a daemon that creates a virtual monitor, then connect from the iPad app over Wi-Fi. Input
and peripheral forwarding depends on the installed host drivers.

For implementation details and protocol documentation, see [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Daemon

The Linux daemon creates a virtual display and captures, encodes, and streams it to the client. It
also:

- injects touch, keyboard, and mouse input
- exposes a virtual microphone and webcam
- forwards client speaker audio

See [`docs/DAEMON_LINUX.md`](docs/DAEMON_LINUX.md) for build and run instructions.

## Client

The native iPad app connects to the daemon, decodes the video/audio stream, and forwards touch,
keyboard, mouse, microphone, and camera input back to the host.

See [`docs/CLIENT_IPAD.md`](docs/CLIENT_IPAD.md) — build and run the native iPad app (`client/ipad`)

## Use

1. Start the daemon.
2. Open Screx on the iPad.
3. Enter the host/IP and tap `Connect`.
4. If it is the first connection, enter the PIN shown by the daemon.
5. Enable the `Screx Virtual` display in GNOME Settings if the virtual monitor is not already active.

The client remembers recent and pinned network targets, and stream settings (resolution, framerate,
codec, and bitrate) are chosen from the connect screen before connecting. In-session controls expose
keyboard, audio, camera, and connection features supported by the daemon.

# Screx

Screx turns a Linux machine into a low-latency wireless display for an iPad.

You run a daemon on the Linux host that creates a virtual monitor, then connect to it from the iPad app over Wi-Fi. The iPad forwards touch, keyboard, mouse, game controller, microphone, and camera input back to the host, and plays the host's audio.

For implementation details and protocol documentation, see [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Components

- [`docs/DAEMON_LINUX.md`](docs/DAEMON_LINUX.md): build and run the Linux daemon (`daemon/`)
- [`docs/CLIENT_IPAD.md`](docs/CLIENT_IPAD.md): build and run the iPad app (`client/ipad`)

## Features

- Virtual display (EVDI) with H.264/H.265 hardware or software encoding
- Touch, keyboard, and mouse input
- Game controllers (up to 4, via `uinput`)
- Host speaker audio played on the iPad
- iPad microphone exposed as a virtual microphone on the host
- iPad camera exposed as a virtual webcam (`v4l2loopback`) on the host

## Use

1. Start the daemon on the Linux host.
2. Open Screx on the iPad, enter the host/IP, and tap `Connect`.
3. If it is the first connection, enter the PIN shown by the daemon.
4. Enable the `Screx Virtual` display in GNOME Settings if the virtual monitor is not already active.

The app remembers recent and pinned hosts. Pick the stream settings (resolution, framerate, codec, and bitrate) on the connect screen before connecting. While a session is running, you can turn the keyboard, audio, camera, controller, and connection features on or off.

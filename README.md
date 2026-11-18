# Screx

Screx turns a Linux machine into a low-latency network display for an iPad.

You run a daemon on the Linux host that creates a virtual monitor, then connect to it from the iPad app over the network. The iPad forwards touch, keyboard, mouse, game controller, microphone, and camera input back to the host, and plays the host's audio.

For implementation details and protocol documentation, see [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Components

- [`docs/DAEMON_LINUX.md`](docs/DAEMON_LINUX.md): build and run the Linux daemon (`daemon/`)
- [`docs/CLIENT_IPAD.md`](docs/CLIENT_IPAD.md): build and run the iPad app (`client`)

## Features

- Virtual display (EVDI) with H.264/H.265 hardware or software encoding
- Touch, keyboard, and mouse input
- Game controllers (up to 4, via `uinput`)
- Host speaker audio played on the iPad
- iPad microphone exposed as a virtual microphone on the host
- iPad camera exposed as a virtual webcam (`v4l2loopback`) on the host
- Speaker and microphone devices use the bundled `screx_snd` ALSA kernel driver, without selecting a user's audio session
- External displays connected to the iPad: Internal only, External only, or Both as two independent Linux monitors
- Laptop mode for External only: a drawn laptop keyboard with the trackpad below it

## Use

Tested working on GNOME, KDE Plasma, Cinnamon and Hyprland. It may also work on other desktop environments.

1. Start the daemon on the Linux host.
2. Open Screx on the iPad, tap **+**, enter the host/IP, and tap `Connect`.
3. If it is the first connection, enter the eight-digit PIN shown by the daemon.
4. Enable the virtual display in GNOME Settings if it is not already active. It is named after the iPad model (e.g. `iPad A16 11"`); a second monitor appears as `iPad External`.

Each host you connect to becomes a tile with its own settings (address, resolution, framerate, codec, and bitrate) under **⋯ → Settings…**, and its own pairing. While a session is running, you can turn the keyboard, audio, camera, controller, and connection features on or off.

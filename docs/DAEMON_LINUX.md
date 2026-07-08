# Linux Daemon

Creates a virtual monitor via EVDI, captures and encodes it, and streams it to the
[iPad app](CLIENT_IPAD.md) or [desktop client](CLIENT_DESKTOP.md) over Wi-Fi or USB. Also injects
touch/keyboard/mouse/controller input and exposes virtual microphone, webcam, and speaker devices.
Source lives in `daemon/`. See [ARCHITECTURE.md](ARCHITECTURE.md) for the protocol.

## Requirements

### Arch Linux

```bash
# Build
sudo pacman -S --needed \
  rust pkgconf clang \
  ffmpeg libva mesa \
  linux-headers

# Runtime
sudo pacman -S --needed \
  libpulse \
  pipewire-pulse \
  pipewire \
  libimobiledevice \
  libusbmuxd \
  v4l2loopback-dkms \
  systemd

# EVDI virtual display
yay -S evdi-git
```

### Ubuntu / Debian

```bash
# Build
sudo apt-get install -y \
  cargo pkg-config clang \
  libavcodec-dev libavformat-dev libavfilter-dev libavutil-dev libswscale-dev libswresample-dev \
  libva-dev mesa-va-drivers va-driver-all \
  linux-headers-$(uname -r)

# Runtime
sudo apt-get install -y \
  pulseaudio-utils \
  pipewire-pulse \
  pipewire-bin \
  libimobiledevice-utils \
  libusbmuxd-tools \
  v4l2loopback-dkms \
  evdi-dkms libevdi1 \
  udev
```

`v4l2loopback` is loaded automatically via `modprobe` the first time the virtual webcam is used —
having `v4l2loopback-dkms` installed is enough, no manual module load needed.

## Build

```bash
cd daemon
cargo build --release
```

Output: `daemon/target/release/screx`

## Run

```bash
cd daemon

# Basic
sudo ./target/release/screx

# Cap sessions at 1080p/60fps, H.264 via VA-API
sudo ./target/release/screx -w 1920 -H 1080 -f 60 -e vaapi -c h264

# H.265 with NVENC, 10 Mbps bitrate ceiling
sudo ./target/release/screx --codec h265 --backend nvenc --max-bitrate 10M

# Network only
sudo ./target/release/screx --network-only

# USB only
sudo ./target/release/screx --usb-only

# Try the virtual webcam without exclusive caps
sudo ./target/release/screx --no-camera-exclusive-caps

# List or remove pairings
sudo ./target/release/screx unpair
sudo ./target/release/screx unpair <device_id>
sudo ./target/release/screx unpair --all
```

`sudo` is required because the daemon creates and manages virtual display (EVDI) and input
(`uinput`) devices.

| Flag | Default | Description |
|---|---|---|
| `-w, --max-width` | `3840` | Maximum display width clients may request (also the default when a client doesn't ask) |
| `-H, --max-height` | `2160` | Maximum display height clients may request (also the default when a client doesn't ask) |
| `-f, --max-framerate` | `60` | Maximum framerate clients may request (also the default when a client doesn't ask) |
| `-k, --keyframe` | `90` | Keyframe interval (frames) |
| `-b, --max-bitrate` | `20M` | Maximum encoder bitrate clients may request (also the default when a client doesn't ask); e.g. `20000000`, `20M`, `500K` |
| `--max-bitrate-usb` | `100M` | Maximum encoder bitrate USB-connected clients may request (ceiling; USB links have far more headroom than typical networks) |
| `-p, --port` | `9000` | UDP/TCP streaming port |
| `-e, --backend` | `auto` | Encoder backend: `auto`, `vaapi`, `nvenc`, `software` |
| `-c, --codec` | `h264` | Default video codec: `h264`, `h265` (clients may request either, if the daemon can encode it) |
| `-v, --verbose` | off | Detailed diagnostic logs |
| `--network-only` | off | Disable USB transport |
| `--usb-only` | off | Disable network pairing and UDP streaming |
| `--no-camera-exclusive-caps` | off | Disable v4l2loopback exclusive capture caps for better app compatibility |

`--max-width`/`--max-height`/`--max-framerate`/`--max-bitrate`/`--max-bitrate-usb` are per-daemon
ceilings and defaults, not fixed values every session gets: connecting clients may propose any
resolution, framerate, codec, or bitrate at or below these bounds during connection, and the
daemon starts that session with the negotiated values. `--max-bitrate-usb` applies only to
sessions negotiated over the USB transport; all other ceilings are shared across transports. See
[ARCHITECTURE.md](ARCHITECTURE.md#capability-negotiation) for the negotiation protocol.

## Use

1. Start the daemon.
2. Open Screx on the iPad or launch the desktop client.
3. Choose one transport:
    - **Network**: enter the host/IP and tap `Connect`
    - **USB**: tap `Connect via USB`
4. If it is the first network connection, enter the PIN shown by the daemon.
5. Enable the `Screx Virtual` display in GNOME Settings if the virtual monitor is not already
   active.

The virtual webcam uses `v4l2loopback`; some applications behave differently depending on
`exclusive_caps` mode — see the
[ArchWiki's v4l2loopback troubleshooting notes](https://wiki.archlinux.org/title/V4l2loopback#Troubleshooting)
for compatibility context.

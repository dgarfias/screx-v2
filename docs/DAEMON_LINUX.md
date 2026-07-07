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

# 1080p, 60 fps, H.264 via VA-API
sudo ./target/release/screx -w 1920 -H 1080 -f 60 -b vaapi -c h264

# H.265 with NVENC
sudo ./target/release/screx --codec h265 --backend nvenc --bitrate 10M

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
| `-w, --width` | `2160` | Virtual display width |
| `-H, --height` | `1620` | Virtual display height |
| `-f, --framerate` | `30` | Target framerate |
| `-k, --keyframe` | `90` | Keyframe interval (frames) |
| `-b, --bitrate` | `8M` | Encoder bitrate (e.g. `8000000`, `8M`, `500K`) |
| `-p, --port` | `9000` | UDP/TCP streaming port |
| `-e, --backend` | `auto` | Encoder backend: `auto`, `vaapi`, `nvenc`, `software` |
| `-c, --codec` | `h264` | Video codec: `h264`, `h265` |
| `-v, --verbose` | off | Detailed diagnostic logs |
| `--network-only` | off | Disable USB transport |
| `--usb-only` | off | Disable network pairing and UDP streaming |
| `--no-camera-exclusive-caps` | off | Disable v4l2loopback exclusive capture caps for better app compatibility |

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

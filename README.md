# Screx

Screx turns an iPad into a low-latency remote display for a Linux machine.

You run a Linux daemon that creates a virtual monitor, then connect from the iPad app over Wi‑Fi or USB. The app can also forward touch, keyboard, mouse, controllers, microphone, speakers, and camera.

For implementation details and protocol documentation, see [`ARCHITECTURE.md`](ARCHITECTURE.md).

## Dependencies

### Arch Linux

```bash
# Build dependencies
sudo pacman -S --needed \
  rust pkgconf clang \
  ffmpeg libva mesa \
  linux-headers

# Runtime dependencies
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
# Build dependencies
sudo apt-get install -y \
  cargo pkg-config clang \
  libavcodec-dev libavformat-dev libavfilter-dev libavutil-dev libswscale-dev libswresample-dev \
  libva-dev mesa-va-drivers va-driver-all \
  linux-headers-$(uname -r)

# Runtime dependencies
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

## Build

### Linux daemon

```bash
cd daemon
cargo build --release
```

### iPad app

Open `client/ipad/Screx.xcodeproj` in Xcode and build it to an iPad running iPadOS 16 or later.

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

# Host readiness checks
sudo ./target/release/screx doctor

# List or remove pairings
sudo ./target/release/screx unpair
sudo ./target/release/screx unpair <device_id>
sudo ./target/release/screx unpair --all
```

The daemon needs `sudo` because it creates and manages virtual display and input devices.

## Use

1. Start the daemon on Linux.
2. Open the Screx app on the iPad.
3. Choose one transport:
   - **Network**: enter the Linux host/IP and tap `Connect`
   - **USB**: tap `Connect via USB`
4. If it is the first network connection, enter the PIN shown by the daemon.
5. Enable the `Screx Virtual` display in GNOME Settings if the virtual monitor is not already active.

The app remembers recent and pinned network targets, and the in-session toolbar gives access to keyboard, audio, camera, controllers, and connection info.

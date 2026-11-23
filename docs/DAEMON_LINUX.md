# Linux Daemon

Creates a virtual monitor via EVDI, captures and encodes it, and streams it over the network to the
[iPad app](CLIENT_IPAD.md). Also injects
touch/keyboard/mouse/controller input and exposes virtual microphone, webcam, and speaker devices.
Source lives in `daemon/`. See [ARCHITECTURE.md](ARCHITECTURE.md) for the protocol.

## Requirements

### Arch Linux

```bash
# Build
sudo pacman -S --needed \
  rust pkgconf clang \
  ffmpeg libva mesa opus \
  linux-headers

# Runtime
sudo pacman -S --needed \
  dkms \
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
  libva-dev mesa-va-drivers va-driver-all libopus-dev \
  linux-headers-$(uname -r)

# Runtime
sudo apt-get install -y \
  dkms \
  v4l2loopback-dkms \
  evdi-dkms libevdi1 \
  udev
```

`v4l2loopback` is loaded automatically via `modprobe` the first time the virtual webcam is used —
having `v4l2loopback-dkms` installed is enough, no manual module load needed.

### Speaker and microphone driver

Install the bundled `screx_snd` ALSA driver before using speaker or microphone forwarding.
Matching kernel headers and DKMS are required. From the repository root:

```bash
sudo install -d /usr/src/screx_snd-0.1.0
sudo install -m644 kernel/screx_snd/{Makefile,dkms.conf,screx_snd.c,screx_snd_uapi.h} /usr/src/screx_snd-0.1.0/
sudo dkms install /usr/src/screx_snd-0.1.0
```

The daemon loads the module on demand. Speaker and microphone toggles create real ALSA
cards named after the iPad; disabling them or disconnecting removes the cards. The daemon
does not connect to a user's audio server. PipeWire or PulseAudio can manage these cards
like other hardware. The speaker and microphone threads keep the cards' 10 ms clocks and ask for
real-time scheduling (`SCHED_FIFO`, priority 10); without the privilege they run at normal
priority and `-v` logs that.

Tested working on GNOME, KDE Plasma, Cinnamon and Hyprland. Other desktop environments may
also work. Touchscreen-to-monitor mapping depends on the desktop; any manual mapping belongs
in the desktop's configuration, not the daemon.

## Build

```bash
cd daemon
cargo build --release
```

Output: `daemon/target/release/screx`

The build compiles Monocypher's Elligator2 map from `shared/` for CPace pairing, so build from a
checkout of the whole repository.

### Tests

```bash
cd daemon
cargo test
```

They cover the published CPace vectors, wrong PINs, low-order shares, pairing persistence, pinned
TLS, reconnects with a saved identity, busy and same-device takeover, both streams, 1022-byte
datagrams, and (when `/dev/dri/renderD128` is available) VA-API VPP and DRM PRIME zero-copy
capture/cursor-blend encoding.

To check interoperability with Apple's QUIC stack, run the ignored test on this host:

```bash
cargo test quic_transport::tests::apple_interop_server -- --exact --ignored --nocapture
```

It listens on UDP 19001 for 90 seconds and prints a test PIN. Then run the smoke test on a Mac
(see [CLIENT_IPAD.md](CLIENT_IPAD.md#tests)) and type that PIN. It uses temporary identity
files, not the daemon's real pairings.

## Run

```bash
cd daemon

# Basic
sudo ./target/release/screx

# Cap sessions at 1080p/60fps, H.264 via VA-API
sudo ./target/release/screx -w 1920 -H 1080 -f 60 -e vaapi -c h264

# H.265 with NVENC, 10 Mbps bitrate ceiling
sudo ./target/release/screx --codec h265 --backend nvenc --max-bitrate 10M

# No audio forwarding and no controllers
sudo ./target/release/screx --no-speaker --no-gamepad

# Try the virtual webcam without exclusive caps
sudo ./target/release/screx --no-camera-exclusive-caps

# List pairings, then remove one by the short ID from the list (e.g. a3f9c), or all of them
sudo ./target/release/screx unpair
sudo ./target/release/screx unpair a3f9c
sudo ./target/release/screx unpair --all
```

`sudo` is required because the daemon creates and manages virtual display (EVDI) and input
(`uinput`) devices.

| Flag | Default | Description |
|---|---|---|
| `-w, --max-width` | `3840` | Maximum display width clients may request (also the default when a client doesn't ask) |
| `-H, --max-height` | `2160` | Maximum display height clients may request (also the default when a client doesn't ask) |
| `-f, --max-framerate` | `60` | Maximum framerate clients may request (also the default when a client doesn't ask) |
| `-k, --keyframe` | `0` | Keyframe interval in frames (`0` = PLI-only / infinite GOP, recommended) |
| `--intra-refresh` | on | Replace periodic IDR keyframes with rolling intra-refresh where the encoder supports it (software H.264, NVENC); full IDRs are then only emitted on client PLI. Use `--intra-refresh false` to keep periodic keyframes |
| `-b, --max-bitrate` | `20M` | Maximum encoder bitrate clients may request (also the default when a client doesn't ask); e.g. `20000000`, `20M`, `500K` |
| `-p, --port` | `9000` | QUIC port (UDP); open UDP, not TCP, in the firewall |
| `--no-vpp` | off | Convert captured frames to NV12 on the CPU instead of the GPU's VA-API video processor |
| `--no-zero-copy` | off | Disable DRM PRIME DMA-BUF zero-copy capture and fall back to CPU pixel readback via `evdi_grab_pixels` (also disabled by `SCREX_ZEROCOPY=0`) |
| `--vaapi-async-depth` | `1` | Frames the VA-API encoder may work on at once (1–8). Higher improves throughput at the cost of up to that many frames of latency |
| `-e, --backend` | `auto` | Encoder backend: `auto`, `vaapi`, `nvenc`, `software` |
| `-c, --codec` | `h264` | Default video codec: `h264`, `h265` (clients may request either, if the daemon can encode it) |
| `-v, --verbose` | off | More logging. Default output covers startup, pairing, connects/disconnects, monitor changes, warnings and errors. `-v` adds connection, device and pipeline details plus per-second capture stats; `-vv` adds per-packet diagnostics |
| `--no-camera-exclusive-caps` | off | Disable v4l2loopback exclusive capture caps for better app compatibility |
| `--no-speaker` | off | Disable forwarding host audio to the iPad |
| `--no-mic` | off | Disable the virtual microphone fed by the iPad mic |
| `--no-camera` | off | Disable the virtual webcam fed by the iPad camera |
| `--no-gamepad` | off | Disable game controller passthrough |

`--max-width`/`--max-height`/`--max-framerate`/`--max-bitrate` are per-daemon
ceilings and defaults, not fixed values every session gets: connecting clients may propose any
resolution, framerate, codec, or bitrate at or below these bounds during connection, and the
daemon starts that session with the negotiated values. See
[ARCHITECTURE.md](ARCHITECTURE.md#capability-negotiation) for the negotiation protocol.

`--no-speaker`/`--no-mic`/`--no-camera`/`--no-gamepad` switch off optional features. The daemon
reports them as unavailable to the iPad, which greys out the matching controls, and ignores any
request to turn them on.

## Use

1. Start the daemon.
2. Open Screx on the iPad, enter the host/IP, and tap `Connect`.
3. If it is the first connection, enter the eight-digit PIN the daemon prints (shown as
   `1234-5678`). It allows one attempt and expires after 120 seconds; connecting again prints a
   new one. Pairings are stored in `quic-pairings-v1.json` and the daemon's TLS identity in
   `quic-identity-v1.json`, under `/root/.config/screx` when run with `sudo`.
4. Enable the iPad-named display in GNOME Settings if the virtual monitor is not already
   active.

### Black screen on GNOME 51 (mutter 51)

mutter 51 picks a 10-bit (AR30) framebuffer for EVDI outputs, which EVDI can't display, so the
iPad stays black. The journal shows `Page flip failed: drmModeAtomicCommit: Invalid argument` from
gnome-shell, and with DRM debug enabled the kernel logs `invalid pixel format AR30`. See
[mutter#5100](https://gitlab.gnome.org/GNOME/mutter/-/work_items/5100) and
[evdi#597](https://github.com/DisplayLink/evdi/issues/597). Until a fixed mutter ships, work around
it and reboot:

```bash
echo 'MUTTER_DEBUG_USE_FBOS=0' | sudo tee /etc/environment.d/90-mutter-evdi.conf
```

If that isn't enough, `MUTTER_DEBUG_MULTI_GPU_FORCE_COPY_MODE=primary-gpu-cpu` is the other reported
workaround.

The virtual webcam uses `v4l2loopback`; some applications behave differently depending on
`exclusive_caps` mode — see the
[ArchWiki's v4l2loopback troubleshooting notes](https://wiki.archlinux.org/title/V4l2loopback#Troubleshooting)
for compatibility context.

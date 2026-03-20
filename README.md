# Screx v2

Low-latency Linux-to-iPad screen streaming. Turns an iPad into a virtual second display for your Linux desktop.

The daemon creates a virtual monitor via EVDI, captures and encodes its framebuffer with VA-API H.264, and streams video + audio to the iPad app over WiFi (UDP with FEC) or USB (TCP via iproxy). The iPad decodes with VideoToolbox hardware acceleration and displays with AVSampleBufferDisplayLayer.

## Features

- **Virtual second display** via EVDI kernel module — appears as a real monitor in GNOME
- **Hardware-accelerated H.264 encoding** via VA-API (`h264_vaapi` through ffmpeg)
- **Dual transport backends**:
  - **WiFi**: UDP with Reed-Solomon FEC, chunked and paced for reliability
  - **USB**: TCP over iproxy/usbmuxd — zero packet loss, lower latency
  - Automatic detection and failover (USB preferred when connected)
- **Audio streaming**: Virtual PulseAudio/PipeWire sink ("Screx iPad") captured via `parec`, streamed alongside video
- **Auto-discovery**: UDP broadcast beacon (no mDNS dependency), iPad auto-connects within seconds
- **Disconnect detection**: Data timeouts and beacon monitoring for automatic reconnection

## Architecture

```
┌─────────────────── Linux Daemon ───────────────────┐
│                                                     │
│  EVDI ──► VA-API H.264 ──► Transport Router ──┬──► UDP (WiFi)
│                                                │
│  parec (audio) ──────────────► Transport Router ┴──► TCP (USB)
│                                                     │
│  Beacon broadcaster (port 9999)                     │
└─────────────────────────────────────────────────────┘
          │ WiFi (UDP :9000)        │ USB (iproxy :9001 → :9000)
          ▼                        ▼
┌─────────────────── iPad App ───────────────────────┐
│                                                     │
│  StreamClient (UDP) ─── ──┬──► H264Decoder ──► AVSampleBufferDisplayLayer
│                           │
│  USBListener (TCP :9000) ─┘──► AudioPlayer ──► AVAudioEngine
│                                                     │
│  Beacon listener (port 9999) → auto-connect         │
└─────────────────────────────────────────────────────┘
```

## Repository Layout

```
screx-v2/
├── daemon/                        # Rust Linux daemon
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs                # Entry point, thread orchestration, shutdown
│       ├── capture.rs             # EVDI virtual display capture (+ synthetic fallback)
│       ├── encode.rs              # VA-API H.264 encoder (ffmpeg-next)
│       ├── stream_server.rs       # UDP sender (FEC), audio sender, shared state
│       ├── usb.rs                 # USB device detection, iproxy management, TCP framed sender
│       ├── discovery.rs           # UDP broadcast beacon
│       ├── audio.rs               # PulseAudio virtual sink + parec capture
│       ├── uinput.rs              # Virtual touchscreen/keyboard, OSK toggle
│       └── doctor.rs              # Host readiness checks
├── app/                           # Swift iPad app
│   └── Screx/
│       ├── ScrexApp.swift         # App entry, StreamViewModel, ContentView
│       ├── Discovery.swift        # UDP beacon listener for auto-discovery
│       ├── StreamClient.swift     # WiFi UDP client, FEC reassembly, keepalive
│       ├── USBListener.swift      # USB TCP listener, framed message parsing
│       ├── Decoder.swift          # H.264 Annex-B → VideoToolbox → display layer
│       ├── AudioPlayer.swift      # PCM playback via AVAudioEngine
│       ├── DisplayView.swift      # UIViewRepresentable for AVSampleBufferDisplayLayer
│       └── FEC.swift              # Reed-Solomon decoder for WiFi FEC recovery
└── gnome-extension/               # Screx OSK — GNOME Shell on-screen keyboard
    └── screx-osk@screx/           # (fork of GJS OSK, GPL-3.0)
```

## Linux Dependencies

### Arch Linux

```bash
sudo pacman -S --needed \
  ffmpeg libva mesa \
  pulseaudio-utils \
  libimobiledevice
```

### Debian / Ubuntu

```bash
sudo apt-get install -y \
  libavcodec-dev libavformat-dev libavfilter-dev libavutil-dev libswscale-dev \
  libva-dev mesa-va-drivers \
  pulseaudio-utils \
  libimobiledevice-utils
```

### EVDI

The [EVDI](https://github.com/DisplayLink/evdi) kernel module must be installed for virtual display support:

```bash
# Install from AUR (Arch) or build from source
yay -S evdi-git
sudo modprobe evdi
```

### On-Screen Keyboard (Screx OSK)

The repo includes a forked GNOME Shell extension (based on [GJS OSK](https://github.com/Vishram1123/gjs-osk)) that provides a tablet-optimized on-screen keyboard displayed on the Screx virtual display. It supports modifier keys (Ctrl, Alt, Super), long-press for accented characters, and auto-shows when text fields are focused.

```bash
# Copy the extension
cp -r gnome-extension/screx-osk@screx ~/.local/share/gnome-shell/extensions/

# Compile the GSettings schema
glib-compile-schemas ~/.local/share/gnome-shell/extensions/screx-osk@screx/schemas/

# Enable the extension (log out and back in, or restart GNOME Shell)
gnome-extensions enable screx-osk@screx
```

If you have the original GJS OSK extension installed, disable it to avoid conflicts:

```bash
gnome-extensions disable gjsosk@vishram1123.com
```

The keyboard is toggled from the iPad app via the keyboard button in the floating toolbar, or automatically when a text field receives focus.

### USB Transport (optional)

For USB streaming, `idevice_id` and `iproxy` must be available (provided by `libimobiledevice-utils`). The daemon auto-detects USB devices and manages iproxy automatically.

## Build

### Daemon (Linux)

```bash
cd daemon

# Development build (synthetic capture + bootstrap encoder)
cargo build

# Release build with real EVDI capture and VA-API encoding
cargo build --release --features real-capture,real-encode
```

### iPad App

Open `app/Screx.xcodeproj` in Xcode and build to your iPad (Cmd+R). Requires iOS 16+.

## Run

```bash
cd daemon

# Basic — uses default resolution (2160x1620) and settings
sudo ./target/release/screx-daemon

# Custom resolution and bitrate
sudo SCREX_WIDTH=1600 SCREX_HEIGHT=1200 SCREX_FPS=60 SCREX_GOP=60 \
     SCREX_BITRATE_BPS=10000000 ./target/release/screx-daemon

# Run host readiness checks
cargo run -- doctor
```

The daemon requires `sudo` because EVDI needs root access to create virtual displays.

### Environment Variables

| Variable | Default | Description |
|---|---|---|
| `SCREX_WIDTH` | `2160` | Virtual display width |
| `SCREX_HEIGHT` | `1620` | Virtual display height |
| `SCREX_FPS` | `30` | Target framerate |
| `SCREX_GOP` | `30` | Keyframe interval (frames) |
| `SCREX_BITRATE_BPS` | `8000000` | H.264 encoder bitrate |
| `SCREX_STREAM_PORT` | `9000` | UDP/TCP streaming port |
| `SCREX_ENCODER_BACKEND` | `auto` | `auto`, `vaapi`, or `bootstrap` |

## Protocols

### WiFi Transport (UDP)

Each UDP packet has a 14-byte header:

| Field | Size | Description |
|---|---|---|
| `frame_id` | u32 BE | Frame sequence number |
| `chunk_idx` | u16 BE | Chunk index within frame |
| `total_data` | u16 BE | Number of data chunks |
| `total_parity` | u16 BE | Number of FEC parity chunks |
| `flags` | u8 | Bit 0 = IDR, Bit 1 = audio |
| `reserved` | u8 | — |
| `payload_len` | u16 BE | Actual payload bytes in this chunk |

Video frames are split into 1400-byte chunks with Reed-Solomon FEC parity shards. Audio is sent without FEC (small enough for single packets).

### USB Transport (TCP)

Length-framed messages over TCP (via iproxy USB tunnel):

| Field | Size | Description |
|---|---|---|
| `length` | u32 BE | Payload length (excludes this header) |
| `type` | u8 | `0x01` = video, `0x02` = audio, `0x03` = control |
| payload | variable | Raw Annex-B (video), raw PCM (audio), ASCII (control) |

Video messages include an additional `is_idr` byte (u8) after the type byte.

### Discovery (UDP Broadcast)

The daemon broadcasts a 14-byte beacon to `255.255.255.255:9999` every 2 seconds:

| Field | Size | Description |
|---|---|---|
| magic | 12 bytes | `"SCREX_BEACON"` |
| port | u16 BE | Streaming port number |

The iPad listens on port 9999 and extracts the daemon's IP from the packet source address.

## How It Works

1. **Daemon starts** → creates EVDI virtual display → GNOME sees a new monitor
2. **Beacon broadcasts** → iPad discovers daemon automatically
3. **iPad connects** (WiFi UDP or USB TCP, whichever is available)
4. **Capture loop**: EVDI damage events trigger framebuffer reads → VA-API encodes to H.264 → transport sends to iPad
5. **Audio loop**: `parec` captures from virtual PulseAudio sink → raw PCM sent alongside video
6. **iPad decodes**: VideoToolbox hardware H.264 decode → AVSampleBufferDisplayLayer renders, AVAudioEngine plays audio
7. **Disconnect detection**: data timeouts (WiFi) and TCP close (USB) trigger automatic reconnection

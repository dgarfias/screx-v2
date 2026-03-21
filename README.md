# Screx v2

Low-latency Linux-to-iPad screen streaming. Turns an iPad into a virtual second display for your Linux desktop.

The daemon creates a virtual monitor via EVDI, captures and encodes its framebuffer with H.264 or H.265/HEVC (VA-API, NVENC, or software), and streams video + audio to the iPad app over WiFi (UDP with FEC) or USB (TCP via iproxy). The iPad decodes with VideoToolbox hardware acceleration and displays with AVSampleBufferDisplayLayer.

## Features

- **Virtual second display** via EVDI kernel module — appears as a real monitor in GNOME
- **H.264 and H.265/HEVC encoding** with multiple backends: VA-API (Intel/AMD), NVENC (NVIDIA), or software (libx264/libx265)
- **Dual transport backends**:
  - **WiFi**: UDP with Reed-Solomon FEC, chunked and paced for reliability
  - **USB**: TCP over iproxy/usbmuxd — zero packet loss, lower latency
  - Automatic detection and failover (USB preferred when connected)
- **Audio streaming**: Virtual PulseAudio/PipeWire sink ("Screx iPad") captured via `parec`, streamed alongside video
- **Microphone forwarding**: iPad microphone → Opus-encoded → Linux virtual PipeWire source ("Screx Microphone")
- **Camera forwarding**: iPad camera → JPEG frames → Linux v4l2loopback virtual webcam
- **Touch input**: Multi-touch from iPad mapped to a Linux virtual touchscreen via uinput
- **Keyboard input**: iPad native keyboard forwarded to Linux via uinput virtual keyboard, with `Ctrl+Shift+U` Unicode input for accented/special characters (ñ, á, ö, etc.)
- **Modifier keys**: Accessory bar above the iPad keyboard with Esc, Tab, Ctrl, Alt, Super, Home, End, Ins, Del, and arrow keys — modifiers are sticky one-shot (tap to arm, next key sends the combo)
- **Auto-discovery**: UDP broadcast beacon (no mDNS dependency), iPad auto-connects within seconds
- **Disconnect detection**: Data timeouts and beacon monitoring for automatic reconnection
- **Crash recovery**: Stale PulseAudio/PipeWire modules from previous runs are cleaned up on startup

## Architecture

```
┌──────────────────── Linux Daemon ─────────────────────┐
│                                                       │
│  EVDI ──► H.264/H.265 encode ──► Transport Router   ──┬──► UDP (WiFi)
│                                                       │
│  parec (audio) ──────────────► Transport Router       ┴──► TCP (USB)
│                                                       │
│  Virtual touchscreen (uinput)  ◄── touch events       │
│  Virtual keyboard (uinput)     ◄── key events         │
│  Virtual mic (PipeWire source) ◄── Opus audio         │
│  Virtual webcam (v4l2loopback) ◄── JPEG frames        │
│                                                       │
│  Beacon broadcaster (port 9999)                       │
└───────────────────────────────────────────────────────┘
          │ WiFi (UDP :9000)        │ USB (iproxy :9001 → :9000)
          ▼                         ▼
┌──────────────────── iPad App ─────────────────────────┐
│                                                       │
│  StreamClient (UDP) ────┬──► VideoDecoder ──► Display  │
│                         │                             │
│  USBListener (TCP) ─────┘──► AudioPlayer ──► Speaker  │
│                                                       │
│  Touch ──────────────────────────────────► daemon     │
│  Keyboard + modifier bar ────────────────► daemon     │
│  Microphone (Opus) ──────────────────────► daemon     │
│  Camera (JPEG) ──────────────────────────► daemon     │
│                                                       │
│  Beacon listener (port 9999) → auto-connect           │
└───────────────────────────────────────────────────────┘
```

## Repository Layout

```
screx-v2/
├── daemon/                        # Rust Linux daemon
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs                # Entry point, thread orchestration, shutdown
│       ├── capture.rs             # EVDI virtual display capture
│       ├── encode.rs              # H.264/H.265 encoder: VA-API, NVENC, or software (ffmpeg-next)
│       ├── stream_server.rs       # UDP sender (FEC), audio sender, shared state
│       ├── usb.rs                 # USB device detection, iproxy management, TCP framed sender
│       ├── transport.rs           # Transport abstraction layer
│       ├── discovery.rs           # UDP broadcast beacon
│       ├── audio.rs               # Virtual sink + parec capture, virtual mic (pipe-source / null-sink)
│       ├── camera.rs              # v4l2loopback virtual webcam writer
│       ├── uinput.rs              # Virtual touchscreen + keyboard, modifier combos, Unicode input
│       ├── doctor.rs              # Host readiness checks
│       ├── signaling.rs           # Signaling helpers
│       └── webrtc_sender.rs       # WebRTC sender (experimental)
├── app/                           # Swift iPad app (iOS 16+)
│   └── Screx/
│       ├── ScrexApp.swift         # App entry, StreamViewModel, ContentView, floating toolbar
│       ├── Discovery.swift        # UDP beacon listener for auto-discovery
│       ├── StreamClient.swift     # WiFi UDP client, FEC reassembly, keepalive
│       ├── USBListener.swift      # USB TCP listener, framed message parsing
│       ├── Decoder.swift          # H.264/H.265 Annex-B → VideoToolbox → display layer
│       ├── AudioPlayer.swift      # PCM playback via AVAudioEngine
│       ├── AVSyncState.swift      # Audio/video synchronization state
│       ├── DisplayView.swift      # Video display, touch forwarding, keyboard input + modifier bar
│       ├── FEC.swift              # Reed-Solomon decoder for WiFi FEC recovery
│       ├── MicCapture.swift       # iPad microphone capture → Opus encoding
│       └── CameraCapture.swift    # iPad camera capture → JPEG frames
```

## Linux Dependencies

### Arch Linux

```bash
# Build dependencies
sudo pacman -S --needed \
  rust pkg-config clang \
  ffmpeg libva mesa \
  linux-headers

# Runtime dependencies
sudo pacman -S --needed \
  pulseaudio-utils \
  pipewire-pulse \
  libimobiledevice \
  v4l2loopback-dkms \
  udev

# EVDI (virtual display — from AUR)
yay -S evdi-git
```

### Debian / Ubuntu

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
  libimobiledevice-utils \
  v4l2loopback-dkms v4l2loopback-utils \
  evdi-dkms libevdi0 \
  udev
```

### Kernel Modules

Three kernel modules are required:

| Module | Package (Arch) | Package (Debian/Ubuntu) | Purpose |
|---|---|---|---|
| **evdi** | `evdi-git` (AUR) | `evdi-dkms` | Virtual display (appears as real monitor in GNOME) |
| **v4l2loopback** | `v4l2loopback-dkms` | `v4l2loopback-dkms` | Virtual webcam for iPad camera forwarding |
| **uinput** | built-in | built-in | Virtual touchscreen and keyboard |

All three are loaded automatically by the daemon when needed. If `uinput` isn't loaded on your system, run `sudo modprobe uinput`.

### USB Transport (optional)

For USB streaming, `idevice_id` and `iproxy` must be available (provided by `libimobiledevice` / `libimobiledevice-utils`). The daemon auto-detects USB devices and manages iproxy automatically.

### PipeWire / PulseAudio

The daemon uses `pactl`, `parec`, and `pacat` (from `pulseaudio-utils`) to create virtual audio sinks and sources. These work on both PulseAudio and PipeWire (via the PulseAudio compatibility layer). On PipeWire setups, `pw-link` is also used for microphone routing and is included with PipeWire.

## Build

### Daemon (Linux)

```bash
cd daemon
cargo build --release
```

### iPad App

Open `app/Screx.xcodeproj` in Xcode and build to your iPad (Cmd+R). Requires iOS 16+.

## Run

```bash
cd daemon

# Basic — uses default resolution (2160x1620) and settings
sudo ./target/release/screx

# Custom resolution, framerate, and codec
sudo ./target/release/screx -w 1920 -H 1080 -f 60 -k 60 -b vaapi -c h264

# H.265 with NVENC at 10 Mbps
sudo ./target/release/screx --codec h265 --backend nvenc --bitrate 10000000

# Run host readiness checks
sudo ./target/release/screx doctor
```

The daemon requires `sudo` because EVDI needs root access to create virtual displays.

### Options

| Flag | Short | Default | Description |
|---|---|---|---|
| `--width` | `-w` | `2160` | Virtual display width |
| `--height` | `-H` | `1620` | Virtual display height |
| `--framerate` | `-f` | `30` | Target framerate |
| `--keyframe` | `-k` | `30` | Keyframe interval (in frames) |
| `--bitrate` | `-r` | `8000000` | Encoder bitrate in bps |
| `--port` | `-p` | `9000` | UDP/TCP streaming port |
| `--backend` | `-b` | `auto` | Encoder backend: `auto`, `vaapi`, `nvenc`, `software` |
| `--codec` | `-c` | `h264` | Video codec: `h264`, `h265` |

## iPad App Controls

The floating toolbar pill can be dragged anywhere on screen. Drag to the left edge to switch to vertical layout, drag to the top or bottom edge to switch back to horizontal. Position and orientation are persisted across launches.

| Button | Action |
|---|---|
| Mic | Toggle iPad microphone forwarding (green when active) |
| Camera | Toggle iPad camera forwarding; long-press to flip front/rear |
| Keyboard | Toggle iPad native keyboard with modifier accessory bar |
| Info (ⓘ) | Toggle connection status overlay; drag from anywhere on pill to reposition |

### Keyboard Accessory Bar

When the keyboard is active, an accessory bar appears above the iPad keyboard:

```
[ Esc ] [ Tab ] [ Ctrl ] [ Alt ] [ Super ] [ Home ] [ End ] [ Ins ] [ Del ] [ ← ] [ ↑ ] [ ↓ ] [ → ]
```

- **Ctrl, Alt, Super** are sticky one-shot modifiers: tap to arm (turns blue), then the next key you type sends the combo (e.g., Ctrl + C). Tap again while armed to send the modifier key alone (e.g., Super to open GNOME Activities).
- Accented characters from the iPad's native long-press keyboard (ñ, á, ö, etc.) are automatically handled via the `Ctrl+Shift+U` Unicode input method on Linux.

## Protocols

### Keyboard Packets

Keyboard events are sent as `"KEY" + type(1) + payload`:

| Type | Byte | Payload | Description |
|---|---|---|---|
| TEXT | `0x01` | UTF-8 string | Regular character input |
| SPECIAL | `0x02` | key code (1) | Esc, Tab, arrows, Home, End, Del, Ins, standalone modifiers |
| COMBO | `0x04` | mods(1) + inner_type(1) + inner_payload | Key with modifiers held (Ctrl/Alt/Super + key) |

Modifier mask bits: `0x01` = Ctrl, `0x02` = Alt, `0x04` = Super.

### WiFi Transport (UDP)

Each UDP packet has an 18-byte header:

| Field | Size | Description |
|---|---|---|
| `frame_id` | u32 BE | Frame sequence number |
| `chunk_idx` | u16 BE | Chunk index within frame |
| `total_data` | u16 BE | Number of data chunks |
| `total_parity` | u16 BE | Number of FEC parity chunks |
| `flags` | u8 | Bit 0 = IDR, Bit 1 = audio |
| `codec_id` | u8 | `0x00` = H.264, `0x01` = H.265 |
| `payload_len` | u16 BE | Actual payload bytes in this chunk |
| `timestamp_ms` | u32 BE | Daemon-side timestamp (milliseconds) |

Video frames are split into 1400-byte chunks with Reed-Solomon FEC parity shards. Audio is sent without FEC (small enough for single packets).

### USB Transport (TCP)

Length-framed messages over TCP (via iproxy USB tunnel):

| Field | Size | Description |
|---|---|---|
| `length` | u32 BE | Payload length (excludes this header) |
| `type` | u8 | `0x01` = video, `0x02` = audio, `0x03` = control |
| payload | variable | Raw Annex-B (video), raw PCM (audio), ASCII (control) |

Video messages include `is_idr` (u8) and `codec_id` (u8: `0x00`=H.264, `0x01`=H.265) after the type byte.

### Discovery (UDP Broadcast)

The daemon broadcasts a 14-byte beacon to `255.255.255.255:9999` every 2 seconds:

| Field | Size | Description |
|---|---|---|
| magic | 12 bytes | `"SCREX_BEACON"` |
| port | u16 BE | Streaming port number |

The iPad listens on port 9999 and extracts the daemon's IP from the packet source address.

## How It Works

1. **Daemon starts** → cleans up stale audio modules → creates EVDI virtual display → GNOME sees a new monitor
2. **Beacon broadcasts** → iPad discovers daemon automatically
3. **iPad connects** (WiFi UDP or USB TCP, whichever is available)
4. **Capture loop**: EVDI damage events trigger framebuffer reads → H.264/H.265 encode (VA-API / NVENC / software) → transport sends to iPad
5. **Audio loop**: `parec` captures from virtual PulseAudio sink → raw PCM sent alongside video
6. **Input loop**: iPad sends touch, keyboard, mic, and camera data back to daemon → injected via uinput, PipeWire, and v4l2loopback
7. **iPad decodes**: VideoToolbox hardware H.264/H.265 decode → AVSampleBufferDisplayLayer renders, AVAudioEngine plays audio
8. **Disconnect detection**: data timeouts (WiFi) and TCP close (USB) trigger automatic reconnection

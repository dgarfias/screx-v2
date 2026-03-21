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
┌──────────────────── Linux Daemon ────────────────────┐
│                                                       │
│  EVDI ──► VA-API H.264 ──► Transport Router ──┬──► UDP (WiFi)
│                                                │
│  parec (audio) ──────────────► Transport Router ┴──► TCP (USB)
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
┌──────────────────── iPad App ────────────────────────┐
│                                                       │
│  StreamClient (UDP) ────┬──► H264Decoder ──► Display  │
│                          │                             │
│  USBListener (TCP) ─────┘──► AudioPlayer ──► Speaker  │
│                                                       │
│  Touch ──────────────────────────────────► daemon      │
│  Keyboard + modifier bar ────────────────► daemon      │
│  Microphone (Opus) ──────────────────────► daemon      │
│  Camera (JPEG) ──────────────────────────► daemon      │
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
│       ├── capture.rs             # EVDI virtual display capture (+ synthetic fallback)
│       ├── encode.rs              # VA-API H.264 encoder (ffmpeg-next)
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
│       ├── Decoder.swift          # H.264 Annex-B → VideoToolbox → display layer
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

1. **Daemon starts** → cleans up stale audio modules → creates EVDI virtual display → GNOME sees a new monitor
2. **Beacon broadcasts** → iPad discovers daemon automatically
3. **iPad connects** (WiFi UDP or USB TCP, whichever is available)
4. **Capture loop**: EVDI damage events trigger framebuffer reads → VA-API encodes to H.264 → transport sends to iPad
5. **Audio loop**: `parec` captures from virtual PulseAudio sink → raw PCM sent alongside video
6. **Input loop**: iPad sends touch, keyboard, mic, and camera data back to daemon → injected via uinput, PipeWire, and v4l2loopback
7. **iPad decodes**: VideoToolbox hardware H.264 decode → AVSampleBufferDisplayLayer renders, AVAudioEngine plays audio
8. **Disconnect detection**: data timeouts (WiFi) and TCP close (USB) trigger automatic reconnection

# Screx v2

Low-latency Linux-to-iPad screen streaming. Turns an iPad into a virtual second display for your Linux desktop.

The daemon creates a virtual monitor via EVDI, captures and encodes its framebuffer with H.264 or H.265/HEVC (VA-API, NVENC, or software), and streams video + audio to the iPad app over the network (UDP with FEC) or USB (TCP via iproxy). The iPad decodes with VideoToolbox hardware acceleration and displays with AVSampleBufferDisplayLayer.

## Features

- **Virtual second display** via EVDI kernel module — appears as a real monitor in GNOME
- **H.264 and H.265/HEVC encoding** with multiple backends: VA-API (Intel/AMD), NVENC (NVIDIA), or software (libx264/libx265)
- **Dual transport backends**:
  - **Network**: UDP with Reed-Solomon FEC, chunked and paced for reliability
  - **USB**: TCP over iproxy/usbmuxd — zero packet loss, lower latency
  - Automatic detection and failover (USB preferred when connected)
- **Audio streaming**: Virtual PulseAudio/PipeWire sink ("Screx iPad") captured via `parec`, streamed alongside video
- **Microphone forwarding**: iPad microphone → Opus-encoded → Linux virtual PipeWire source ("Screx Microphone")
- **Camera forwarding**: iPad camera → JPEG frames → Linux v4l2loopback virtual webcam
- **Touch input**: Multi-touch from iPad mapped to a Linux virtual touchscreen via uinput
- **Keyboard input**: iPad native keyboard forwarded to Linux via uinput virtual keyboard, with `Ctrl+Shift+U` Unicode input for accented/special characters (ñ, á, ö, etc.)
- **Modifier keys**: Accessory bar above the iPad keyboard with Esc, Tab, Ctrl, Alt, Super, Home, End, Ins, Del, and arrow keys — modifiers are sticky one-shot (tap to arm, next key sends the combo)
- **Pairing and encryption**: PIN-based pairing via X25519 ECDH key exchange; all network UDP traffic encrypted with AES-256-GCM. Paired devices stored in `~/.config/screx/paired_devices.json`; reconnections are automatic (no re-pairing needed)
- **Single-client mode**: Only one iPad can connect at a time; additional connection attempts receive a `SCREX_BUSY` rejection
- **Auto-discovery**: UDP broadcast beacon (no mDNS dependency), iPad auto-connects within seconds. Beacon pauses during active sessions and resumes on disconnect
- **Disconnect detection**: Data timeouts and beacon monitoring for automatic reconnection
- **Crash recovery**: Stale PulseAudio/PipeWire modules from previous runs are cleaned up on startup

## Architecture

```
┌──────────────────── Linux Daemon ─────────────────────┐
│                                                       │
│  EVDI ──► H.264/H.265 encode ──► Transport Router   ──┬──► UDP (Network, AES-256-GCM)
│                                                       │
│  parec (audio) ──────────────► Transport Router       ┴──► TCP (USB)
│                                                       │
│  Virtual touchscreen (uinput)  ◄── touch events       │
│  Virtual keyboard (uinput)     ◄── key events         │
│  Virtual mic (PipeWire source) ◄── Opus audio         │
│  Virtual webcam (v4l2loopback) ◄── JPEG frames        │
│                                                       │
│  Pairing server (TCP :9000) ── PIN auth + key exchange│
│  Beacon broadcaster (port 9999, pauses when connected)│
└───────────────────────────────────────────────────────┘
          │ Network (UDP :9000)     │ USB (iproxy :9001 → :9000)
          │ encrypted (AES-GCM)     │
          ▼                         ▼
┌──────────────────── iPad App ─────────────────────────┐
│                                                       │
│  PairingService (TCP) ── PIN entry + Keychain storage │
│                                                       │
│  StreamClient (UDP) ────┬──► VideoDecoder ──► Display │
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
│       ├── crypto.rs              # AES-256-GCM encrypt/decrypt, HKDF-SHA256, HMAC, nonce construction
│       ├── pairing.rs             # TCP pairing handshake, paired device storage, unpair CLI
│       ├── usb.rs                 # USB device detection, iproxy management, TCP framed sender
│       ├── transport.rs           # Transport abstraction layer
│       ├── discovery.rs           # UDP broadcast beacon (pauses during active sessions)
│       ├── audio.rs               # Virtual sink + parec capture, virtual mic (pipe-source / null-sink)
│       ├── camera.rs              # v4l2loopback virtual webcam writer
│       ├── uinput.rs              # Virtual touchscreen + keyboard, modifier combos, Unicode input
│       ├── doctor.rs              # Host readiness checks
│       ├── signaling.rs           # Signaling helpers
│       └── webrtc_sender.rs       # WebRTC sender (experimental)
├── app/                           # Swift iPad app (iOS 16+)
│   └── Screx/
│       ├── ScrexApp.swift         # App entry, StreamViewModel, ContentView, floating toolbar
│       ├── Crypto.swift           # CryptoKit wrappers: AES-GCM, X25519 ECDH, HKDF, HMAC
│       ├── PairingService.swift   # TCP pairing client, PIN entry callback, Keychain storage
│       ├── Discovery.swift        # UDP beacon listener for auto-discovery
│       ├── StreamClient.swift     # Network UDP client, FEC reassembly, keepalive, encrypted I/O
│       ├── USBListener.swift      # USB TCP listener, framed message parsing
│       ├── Decoder.swift          # H.264/H.265 Annex-B → VideoToolbox → display layer
│       ├── AudioPlayer.swift      # PCM playback via AVAudioEngine
│       ├── AVSyncState.swift      # Audio/video synchronization state
│       ├── DisplayView.swift      # Video display, touch forwarding, keyboard input + modifier bar
│       ├── FEC.swift              # Reed-Solomon decoder for network FEC recovery
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
sudo ./target/release/screx --codec h265 --backend nvenc --bitrate 10M

# Disable beacon broadcasting (e.g. remote/VPS use)
sudo ./target/release/screx --no-beacon -w 1920 -H 1080

# Run host readiness checks
sudo ./target/release/screx doctor

# List paired devices or unpair
sudo ./target/release/screx unpair            # list all paired devices
sudo ./target/release/screx unpair <device_id> # unpair a specific device
sudo ./target/release/screx unpair --all       # unpair all devices
```

The daemon requires `sudo` because EVDI needs root access to create virtual displays.

### Options

| Flag | Short | Default | Description |
|---|---|---|---|
| `--width` | `-w` | `2160` | Virtual display width |
| `--height` | `-H` | `1620` | Virtual display height |
| `--framerate` | `-f` | `30` | Target framerate |
| `--keyframe` | `-k` | `30` | Keyframe interval (in frames) |
| `--bitrate` | `-r` | `8M` | Encoder bitrate (accepts `8M`, `500K`, or raw `8000000`) |
| `--port` | `-p` | `9000` | UDP/TCP streaming port |
| `--backend` | `-b` | `auto` | Encoder backend: `auto`, `vaapi`, `nvenc`, `software` |
| `--codec` | `-c` | `h264` | Video codec: `h264`, `h265` |
| `--no-beacon` | | `false` | Disable UDP discovery beacon (for VPS/remote use) |

### Subcommands

| Command | Description |
|---|---|
| `doctor` | Run host readiness checks (kernel modules, dependencies) |
| `unpair [device_id]` | Remove a paired device, or `--all` to clear all paired devices |

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

### Pairing Protocol (TCP)

On first connection, the iPad and daemon perform a PIN-based pairing handshake over TCP (port 9000). Reconnections from paired devices skip the PIN step.

**New device flow:**

1. iPad sends `SCREX_PAIR` (10 bytes) + device UUID (36 bytes) + X25519 public key (32 bytes)
2. Daemon generates its own X25519 keypair, computes ECDH shared secret, generates a 6-digit PIN, prints it to stdout
3. Daemon sends `SCREX_PIN` (9 bytes) + server public key (32 bytes)
4. User enters the PIN on the iPad
5. iPad encrypts the PIN with the ECDH-derived key, sends `SCREX_ANSWER` (12 bytes) + encrypted PIN (34 bytes)
6. Daemon verifies the PIN. If correct: derives a `pairing_key` (HKDF) stored in `~/.config/screx/paired_devices.json`, derives a `session_key`, sends `SCREX_OK` (9 bytes) + session salt (32 bytes) + HMAC (32 bytes). If wrong: sends `SCREX_REJECT` (12 bytes)

**Paired device flow (reconnection):**

1. iPad sends `SCREX_HELLO` (11 bytes) + device UUID (36 bytes) + client nonce (32 bytes)
2. Daemon looks up the pairing key, derives a session key from pairing key + nonces
3. Daemon sends `SCREX_OK` (9 bytes) + server nonce (32 bytes) + HMAC (32 bytes)

**Busy rejection:** If a session is already active, the daemon sends `SCREX_BUSY` (12 bytes) and closes the TCP connection.

Once the session key is established, all UDP traffic (both directions) is encrypted with **AES-256-GCM**. The 18-byte packet header is used as AAD (authenticated but not encrypted). Nonces are constructed from packet header fields (direction byte + frame_id + chunk_idx + flags) to guarantee uniqueness.

### Keyboard Packets

Keyboard events are sent as `"KEY" + type(1) + payload`:

| Type | Byte | Payload | Description |
|---|---|---|---|
| TEXT | `0x01` | UTF-8 string | Regular character input |
| SPECIAL | `0x02` | key code (1) | Esc, Tab, arrows, Home, End, Del, Ins, standalone modifiers |
| COMBO | `0x04` | mods(1) + inner_type(1) + inner_payload | Key with modifiers held (Ctrl/Alt/Super + key) |

Modifier mask bits: `0x01` = Ctrl, `0x02` = Alt, `0x04` = Super.

### Network Transport (UDP)

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

When a session key is established (network connections), the payload after the header is AES-256-GCM encrypted with a 16-byte authentication tag appended. The header is sent in plaintext but authenticated as AAD. Client-to-daemon packets prepend a 4-byte sequence number (used as AAD and for nonce construction).

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

The iPad listens on port 9999 and extracts the daemon's IP from the packet source address. The beacon pauses automatically during an active session and resumes when the client disconnects.

## How It Works

1. **Daemon starts** → cleans up stale audio modules → creates EVDI virtual display → GNOME sees a new monitor
2. **Beacon broadcasts** → iPad discovers daemon automatically
3. **Pairing** (first time): TCP handshake with X25519 key exchange → daemon displays a 6-digit PIN → user enters PIN on iPad → pairing key stored on both sides. Subsequent connections skip this step
4. **Session established**: Session key derived → all network UDP traffic encrypted with AES-256-GCM. Beacon pauses, additional connection attempts are rejected
5. **iPad connects** (network UDP or USB TCP, whichever is available)
6. **Capture loop**: EVDI damage events trigger framebuffer reads → H.264/H.265 encode (VA-API / NVENC / software) → encrypted transport sends to iPad
7. **Audio loop**: `parec` captures from virtual PulseAudio sink → raw PCM encrypted and sent alongside video
8. **Input loop**: iPad sends encrypted touch, keyboard, mic, and camera data back to daemon → decrypted and injected via uinput, PipeWire, and v4l2loopback
9. **iPad decodes**: VideoToolbox hardware H.264/H.265 decode → AVSampleBufferDisplayLayer renders, AVAudioEngine plays audio
10. **Disconnect detection**: data timeouts (network) and TCP close (USB) trigger automatic reconnection; beacon resumes for rediscovery

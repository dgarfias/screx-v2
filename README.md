# Screx v2

Low-latency Linux-to-iPad screen streaming. Turns an iPad into a virtual second display for your Linux desktop.

The daemon creates a virtual monitor via EVDI, captures and encodes its framebuffer with H.264 or H.265/HEVC (VA-API, NVENC, or software), and streams video + audio to the iPad app over the network or USB. On the network path, media stays on UDP with FEC while control and input use a persistent encrypted TCP channel. On USB, everything uses framed TCP via iproxy. The iPad decodes with VideoToolbox hardware acceleration and displays with AVSampleBufferDisplayLayer.

## Features

- **Virtual second display** via EVDI kernel module — appears as a real monitor in GNOME
- **H.264 and H.265/HEVC encoding** with multiple backends: VA-API (Intel/AMD), NVENC (NVIDIA), or software (libx264/libx265)
- **Dual transport backends**:
  - **Network**: UDP with Reed-Solomon FEC for media plus persistent encrypted TCP for control/input
  - **USB**: TCP over iproxy/usbmuxd — zero packet loss, lower latency
  - Automatic detection and failover (USB preferred when connected)
- **Audio streaming**: Virtual PulseAudio/PipeWire sink ("Screx iPad") captured via `parec`, streamed alongside video
- **Microphone forwarding**: iPad microphone → Opus-encoded → Linux virtual PipeWire source ("Screx Microphone")
- **Camera forwarding**: iPad camera → JPEG frames → Linux v4l2loopback virtual webcam
- **Touch input**: Multi-touch from iPad mapped to a Linux virtual touchscreen via uinput
- **Keyboard input**: iPad native keyboard forwarded to Linux via uinput virtual keyboard, with `Ctrl+Shift+U` Unicode input for accented/special characters (ñ, á, ö, etc.)
- **Modifier keys**: Accessory bar above the iPad keyboard with Esc, Tab, Ctrl, Alt, Super, Home, End, Ins, Del, and arrow keys — modifiers are sticky one-shot (tap to arm, next key sends the combo)
- **Physical peripheral forwarding**: External mouse and keyboard connected to the iPad are detected automatically and forwarded to Linux over both Network and USB. The daemon only creates the matching Linux virtual mouse/keyboard device when the iPad reports that the peripheral is actually attached.
- **Game controller forwarding**: Up to 4 controllers connected to the iPad are detected automatically and forwarded as generic Linux virtual gamepads. The daemon creates one Linux virtual gamepad per attached controller and removes it again when the controller disconnects.
- **Pointer capture for external mouse**: When a physical mouse is active, iPadOS pointer input is captured for the app, the system pointer is hidden, and top status/system overlays are suppressed for a cleaner full-screen desktop view
- **Touch/pointer separation**: Indirect pointer touches are filtered out so physical mouse clicks are not also forwarded as touchscreen taps
- **Pairing and encryption**: PIN-based pairing via X25519 ECDH key exchange; network UDP media and network TCP control both use AES-256-GCM. Paired devices stored in `~/.config/screx/paired_devices.json`; reconnections are automatic (no re-pairing needed)
- **Single-client mode**: Only one iPad can connect at a time; additional connection attempts receive a `SCREX_BUSY` rejection
- **Auto-discovery**: UDP broadcast beacon (no mDNS dependency), iPad auto-connects within seconds. Beacon pauses during active sessions and resumes on disconnect
- **Disconnect detection**: Data timeouts and beacon monitoring for automatic reconnection
- **Crash recovery**: Stale PulseAudio/PipeWire modules from previous runs are cleaned up on startup

## Architecture

```
┌──────────────────── Linux Daemon ─────────────────────┐
│                                                       │
│  EVDI ──► H.264/H.265 encode ──► Transport Router   ──┬──► UDP media (Network, AES-256-GCM)
│                                                       │
│  parec (audio) ──────────────► Transport Router       ┴──► TCP (USB)
│                                                       │
│  Virtual touchscreen (uinput)  ◄── touch events       │
│  Virtual keyboard (uinput)     ◄── key events         │
│  Virtual mic (PipeWire source) ◄── Opus audio         │
│  Virtual webcam (v4l2loopback) ◄── JPEG frames        │
│                                                       │
│  Pairing server + control channel (TCP :9000)         │
│      └── PIN auth, key exchange, persistent input TCP │
│  Beacon broadcaster (port 9999, pauses when connected)│
└───────────────────────────────────────────────────────┘
          │ Network UDP media + TCP control │ USB (iproxy :9001 → :9000)
          │ encrypted (AES-GCM)             │
          ▼                         ▼
┌──────────────────── iPad App ─────────────────────────┐
│                                                       │
│  PairingService (TCP) ── PIN entry + Keychain storage │
│  NetworkControlClient (TCP) ─ touch/keys/mouse/gamepad│
│                           └─ PLI + control messages   │
│                                                       │
│  StreamClient (UDP) ────┬──► VideoDecoder ──► Display │
│                         │                             │
│  USBListener (TCP) ─────┘──► AudioPlayer ──► Speaker  │
│                                                       │
│  Touch / Pencil ─────────────────────────► daemon     │
│  Keyboard + modifier bar ────────────────► daemon     │
│  External mouse / keyboard ──────────────► daemon     │
│  Up to 4 game controllers ───────────────► daemon     │
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
│       ├── stream_server.rs       # UDP sender (FEC), audio sender, shared state, UDP media receiver
│       ├── crypto.rs              # AES-256-GCM encrypt/decrypt, HKDF-SHA256, HMAC, nonce construction
│       ├── pairing.rs             # TCP pairing handshake, persistent network control channel, paired device storage
│       ├── usb.rs                 # USB device detection, iproxy management, TCP framed sender
│       ├── transport.rs           # Transport abstraction layer
│       ├── discovery.rs           # UDP broadcast beacon (pauses during active sessions)
│       ├── audio.rs               # Virtual sink + parec capture, virtual mic (pipe-source / null-sink)
│       ├── camera.rs              # v4l2loopback virtual webcam writer
│       ├── uinput.rs              # Virtual touchscreen, keyboard, mouse, and gamepad injection
│       ├── doctor.rs              # Host readiness checks
│       ├── signaling.rs           # Signaling helpers
│       └── webrtc_sender.rs       # WebRTC sender (experimental)
├── app/                           # Swift iPad app (iOS 16+)
│   └── Screx/
│       ├── ScrexApp.swift         # App entry, transport orchestration, peripheral + controller forwarding UI
│       ├── Crypto.swift           # CryptoKit wrappers: AES-GCM, X25519 ECDH, HKDF, HMAC
│       ├── PairingService.swift   # TCP pairing client, PIN entry callback, Keychain storage
│       ├── NetworkControlClient.swift # Persistent encrypted TCP control client for network input
│       ├── Discovery.swift        # UDP beacon listener for auto-discovery
│       ├── StreamClient.swift     # Network UDP media client, FEC reassembly, keepalive, encrypted I/O
│       ├── USBListener.swift      # USB TCP listener, framed message parsing
│       ├── Decoder.swift          # H.264/H.265 Annex-B → VideoToolbox → display layer
│       ├── AudioPlayer.swift      # PCM playback via AVAudioEngine
│       ├── AVSyncState.swift      # Audio/video synchronization state
│       ├── DisplayView.swift      # Video display, touch forwarding, indirect pointer filtering, discrete wheel scroll capture
│       ├── FEC.swift              # Reed-Solomon decoder for network FEC recovery
│       ├── MicCapture.swift       # iPad microphone capture → Opus encoding
│       └── CameraCapture.swift    # iPad camera capture → JPEG frames
```

## Linux Dependencies

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
  pipewire-bin \
  libimobiledevice-utils \
  libusbmuxd-tools \
  v4l2loopback-dkms \
  evdi-dkms libevdi1 \
  udev
```

### Kernel Modules

Three kernel modules are required:

| Module | Package (Arch) | Package (Debian/Ubuntu) | Purpose |
|---|---|---|---|
| **evdi** | `evdi-git` (AUR) | `evdi-dkms` | Virtual display (appears as real monitor in GNOME) |
| **v4l2loopback** | `v4l2loopback-dkms` | `v4l2loopback-dkms` | Virtual webcam for iPad camera forwarding |
| **uinput** | built-in | built-in | Virtual touchscreen, keyboard, mouse, and gamepad |

All three are loaded automatically by the daemon when needed. If `uinput` isn't loaded on your system, run `sudo modprobe uinput`.

### USB Transport (optional)

For USB streaming, `idevice_id` and `iproxy` must be available. On Arch, they come from `libimobiledevice` and `libusbmuxd`. On Debian/Ubuntu, they come from `libimobiledevice-utils` and `libusbmuxd-tools`. The daemon auto-detects USB devices and manages iproxy automatically.

### PipeWire / PulseAudio

The daemon uses `pactl`, `parec`, and `pacat` to create virtual audio sinks and sources. On Arch, those commands are provided by `libpulse`; on Debian/Ubuntu they are provided by `pulseaudio-utils`. These work on both PulseAudio and PipeWire (via the PulseAudio compatibility layer). On PipeWire setups, `pw-link` is also used for microphone routing; on Arch it is provided by `pipewire`, and on Debian/Ubuntu by `pipewire-bin`.

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
| `--verbose` | `-v` | `false` | Enable detailed diagnostic logging |

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
| Keyboard | Toggle iPad native keyboard with modifier accessory bar; grays out when an external keyboard is connected and shows `External keyboard detected` if tapped |
| Info (ⓘ) | Toggle connection status overlay; drag from anywhere on pill to reposition |

### Keyboard Accessory Bar

When the keyboard is active, an accessory bar appears above the iPad keyboard:

```
[ Esc ] [ Tab ] [ Ctrl ] [ Alt ] [ Super ] [ Home ] [ End ] [ Ins ] [ Del ] [ ← ] [ ↑ ] [ ↓ ] [ → ]
```

- **Ctrl, Alt, Super** are sticky one-shot modifiers: tap to arm (turns blue), then the next key you type sends the combo (e.g., Ctrl + C). Tap again while armed to send the modifier key alone (e.g., Super to open GNOME Activities).
- Accented characters from the iPad's native long-press keyboard (ñ, á, ö, etc.) are automatically handled via the `Ctrl+Shift+U` Unicode input method on Linux.

### External Mouse and Keyboard

- The iPad app watches for external mouse and keyboard connections automatically.
- When an external mouse is detected, the app tells the daemon that a mouse exists, and the daemon creates a Linux virtual mouse for that session.
- When an external keyboard is detected, the app tells the daemon that a keyboard exists, and Linux uses the existing virtual keyboard for raw key events from that hardware keyboard.
- If either peripheral disconnects from the iPad, the app reports that immediately and the daemon tears down or stops using the matching Linux-side virtual device.
- A physical mouse connected to the iPad is forwarded as a Linux virtual mouse.
- A physical keyboard connected to the iPad is forwarded as raw key events to the Linux virtual keyboard.
- Pointer input is captured by the app while the physical mouse is active, so indirect pointer touches are not also forwarded as touchscreen taps.
- Mouse wheel scrolling is forwarded separately from button clicks and uses the iPad display surface's discrete scroll input path.
- Left and right click preserve normal press/release semantics. Middle click is supported as a click action; middle-button hold semantics are not guaranteed.
- When an external keyboard is connected, the on-screen keyboard button is disabled visually and tapping it shows `External keyboard detected`.

### Game Controllers

- The iPad app watches for controller attach/detach events automatically.
- Up to 4 controllers can be forwarded at the same time.
- Each attached controller is assigned its own slot and creates exactly one Linux virtual gamepad device.
- Linux gamepads are only created when the iPad explicitly reports that a controller is attached; nothing is pre-created just in case.
- When a controller disconnects from the iPad, the daemon removes the corresponding Linux virtual gamepad automatically.
- Controllers are exposed as generic Linux gamepads over `uinput`, intended to work broadly with native Linux input stacks.
- Controllers using unsupported GameController profiles are ignored rather than creating a broken virtual device.

## Protocols

### Pairing Protocol (TCP)

On first connection, the iPad and daemon perform a PIN-based pairing handshake over TCP (port 9000). Reconnections from paired devices skip the PIN step.

**New device flow:**

1. iPad sends `SCREX_PAIR` (10 bytes) + device ID (16 bytes) + X25519 public key (32 bytes)
2. Daemon generates its own X25519 keypair, computes ECDH shared secret, generates a 6-digit PIN, prints it to stdout
3. Daemon sends `SCREX_PIN` (10 bytes) + server public key (32 bytes)
4. User enters the PIN on the iPad
5. iPad encrypts the PIN with the ECDH-derived key, sends `SCREX_ANSWER` (12 bytes) + encrypted PIN (34 bytes)
6. Daemon verifies the PIN. If correct: derives a `pairing_key` (HKDF) stored in `~/.config/screx/paired_devices.json`, derives a `session_key`, sends `SCREX_OK` (10 bytes) + session salt (32 bytes) + HMAC (32 bytes). If wrong: sends `SCREX_REJECT` (12 bytes)

**Paired device flow (reconnection):**

1. iPad sends `SCREX_HELLO` (11 bytes) + device ID (16 bytes) + client nonce (32 bytes)
2. Daemon looks up the pairing key, derives a session key from pairing key + nonces
3. Daemon sends `SCREX_OK` (10 bytes) + server nonce (32 bytes) + HMAC (32 bytes)

**Busy rejection:** If a session is already active, the daemon sends `SCREX_BUSY` (12 bytes) and closes the TCP connection.

Once the session key is established, network traffic is split:

- **UDP media path**: video/audio from daemon to iPad plus mic/camera from iPad to daemon, encrypted with **AES-256-GCM**
- **TCP control path**: touch, keyboard, physical mouse/keyboard, gamepad attach/state/detach, peripheral attach/detach, and PLI/control messages, encrypted with **AES-256-GCM**

The UDP media header remains plaintext but authenticated as AAD. TCP control frames are length-prefixed and include a per-frame sequence number used as AAD and as part of nonce construction.

### Keyboard Packets

Keyboard events are sent as `"KEY" + type(1) + payload`:

| Type | Byte | Payload | Description |
|---|---|---|---|
| TEXT | `0x01` | UTF-8 string | Regular character input |
| SPECIAL | `0x02` | key code (1) | Esc, Tab, arrows, Home, End, Del, Ins, standalone modifiers |
| COMBO | `0x04` | mods(1) + inner_type(1) + inner_payload | Key with modifiers held (Ctrl/Alt/Super + key) |

Modifier mask bits: `0x01` = Ctrl, `0x02` = Alt, `0x04` = Super.

### Network Media Transport (UDP)

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

When a session key is established, the payload after the header is AES-256-GCM encrypted with a 16-byte authentication tag appended. The header is sent in plaintext but authenticated as AAD. Client-to-daemon UDP media packets prepend a 4-byte sequence number (used as AAD and for nonce construction).

### Network Control Transport (TCP)

Network control/input uses the same paired TCP socket established during pairing.

| Field | Size | Description |
|---|---|---|
| `length` | u32 BE | Frame length excluding this header |
| `seq` | u32 BE | Per-frame sequence number |
| `ciphertext+tag` | variable | AES-256-GCM encrypted control payload |

Plaintext control payloads reuse the existing message prefixes: `PLI`, `TOUCH`, `KEY`, `MOUSE`, `RAWKEY`, `PERIPH`, and `GPAD`.

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
4. **Session established**: Session key derived → persistent encrypted TCP control channel stays open for input/control; encrypted UDP is used for media. Beacon pauses, additional connection attempts are rejected
5. **iPad connects** (network UDP media + TCP control, or USB TCP)
6. **Capture loop**: EVDI damage events trigger framebuffer reads → H.264/H.265 encode (VA-API / NVENC / software) → encrypted UDP media sends to iPad
7. **Audio loop**: `parec` captures from virtual PulseAudio sink → raw PCM encrypted and sent alongside video
8. **Input loop**: iPad sends encrypted touch, keyboard, physical mouse/keyboard, gamepad state, and control traffic over TCP; indirect pointer touches are filtered out so physical mouse clicks do not also become touchscreen taps. Mic and camera data return over encrypted UDP media → injected via uinput, PipeWire, and v4l2loopback
9. **iPad decodes**: VideoToolbox hardware H.264/H.265 decode → AVSampleBufferDisplayLayer renders, AVAudioEngine plays audio
10. **Disconnect detection**: data timeouts (network) and TCP close (USB) trigger automatic reconnection; beacon resumes for rediscovery

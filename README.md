# Screx v2

Low-latency Linux-to-iPad screen streaming. Turns an iPad into a virtual second display for your Linux desktop.

The daemon creates a virtual monitor via EVDI, captures and encodes its framebuffer with H.264 or H.265/HEVC (VA-API, NVENC, or software), and streams video + audio to the iPad app over the network or USB. On the network path, media stays on UDP with FEC while control and input use a persistent encrypted TCP channel. On USB, everything uses framed TCP via iproxy. The iPad decodes with VideoToolbox hardware acceleration and displays with AVSampleBufferDisplayLayer.

## Features

- **Virtual second display** via EVDI kernel module — appears as a real monitor in GNOME
- **H.264 and H.265/HEVC encoding** with multiple backends: VA-API (Intel/AMD), NVENC (NVIDIA), or software (libx264/libx265)
- **Dual transport backends**:
  - **Network**: UDP with Reed-Solomon FEC for media plus persistent encrypted TCP for control/input
  - **USB**: TCP over iproxy/usbmuxd — zero packet loss, lower latency
  - Both transports are manual from the iPad app: network connects by host/IP, USB connects from a dedicated `Connect via USB` button
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
- **Manual connection screen**: Enter a daemon hostname/IP directly, optionally include a port like `192.168.1.10:9000`, pin up to 10 favorite network connections so they never age out, reconnect from the app's 5 most recent unpinned network connections, or use a dedicated USB connect button when the iPad is plugged in
- **Single-client mode**: Only one iPad can connect at a time; additional connection attempts receive a `SCREX_BUSY` rejection
- **Disconnect detection**: Network data timeouts and TCP close handling return the app to a clear disconnected state instead of silently retrying in the background
- **Selectable daemon transport mode**: Run the daemon in combined mode (default), `--network-only`, or `--usb-only`
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
│  Manual host/IP[:port] entry + pinned/recent network  │
│  connections + explicit USB connect button            │
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
│       ├── audio.rs               # Virtual sink + parec capture, virtual mic (pipe-source / null-sink)
│       ├── camera.rs              # v4l2loopback virtual webcam writer
│       ├── uinput.rs              # Virtual touchscreen, keyboard, mouse, and gamepad injection
│       ├── doctor.rs              # Host readiness checks
│       └── logging.rs             # Verbose logging helpers
├── app/                           # Swift iPad app (iOS 16+)
│   └── Screx/
│       ├── ScrexApp.swift         # App entry, transport orchestration, peripheral + controller forwarding UI
│       ├── Crypto.swift           # CryptoKit wrappers: AES-GCM, X25519 ECDH, HKDF, HMAC
│       ├── PairingService.swift   # TCP pairing client, PIN entry callback, Keychain storage
│       ├── NetworkControlClient.swift # Persistent encrypted TCP control client for network input
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

For USB streaming, `idevice_id` and `iproxy` must be available. On Arch, they come from `libimobiledevice` and `libusbmuxd`. On Debian/Ubuntu, they come from `libimobiledevice-utils` and `libusbmuxd-tools`. When USB transport is enabled on the daemon, it watches for attached iOS devices and manages `iproxy` automatically. The iPad app does not auto-connect over USB; the user starts USB listening from the connection screen with `Connect via USB`.

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

# Network only (disable USB transport)
sudo ./target/release/screx --network-only

# USB only (disable TCP pairing server and UDP network transport)
sudo ./target/release/screx --usb-only

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
| `--verbose` | `-v` | `false` | Enable detailed diagnostic logging |
| `--network-only` |  | `false` | Disable USB transport and run only the network pairing/control/media path |
| `--usb-only` |  | `false` | Disable network pairing/UDP transport and run only the USB transport path |

### Subcommands

| Command | Description |
|---|---|
| `doctor` | Run host readiness checks (kernel modules, dependencies) |
| `unpair [device_id]` | Remove a paired device, or `--all` to clear all paired devices |

## iPad App Controls

### Connection Screen

When disconnected, the iPad app shows a dedicated connection screen rather than the small in-session info overlay.

- **Network connect**: Enter a daemon host/IP or `host:port`, then tap `Connect`
- **Saved network targets**: Pin up to 10 favorite endpoints and keep up to 5 recent unpinned endpoints
- **USB connect**: Tap `Connect via USB` to start the app's USB listener. The button is only enabled when the iPad appears to be plugged in via USB/power
- **No background auto-retry**: after a failed or dropped connection, the app returns to the connection screen and waits for explicit user action

### In-Session Overlay

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

## How It Works

1. **Daemon starts** → cleans up stale audio modules → creates EVDI virtual display → GNOME sees a new monitor
2. **User chooses a transport** from the iPad connection screen:
   - **Network**: enter a daemon host/IP (optionally `host:port`), or reuse a pinned/recent saved network target
   - **USB**: tap `Connect via USB` to start the app's USB listener, then let the daemon's USB transport connect through `iproxy`
3. **Pairing** (network, first time only): TCP handshake with X25519 key exchange → daemon displays a 6-digit PIN → user enters PIN on iPad → pairing key stored on both sides. Subsequent network connections skip the PIN step
4. **Session established**:
   - **Network**: session key derived → persistent encrypted TCP control channel stays open for input/control; encrypted UDP is used for media
   - **USB**: the daemon opens a framed TCP stream over the USB tunnel for media and control
5. **Capture loop**: EVDI damage events trigger framebuffer reads → H.264/H.265 encode (VA-API / NVENC / software) → transport router sends frames over encrypted UDP (network) or framed TCP (USB)
6. **Audio loop**: `parec` captures from virtual PulseAudio sink → raw PCM is sent alongside video on the active transport
7. **Input loop**: iPad sends touch, keyboard, physical mouse/keyboard, gamepad state, and control traffic to the daemon. Indirect pointer touches are filtered out so physical mouse clicks do not also become touchscreen taps. Mic and camera data return to Linux and are injected via uinput, PipeWire, and v4l2loopback
8. **iPad decodes**: VideoToolbox hardware H.264/H.265 decode → AVSampleBufferDisplayLayer renders, AVAudioEngine plays audio
9. **Disconnect detection**: data timeouts (network) and TCP close/error handling (USB) return the app to the manual connection screen; reconnects are user-initiated rather than automatic

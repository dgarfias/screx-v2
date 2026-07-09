# Screx Architecture

## Overview

Screx is a remote display system built around a virtual monitor on a host machine (Linux or
Windows) and custom low-latency clients on iPad and desktop (macOS/Windows/Linux).

At a high level:

- the daemon creates a virtual display (EVDI on Linux, a DXGI-duplicated virtual monitor via a
  third-party driver on Windows)
- the host compositor renders to that virtual display
- the daemon captures, encodes, and streams video/audio to the client
- the client sends back touch/mouse/keyboard, controller, microphone, camera, and session control
  messages

The daemon binary and wire protocol are the same across host platforms — see
[DAEMON_LINUX.md](DAEMON_LINUX.md) and [DAEMON_WINDOWS.md](DAEMON_WINDOWS.md) for platform-specific
build/run/driver details. Either daemon works with either client — see
[CLIENT_IPAD.md](CLIENT_IPAD.md) and [CLIENT_DESKTOP.md](CLIENT_DESKTOP.md).

The system supports two transports:

- **Network**
  - UDP for media
  - encrypted TCP for pairing and control
- **USB** (iPad client only)
  - framed TCP over `iproxy` (Linux) / Apple Mobile Device Service (Windows)

## High-Level Data Flow

```text
Host desktop/compositor
    -> virtual monitor (EVDI / DXGI duplication)
    -> capture
    -> encode
    -> transport
    -> client decode/display

Client input/peripherals
    -> control transport
    -> daemon
    -> uinput+PipeWire+v4l2loopback (Linux) / SendInput+WASAPI+DirectShow+ViGEmBus (Windows)
```

## Main Components

### Daemon (shared core)

Key files:

- `daemon/src/main.rs`
- `daemon/src/capture.rs`
- `daemon/src/encode.rs`
- `daemon/src/stream_server.rs`
- `daemon/src/pairing.rs`
- `daemon/src/usb.rs`
- `daemon/src/audio.rs`
- `daemon/src/camera.rs`
- `daemon/src/input.rs`

Responsibilities:

- create and manage the virtual display
- capture frames from it
- encode H.264/H.265
- stream media over network or USB
- manage pairing and session keys
- parse touch/keyboard/mouse/controller input messages
- expose virtual microphone, webcam, and speaker devices

### Daemon (platform backends)

Platform-specific implementations live behind the `crate::platform` module
(`daemon/src/platform/mod.rs`), selected at compile time:

- `daemon/src/platform/linux/` — EVDI virtual display, `uinput` virtual input devices,
  PipeWire/PulseAudio, `v4l2loopback` webcam, `idevice_id`/`iproxy` for USB
- `daemon/src/platform/windows/` — DXGI Desktop Duplication + a third-party Indirect Display
  Driver for the virtual monitor (`display.rs`), `SendInput`-based input (`input.rs`), WASAPI +
  "Steam Streaming Speakers" for audio (`wasapi.rs`, `audio_driver.rs`), a DirectShow capture
  filter DLL for the virtual webcam (`vcam.rs`, `vcam_filter/lib.rs`), ViGEmBus for virtual Xbox
  360 gamepads (`vigem.rs`), and native usbmuxd-protocol speech to Apple Mobile Device Service for
  USB detection (`usbmux.rs`)

See [DAEMON_LINUX.md](DAEMON_LINUX.md) and [DAEMON_WINDOWS.md](DAEMON_WINDOWS.md) for the
dependencies and drivers each backend needs.

### iPad app

Key files:

- `client/ipad/Screx/ScrexApp.swift`
- `client/ipad/Screx/PairingService.swift`
- `client/ipad/Screx/NetworkControlClient.swift`
- `client/ipad/Screx/StreamClient.swift`
- `client/ipad/Screx/USBListener.swift`
- `client/ipad/Screx/Decoder.swift`
- `client/ipad/Screx/DisplayView.swift`
- `client/ipad/Screx/AudioPlayer.swift`
- `client/ipad/Screx/MicCapture.swift`
- `client/ipad/Screx/CameraCapture.swift`
- `client/ipad/Screx/Crypto.swift`

Responsibilities:

- manual connection UI for network and USB
- PIN pairing flow
- capability negotiation (`CAPS`/`STNG`) with the daemon
- network media receive path
- USB media receive path
- low-latency video decode and display
- local audio playback
- input forwarding
- microphone and camera forwarding
- connection-health UI and session state
- Stream Settings sheet for choosing resolution, framerate, codec, and bitrate before connecting

### Desktop client

Key files:

- `client/desktop/src/main.rs`, `backend.rs`, `app_state.rs`
- `client/desktop/src/decoder.rs`, `video_surface.rs`
- `client/desktop/src/input.rs`, `keyboard_grab.rs`
- `client/desktop/src/audio_player.rs`, `mic_capture.rs`
- `client/desktop/src/webcam_capture.rs`
- `client/desktop/qml/Main.qml`

Responsibilities (network transport only — no USB support):

- connection UI (Qt Quick/QML) driving a Rust backend
- PIN pairing flow
- capability negotiation (`CAPS`/`STNG`) with the daemon
- Stream Settings UI for choosing resolution, framerate, codec, and bitrate before connecting
- network media receive path, with zero-copy hardware-accelerated decode/display where available
  (VA-API on Linux, D3D11VA on Windows)
- local audio playback and microphone capture
- input forwarding, including OS-level keyboard grabbing
- webcam forwarding via `nokhwa`

## Transport Model

### Network mode

- Pairing and control use a persistent TCP connection.
- Media uses UDP.
- Video and audio are sent daemon -> client over UDP.
- Mic and camera data are sent client -> daemon over UDP.
- Touch/keyboard/mouse/controller/peripheral state travel over encrypted TCP control.

Why:

- UDP avoids head-of-line blocking for media.
- TCP control gives reliable ordering for input/state messages.

### USB mode

- The daemon uses `idevice_id` and `iproxy`.
- The iPad opens a local TCP listener.
- The daemon connects through `iproxy`.
- Video, audio, and control all use framed TCP messages over that USB tunnel.

Why:

- USB avoids Wi‑Fi jitter and loss.
- One reliable framed stream is simple and effective for local tethered use.

## Connection Lifecycle

### Network session

1. iPad opens TCP to daemon.
2. Pairing or reconnect handshake runs.
3. Session key is established.
4. TCP control channel stays open.
5. iPad opens UDP path and starts sending encrypted register packets.
6. Daemon authenticates first UDP packets and starts media.
7. First decoded frame on the client moves session into streaming state.

### USB session

1. iPad enters `Connect via USB` listening mode.
2. Daemon detects the connected iPadOS device and starts `iproxy`.
3. Daemon connects to the app listener over forwarded TCP.
4. iPad sends `READY`.
5. Daemon activates USB transport.
6. Daemon starts media/control over framed TCP.

## Pairing Protocol

Pairing happens only on the network path.

### New device flow

1. iPad sends `SCREX_PAIR` + device ID + X25519 public key
2. daemon generates its own X25519 keypair and a 6-digit PIN
3. daemon sends `SCREX_PIN` + server public key
4. user enters PIN on iPad
5. iPad sends `SCREX_ANSWER` + encrypted PIN
6. daemon verifies PIN
7. daemon derives and stores a pairing key
8. both sides derive a session key
9. daemon sends `SCREX_OK` + session salt + HMAC

### Reconnect flow

1. iPad sends `SCREX_HELLO` + device ID + client nonce
2. daemon loads pairing key
3. both sides derive a fresh session key using nonces
4. daemon sends `SCREX_OK` + server nonce + HMAC

### Busy rejection

If a session is already active, the daemon replies with `SCREX_BUSY`.

## Encryption Model

### Algorithms

- X25519 for key agreement
- HKDF-SHA256 for key derivation
- AES-256-GCM for transport encryption and authentication
- HMAC-SHA256 for handshake verification

### UDP media

- header is plaintext
- payload is AES-GCM encrypted
- header is authenticated as AAD

### TCP control

- control payloads are AES-GCM encrypted
- per-frame sequence number is authenticated as AAD

### Nonce directions

- daemon -> iPad UDP uses `nonce_server`
- iPad -> daemon UDP uses `nonce_client`
- iPad -> daemon TCP control uses `nonce_control_client`
- daemon -> iPad TCP control uses `nonce_control_server`

## Protocol Details

## Network Media Transport (UDP)

Each UDP packet starts with an 18-byte header:

| Field | Size | Description |
|---|---|---|
| `frame_id` | u32 BE | Frame number |
| `chunk_idx` | u16 BE | Chunk number within frame |
| `total_data` | u16 BE | Number of data chunks |
| `total_parity` | u16 BE | Number of parity chunks |
| `flags` | u8 | Bit 0 = IDR, Bit 1 = audio |
| `codec_id` | u8 | `0x00` = H.264, `0x01` = H.265 |
| `payload_len` | u16 BE | Payload size |
| `timestamp_ms` | u32 BE | Daemon timestamp |

Notes:

- video is split into chunks of 1400 bytes
- optional Reed-Solomon parity is added for video
- audio is small enough to go without FEC
- client-to-daemon media packets prepend a 4-byte sequence number before encrypted payload

### Network Control Transport (TCP)

Each encrypted TCP control frame is:

| Field | Size | Description |
|---|---|---|
| `length` | u32 BE | Frame body length |
| `seq` | u32 BE | Control sequence number |
| `ciphertext+tag` | variable | AES-GCM encrypted payload |

Control payloads include messages such as:

- `PLI`
- `TOUCH`
- `KEY`
- `MOUSE`
- `RAWKEY`
- `PERIPH`
- `GPAD`
- `SPKR`
- `HOST<hostname>`
- `CAPS` (daemon -> client, capability negotiation)
- `STNG` (client -> daemon, capability negotiation)
- `DISCONNECT`

### USB Transport (TCP)

Each framed USB message is:

| Field | Size | Description |
|---|---|---|
| `length` | u32 BE | Payload length |
| `type` | u8 | `0x01` video, `0x02` audio, `0x03` control |
| `payload` | variable | Type-specific bytes |

#### USB video payload

`type(1) + is_idr(1) + codec_id(1) + timestamp_ms(4) + annex_b`

#### USB audio payload

`type(1) + timestamp_ms(4) + pcm`

#### USB control payload

ASCII prefix plus message body, for example:

- `READY`
- `PLI`
- `SPKR<1-byte-flag>`
- `HOST<hostname>`
- `CAPS...` (daemon -> client, capability negotiation)
- `STNG...` (client -> daemon, capability negotiation)
- `TOUCH...`
- `KEY...`
- `MOUSE...`
- `RAWKEY...`
- `PERIPH...`
- `GPAD...`
- `CAM...`
- `MIC...`

## Capability Negotiation

Right after the daemon sends `HOST<hostname>` on the control channel — network TCP control and USB
control alike — it sends `CAPS`, telling the client what it can actually do. The client replies with
`STNG`, proposing session settings within the bounds `CAPS` advertised. Both messages travel inside
the same control framing already used for `HOST` (network: AES-GCM control frame via
`send_control_frame`; USB: `type = 0x03` control payload) — there is no new transport, socket, or
crypto involved.

Both messages share one TLV envelope: a 4-byte ASCII magic, a version byte, an entry count, then that
many `tag(u8) + length(u16 BE) + value` entries. **A parser that doesn't recognize a tag reads the
`u16` length and skips that many bytes, then continues to the next entry** — unknown tags are never
treated as an error. This is how future protocol versions add tags without breaking older parsers.

### `CAPS` (daemon -> client)

```
"CAPS"(4 bytes ASCII) + version(u8, currently 1) + entry_count(u8) + entries...

each entry: tag(u8) + length(u16 BE) + value(length bytes)
```

v1 tags:

| Tag | Name | Value layout | Meaning |
|---|---|---|---|
| `0x01` | CAMERA | `available(u8 0/1)` | Virtual webcam forwarding works right now |
| `0x02` | MICROPHONE | `available(u8 0/1)` | Virtual microphone forwarding works right now |
| `0x03` | SPEAKER | `available(u8 0/1)` | Speaker/system-audio forwarding works right now |
| `0x04` | GAMEPAD | `available(u8 0/1)` + `max_controllers(u8)` | Gamepad passthrough works, and how many simultaneous controllers |
| `0x05` | CODECS | `count(u8)` + `count` bytes of codec id (`0x00`=H.264, `0x01`=H.265) | Which codecs this daemon can actually encode right now |
| `0x06` | MAX_RESOLUTION | `width(u16 BE)` + `height(u16 BE)` | Upper bound the client may request |
| `0x07` | MAX_FRAMERATE | `fps(u8)` | Upper bound the client may request |
| `0x08` | BITRATE_RANGE | `min_bps(u32 BE)` + `max_bps(u32 BE)` | Bounds the client may request |

`BITRATE_RANGE`'s `max_bps` reflects the ceiling for whichever transport `CAPS` was sent over —
the daemon advertises a higher value on USB (`--max-bitrate-usb`, default `100M`) than on network
(`--max-bitrate`, default `20M`), since USB links have far more headroom than typical networks. A
client that reconnects over a different transport should expect a different `BITRATE_RANGE` and
re-validate against it. `MAX_RESOLUTION`/`MAX_FRAMERATE` ceilings are shared across transports.

### `STNG` (client -> daemon)

```
"STNG"(4 bytes ASCII) + version(u8, currently 1) + entry_count(u8) + entries...

each entry: tag(u8) + length(u16 BE) + value(length bytes)
```

v1 tags:

| Tag | Name | Value layout |
|---|---|---|
| `0x01` | RESOLUTION | `width(u16 BE)` + `height(u16 BE)` |
| `0x02` | FRAMERATE | `fps(u8)` |
| `0x03` | CODEC | `codec_id(u8)` (`0x00`=H.264, `0x01`=H.265) |
| `0x04` | BITRATE | `bps(u32 BE)` |

An omitted tag means "use the daemon's default for that field." `entry_count = 0` is valid and means
"everything default, just connect" — the client should still send the message in that case rather
than skipping it, since the daemon is waiting for it (see Backward compatibility below).

### Client-side validation

Clients do not clamp or silently drop out-of-range settings. Before sending `STNG`, a client
validates its own configured settings (resolution, framerate, codec, bitrate) against the bounds
just advertised in `CAPS`; if anything the user has configured falls outside those bounds, the
client refuses to proceed with the connection and shows the user an error naming the offending
setting, rather than degrading silently to some other value.

### Clamping (daemon-side safety net)

Even though a conforming client is expected to refuse to connect rather than send an out-of-range
`STNG`, the daemon never applies `STNG` values as-is — it clamps against its own configured bounds
before starting the session's capture/encode pipeline, as a safety net for non-conforming or buggy
clients:

- resolution clamps to `[640x360, --max-width x --max-height]`
- framerate clamps to `[15, --max-framerate]`
- bitrate clamps to `[500 Kbps, --max-bitrate]` for sessions negotiated over the network transport,
  or `[500 Kbps, --max-bitrate-usb]` for sessions negotiated over USB — whichever ceiling matches
  the transport the `STNG` message arrived on
- a requested codec not in the daemon's advertised `CODECS` falls back to the daemon's default codec

### Backward compatibility

| Scenario | Behavior |
|---|---|
| Old client, new daemon | Client doesn't recognize `CAPS` and drops it, never sends `STNG`. Daemon waits ~2s for `STNG`, then proceeds with its CLI-configured defaults — identical to pre-negotiation behavior. |
| New client, old daemon | Client waits ~2s for `CAPS` after the control channel comes up; if it never arrives, the client assumes a legacy daemon with every feature available, sends no `STNG` at all, and connects exactly as before. Clients only ever send `STNG` in response to a received `CAPS`. |
| New client, new daemon | Full negotiation: `CAPS` then `STNG`, clamped as above. |

## Media Subprotocols

### Camera forwarding

#### Network camera

Camera frames use:

`"CAM" + frame_id(u32 BE) + chunk_idx(u16 BE) + total_chunks(u16 BE) + jpeg_chunk`

The daemon reassembles the JPEG and writes it to the virtual webcam.

#### USB camera

USB camera uses the same chunk layout as the network camera payload after the `"CAM"` prefix, but wrapped inside USB TCP control frames.

### Microphone forwarding

Mic packets use:

`"MIC" + seq(u32 BE) + opus_packet`

The daemon decodes Opus and feeds a virtual microphone source.

## Input Protocol

### Keyboard

Keyboard payloads use `"KEY" + type + payload`.

| Type | Byte | Description |
|---|---|---|
| Text | `0x01` | UTF-8 text |
| Special | `0x02` | Special key code |
| Combo | `0x04` | Modifiers + nested key payload |

Modifier bits:

- `0x01` = Ctrl
- `0x02` = Alt
- `0x04` = Super

### Touch

Touch payloads use `"TOUCH"` followed by packed contact data from the iPad surface.

### Mouse

Mouse payloads use `"MOUSE"` and include:

- motion
- button press/release
- wheel scroll

### Raw keyboard

External keyboard HID-like packets use `"RAWKEY"`.

### Peripheral attach/detach

`"PERIPH"` notifies mouse and keyboard presence.

### Game controllers

`"GPAD"` handles controller attach/detach/state updates.

## Audio / Camera / Speaker Model

### Speakers

- Linux creates a virtual sink named `screx_ipad` and captures from `screx_ipad.monitor`; Windows
  installs/enables the "Steam Streaming Speakers" device and loopback-captures it via WASAPI
- audio is sent to the client
- the speaker toggle can hard-detach the sink using the `SPKR` control message

### Microphone

- the client captures microphone audio and encodes it as Opus
- on Linux the daemon decodes and exposes a virtual microphone source via PipeWire/PulseAudio; on
  Windows it decodes into VB-Audio VB-CABLE's input

### Camera

- the client captures camera frames as JPEG
- on Linux the daemon writes them into a `v4l2loopback` webcam device; on Windows it writes them
  into a shared-memory buffer read by a registered DirectShow capture filter (`screx_vcam.dll`)

## Connection Health Model

The iPad app uses explicit session-health states such as:

- Idle
- Connecting
- Pairing
- Waiting for video
- Streaming
- Busy
- Connection refused
- Timed out
- Session stale, try again

These states are driven by transport events instead of only raw status strings.

## Virtual Devices

### Linux

The daemon may create:

- EVDI virtual display
- `uinput` virtual touchscreen
- `uinput` virtual keyboard
- `uinput` virtual mouse
- up to 4 `uinput` virtual gamepads
- `v4l2loopback` virtual webcam
- PipeWire / PulseAudio virtual sink for client speakers
- PipeWire virtual source for client microphone

### Windows

The daemon may create/enable:

- a virtual display devnode bound to a third-party Indirect Display Driver
- a DirectShow virtual webcam filter (`screx_vcam.dll`, registered under
  `CLSID_VideoInputDeviceCategory`)
- up to 4 virtual Xbox 360 gamepads via ViGEmBus
- a "Steam Streaming Speakers" devnode for client speaker output
- input injection via `SendInput` (no persistent device object)

Microphone forwarding on Windows relies on the separately-installed VB-Audio VB-CABLE rather than
a daemon-managed device — see [DAEMON_WINDOWS.md](DAEMON_WINDOWS.md).

## Notes on Compatibility

- Network mode is the main path for pairing and remote use.
- USB mode (iPad only) is lower-latency and more stable when tethered.
- The Linux virtual webcam uses `v4l2loopback`; some applications behave differently depending on `exclusive_caps` mode. See the v4l2loopback troubleshooting notes in the ArchWiki for compatibility context: [ArchWiki: v4l2loopback Troubleshooting](https://wiki.archlinux.org/title/V4l2loopback#Troubleshooting).
- See [DAEMON_WINDOWS.md](DAEMON_WINDOWS.md) for the Windows-specific drivers required and their compatibility notes.

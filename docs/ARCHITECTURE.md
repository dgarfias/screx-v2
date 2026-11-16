# Screx Architecture

## Overview

Screx is a remote display system built around a virtual monitor on a Linux host and a custom
low-latency iPad client.

At a high level:

- the daemon creates a virtual display (EVDI)
- the host compositor renders to that virtual display
- the daemon captures, encodes, and streams video/audio to the client
- the client sends back input, peripheral, and session-control messages; the daemon advertises
  which optional host features are available

See [DAEMON_LINUX.md](DAEMON_LINUX.md) and [CLIENT_IPAD.md](CLIENT_IPAD.md) for build/run details.

All traffic goes over the network (Wi-Fi):

- UDP for media
- encrypted TCP for pairing and control

## High-Level Data Flow

```text
Host desktop/compositor
    -> virtual monitor (EVDI)
    -> capture
    -> encode
    -> transport
    -> client decode/display

Client input/peripherals
    -> control transport
    -> daemon
    -> uinput + PipeWire/PulseAudio + v4l2loopback
```

The iPad sends **relative** pointer deltas under pointer lock when an external mouse/trackpad is
attached; touch stays absolute.

## Main Components

### Daemon

Key files:

- `daemon/src/main.rs`
- `daemon/src/capture.rs`
- `daemon/src/encode.rs`
- `daemon/src/stream_server.rs`
- `daemon/src/pairing.rs`
- `daemon/src/audio.rs`
- `daemon/src/camera.rs`
- `daemon/src/input.rs`

Responsibilities:

- create and manage the virtual display
- capture frames from it
- encode H.264/H.265
- stream media over the network
- manage pairing and session keys
- parse touch/keyboard/mouse/controller input messages
- expose optional virtual microphone, webcam, speaker, and gamepad devices

### Daemon (Linux backends)

Host-specific code lives in `daemon/src/platform/linux/`: EVDI virtual display (`evdi.rs`),
`uinput` virtual input devices (`uinput.rs`), PipeWire/PulseAudio (`pulse.rs`), the
`v4l2loopback` webcam (`v4l2.rs`), and UDP socket tuning (`net.rs`). See
[DAEMON_LINUX.md](DAEMON_LINUX.md) for the dependencies each backend needs.

### iPad app

Key files:

- `client/ipad/Screx/ScrexApp.swift`
- `client/ipad/Screx/PairingService.swift`
- `client/ipad/Screx/NetworkControlClient.swift`
- `client/ipad/Screx/StreamClient.swift`
- `client/ipad/Screx/Decoder.swift`
- `client/ipad/Screx/DisplayView.swift`
- `client/ipad/Screx/AudioPlayer.swift`
- `client/ipad/Screx/MicCapture.swift`
- `client/ipad/Screx/CameraCapture.swift`
- `client/ipad/Screx/Crypto.swift`

Responsibilities:

- manual connection UI
- PIN pairing flow
- capability negotiation (`CAPS`/`SETTINGS`) with the daemon
- network media receive path
- low-latency video decode and display
- local audio playback
- input forwarding
- microphone and camera forwarding
- connection-health UI and session state
- Stream Settings sheet for choosing resolution, framerate, codec, and bitrate before connecting

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

## Connection Lifecycle

### Network session

1. Client opens TCP to daemon.
2. Pairing or reconnect handshake runs.
3. Session key is established.
4. TCP control channel stays open.
5. Client opens UDP path and starts sending encrypted register packets.
6. Daemon authenticates first UDP packets and starts media.
7. First decoded frame on the client moves session into streaming state.

## Pairing Protocol

### New device flow

1. Client sends `SCREX_PAIR` + device ID + X25519 public key
2. daemon generates its own X25519 keypair and a 6-digit PIN
3. daemon sends `SCREX_PIN` + server public key
4. User enters the PIN in the client
5. Client sends `SCREX_ANSWER` + encrypted PIN
6. daemon verifies PIN
7. daemon derives and stores a pairing key
8. both sides derive a session key
9. daemon sends `SCREX_OK` + daemon device ID + session salt + HMAC

### Reconnect flow

1. Client sends `SCREX_HELLO` + device ID + client nonce (only if it has paired with some
   daemon before; otherwise it starts with the new device flow)
2. daemon loads the pairing key for that device, or replies `SCREX_REJECT` if it doesn't know it
3. both sides derive a fresh session key using nonces
4. daemon sends `SCREX_OK` + daemon device ID + server nonce + HMAC
5. client loads the key it stored for that daemon ID and verifies the HMAC

If the daemon rejects the HELLO, or the client has no key for that daemon ID (or it doesn't verify),
the client runs the new device flow on the same connection. Clients store pairing keys by daemon ID,
so a daemon that changes IP address keeps working.

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

- daemon -> client UDP uses `nonce_server`
- client -> daemon UDP uses `nonce_client`
- client -> daemon TCP control uses `nonce_control_client`
- daemon -> client TCP control uses `nonce_control_server`

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
| `codec_id` | u8 | Video: `0x00` = H.264, `0x01` = H.265. Audio: unused (`0x00`); audio is always Opus |
| `payload_len` | u16 BE | Payload size |
| `timestamp_ms` | u32 BE | Daemon timestamp |

Notes:

- video is split into chunks of 1400 bytes; every data chunk except a frame's last is full-size,
  and parity chunks are always full-size (receivers zero-pad short chunks before FEC recovery)
- optional Reed-Solomon parity is added for video
- a frame that fits in 255 data+parity chunks is one Reed-Solomon block. Larger frames (usually
  keyframes) are split into `n = ceil((total_data + total_parity) / 254)` blocks. Data chunks `0..total_data` and parity chunks `total_data..` are each divided
  into `n` contiguous runs as evenly as possible (earlier blocks take the extra chunk), and block
  `b`'s parity protects only block `b`'s data
- frames larger than 48 chunks are paced: sent in batches of 8 spread across half the frame
  interval rather than as one burst
- audio is small enough to go without FEC. Speaker audio is one 10 ms chunk per frame_id: 48 kHz
  stereo Opus at 128 kbps
- client-to-daemon UDP packets are `seq(u32 BE) + encrypted payload`; the payload is
  `type(u8) + body`, see "Client -> daemon UDP payloads" below
- the iPad retains incomplete video assemblies for 100 ms, then reconstructs recoverable frames
  from parity or discards them and requests recovery

### Network Control Transport (TCP)

Each encrypted TCP control frame is:

| Field | Size | Description |
|---|---|---|
| `length` | u32 BE | Frame body length |
| `seq` | u32 BE | Control sequence number |
| `ciphertext+tag` | variable | AES-GCM encrypted payload |

Every decrypted control payload is `type(u8) + body`. Type values live in
`daemon/src/protocol.rs` and `client/ipad/Screx/Protocol.swift`. Integers are big-endian.

Client -> daemon:

| Type | Name | Body |
|---|---|---|
| `0x01` | SETTINGS | see Capability Negotiation |
| `0x02` | PLI | empty |
| `0x03` | PING | empty; answered with PONG |
| `0x04` | DISCONNECT | empty |
| `0x10` | TOUCH | packed contacts, see Input Protocol |
| `0x11` | KEY | see Input Protocol |
| `0x12` | RAWKEY | see Input Protocol |
| `0x13` | MOUSE | see Input Protocol |
| `0x14` | PERIPH | `device(u8: 0x01 mouse, 0x02 keyboard) + attached(u8)` |
| `0x15` | GAMEPAD | `controller_id(u8) + kind(u8) + state` |
| `0x20` | SPEAKER | `enabled(u8)` |
| `0x21` | MIC | `enabled(u8)` |
| `0x22` | CAMERA | `width(u16) + height(u16) + fps(u16)`, all zero = off |

Daemon -> client:

| Type | Name | Body |
|---|---|---|
| `0x80` | CAPS | see Capability Negotiation |
| `0x81` | PONG | empty |

The daemon ignores unknown types and wrong-length bodies.

`PLI` requests an IDR and raises a capture-refresh signal, which makes the capture loop obtain or
resend a frame so a static desktop can still answer the recovery request.

### Client -> daemon UDP payloads

| Type | Name | Body |
|---|---|---|
| `0x01` | KEEPALIVE | empty; the first one tells the daemon where to send media |
| `0x02` | CAMERA | `frame_id(u32) + chunk_idx(u16) + total_chunks(u16) + jpeg_chunk` |
| `0x03` | MIC | `seq(u32) + opus_packet` |

## Capability Negotiation

When the control channel opens, the daemon sends `CAPS`: what it can do right now (from inexpensive
availability probes), its limits, and its host name. Later device or session initialization may
still fail. The client replies with `SETTINGS`, proposing session settings within those limits. Both
are fixed-layout messages; both sides ship together, so there is no versioning.

Features the operator switched off with `--no-camera`, `--no-mic`, `--no-speaker`, or
`--no-gamepad` are always reported as unavailable, and the daemon ignores requests to enable them.

### `CAPS` (daemon -> client)

| Offset | Field | Size | Meaning |
|---|---|---|---|
| 0 | camera | u8 | 1 = virtual webcam available |
| 1 | microphone | u8 | 1 = virtual microphone available |
| 2 | speaker | u8 | 1 = speaker forwarding available |
| 3 | gamepad_max | u8 | max simultaneous controllers, 0 = unsupported |
| 4 | codecs | u8 | bitmask, bit n set = codec id n can be encoded (`0x00` H.264, `0x01` H.265) |
| 5 | max_width | u16 | upper bound the client may request |
| 7 | max_height | u16 | upper bound the client may request |
| 9 | max_fps | u8 | upper bound the client may request |
| 10 | bitrate_min | u32 | bps |
| 14 | bitrate_max | u32 | bps |
| 18 | hostname | rest | UTF-8, may be empty |

Offsets are within the body, after the type byte.

### `SETTINGS` (client -> daemon)

| Offset | Field | Size | "Daemon default" value |
|---|---|---|---|
| 0 | width | u16 | 0 |
| 2 | height | u16 | 0 |
| 4 | fps | u8 | 0 |
| 5 | codec | u8 (`0x00` H.264, `0x01` H.265) | `0xFF` |
| 6 | bitrate | u32 (bps) | 0 |

The iPad's "Daemon default" options map to these values; the daemon then uses its CLI-configured
default for that field.

### Client-side validation

Clients do not clamp or silently drop out-of-range settings. Before sending `SETTINGS`, a client
validates its own configured settings (resolution, framerate, codec, bitrate) against the bounds
just advertised in `CAPS`; if anything the user has configured falls outside those bounds, the
client refuses to proceed with the connection and shows the user an error naming the offending
setting, rather than degrading silently to some other value.

### Clamping (daemon-side safety net)

Even though a conforming client is expected to refuse to connect rather than send an out-of-range
`SETTINGS`, the daemon never applies the values as-is — it clamps against its own configured bounds
before starting the session's capture/encode pipeline, as a safety net for non-conforming or buggy
clients:

- resolution clamps to `[640x360, --max-width x --max-height]`
- framerate clamps to `[15, --max-framerate]`
- bitrate clamps to `[500 Kbps, --max-bitrate]`
- a requested codec the daemon can't encode falls back to the daemon's default codec

### Ordering

Negotiation is mandatory. The daemon does not start capture/encode for a session until that
session's `SETTINGS` arrives; it waits for it indefinitely, and gives up only if the client
disconnects. Settings left over from an earlier connection are discarded when a new control channel
opens. The client
fails the connection if `CAPS` has not arrived within 5 seconds of the control channel coming up,
and only sends `SETTINGS` in reply to a received `CAPS`. Optional features (camera, microphone, speaker,
controllers) stay disabled in the iPad UI until `CAPS` arrives.

## Media Subprotocols

### Camera forwarding

Camera JPEGs are split into UDP `CAMERA` payloads (see above). The daemon reassembles each JPEG
and writes it to the virtual webcam.

### Microphone forwarding

Each UDP `MIC` payload carries one Opus packet and a mic-only sequence number. The daemon decodes
Opus and feeds a virtual microphone source. Because the number counts mic packets only, the daemon
can tell when one was lost: gaps of up to 5 packets are filled with Opus packet-loss concealment,
and late or duplicate packets are dropped.

## Input Protocol

### Keyboard

`KEY` bodies are `kind(u8) + payload`.

| Kind | Byte | Description |
|---|---|---|
| Text | `0x01` | UTF-8 text |
| Special | `0x02` | Special key code |
| Combo | `0x04` | Modifiers + nested key payload |

Modifier bits:

- `0x01` = Ctrl
- `0x02` = Alt
- `0x04` = Super

`KEY` combo packets do not define a Shift modifier bit. Shifted printable characters are carried in
the nested text payload; physical Shift key transitions, including external keyboards, use
`RAWKEY`.

### Touch

`TOUCH` bodies are packed contact data from the iPad surface.

### Mouse

`MOUSE` bodies carry:

- motion
- button press/release
- wheel scroll

### Raw keyboard

`RAWKEY` carries external keyboard HID usage press/release events.

### Peripheral attach/detach

`PERIPH` reports a physical mouse or keyboard being attached or detached. On detach the daemon
releases any mouse buttons or keys still held, so a device unplugged mid-press can't leave input
stuck on the host. The daemon does the same when a session ends.

### Game controllers

`GAMEPAD` handles controller attach/detach/state updates.

## Audio / Camera / Speaker Model

### Speakers

- the daemon creates a virtual sink named `screx_ipad` and captures from `screx_ipad.monitor`
- audio is encoded as Opus and sent to the client
- the `SPEAKER` control message attaches/detaches the virtual sink

### Microphone

- the client captures microphone audio and encodes it as Opus
- the daemon decodes it and exposes a virtual microphone source via PipeWire/PulseAudio

### Camera

- the client captures camera frames as JPEG
- the daemon writes them into a `v4l2loopback` webcam device

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

The daemon may create:

- EVDI virtual display
- `uinput` virtual touchscreen
- `uinput` virtual keyboard
- `uinput` virtual mouse
- up to 4 `uinput` virtual gamepads
- `v4l2loopback` virtual webcam
- PipeWire / PulseAudio virtual sink for client speakers
- PipeWire virtual source for client microphone

## Notes on Compatibility

- The Linux virtual webcam uses `v4l2loopback`; some applications behave differently depending on `exclusive_caps` mode. See the v4l2loopback troubleshooting notes in the ArchWiki for compatibility context: [ArchWiki: v4l2loopback Troubleshooting](https://wiki.archlinux.org/title/V4l2loopback#Troubleshooting).

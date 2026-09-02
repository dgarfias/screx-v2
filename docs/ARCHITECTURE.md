# Screx Architecture

## Overview

Screx is a remote display system built around a virtual monitor on a Linux host machine and a
custom low-latency client on iPad.

At a high level:

- the daemon creates a virtual display (EVDI on Linux)
- the host compositor renders to that virtual display
- the daemon captures, encodes, and streams video/audio to the client
- the client sends back its supported input, peripheral, and session-control messages; the daemon
  advertises which optional host features are available

See [DAEMON_LINUX.md](DAEMON_LINUX.md) for build/run/driver details and
[CLIENT_IPAD.md](CLIENT_IPAD.md) for the client.

The system is network-only:

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
    -> uinput+PipeWire+v4l2loopback
```

The iPad sends **relative** pointer deltas under pointer lock when an external mouse/trackpad is
attached; touch stays absolute.

## Main Components

### Daemon (shared core)

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
- parse touch/keyboard/mouse input messages
- expose optional virtual microphone, webcam, and speaker devices where required drivers are
  available

### Daemon (platform backend)

Platform-specific implementation lives in `daemon/src/platform/linux/` — EVDI virtual display,
`uinput` virtual input devices, PipeWire/PulseAudio, `v4l2loopback` webcam.

See [DAEMON_LINUX.md](DAEMON_LINUX.md) for the dependencies and drivers the backend needs.

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
- capability negotiation (`CAPS`/`STNG`) with the daemon
- network media receive path
- low-latency video decode and display
- local audio playback
- input forwarding
- microphone and camera forwarding
- connection-health UI and session state
- Stream Settings sheet for choosing resolution, framerate, codec, and bitrate before connecting

## Transport Model

- Pairing and control use a persistent TCP connection.
- Media uses UDP.
- Video and audio are sent daemon -> client over UDP.
- Mic and camera data are sent client -> daemon over UDP.
- Touch/keyboard/mouse/peripheral state travel over encrypted TCP control.

Why:

- UDP avoids head-of-line blocking for media.
- TCP control gives reliable ordering for input/state messages.

## Connection Lifecycle

1. Client opens TCP to daemon.
2. Pairing or reconnect handshake runs.
3. Session key is established.
4. TCP control channel stays open.
5. Client opens UDP path and starts sending encrypted register packets.
6. Daemon authenticates first UDP packets and starts media.
7. First decoded frame on the client moves session into streaming state.

## Pairing Protocol

Pairing happens only on the network path.

### New device flow

1. Client sends `SCREX_PAIR` + device ID + X25519 public key
2. daemon generates its own X25519 keypair and a 6-digit PIN
3. daemon sends `SCREX_PIN` + server public key
4. User enters the PIN in the client
5. Client sends `SCREX_ANSWER` + encrypted PIN
6. daemon verifies PIN
7. daemon derives and stores a pairing key
8. both sides derive a session key
9. daemon sends `SCREX_OK` + session salt + HMAC

### Reconnect flow

1. Client sends `SCREX_HELLO` + device ID + client nonce
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
| `codec_id` | u8 | `0x00` = H.264, `0x01` = H.265 |
| `payload_len` | u16 BE | Payload size |
| `timestamp_ms` | u32 BE | Daemon timestamp |

Notes:

- video is split into chunks of 1400 bytes
- optional Reed-Solomon parity is added for video
- audio is small enough to go without FEC; the Opus payload is a single packet per 10 ms frame
- client-to-daemon media packets prepend a 4-byte sequence number before encrypted payload
- the iPad retains incomplete video assemblies for 100 ms, then reconstructs recoverable frames
  from parity or discards them and requests recovery

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
- `SPKR`
- `HOST<hostname>`
- `CAPS` (daemon -> client, capability negotiation)
- `STNG` (client -> daemon, capability negotiation)
- `DISCONNECT`

`PLI` requests an IDR and raises a capture-refresh signal, letting the daemon obtain or resend a
frame to answer the recovery request.

## Capability Negotiation

Right after the daemon sends `HOST<hostname>` on the control channel, it sends `CAPS` with the
results of inexpensive availability probes. Later device or session initialization may still fail.
The client replies with `STNG`, proposing session settings within the bounds `CAPS` advertised.
Both messages travel inside the same control framing already used for `HOST` (AES-GCM control
frame via `send_control_frame`) — there is no new transport, socket, or crypto involved.

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

Capability availability is based on inexpensive platform probes performed for each connection. It
reflects known backend and driver availability but does not guarantee that later device or session
initialization cannot fail.

| Tag | Name | Value layout | Meaning |
|---|---|---|---|
| `0x01` | CAMERA | `available(u8 0/1)` | Virtual webcam availability probe result |
| `0x02` | MICROPHONE | `available(u8 0/1)` | Virtual microphone availability probe result |
| `0x03` | SPEAKER | `available(u8 0/1)` | Speaker/system-audio availability probe result |
| `0x05` | CODECS | `count(u8)` + `count` bytes of codec id (`0x00`=H.264, `0x01`=H.265) | Codec IDs for which the selected backend has a matching FFmpeg encoder registered; hardware or session initialization may still fail when streaming starts |
| `0x06` | MAX_RESOLUTION | `width(u16 BE)` + `height(u16 BE)` | Upper bound the client may request |
| `0x07` | MAX_FRAMERATE | `fps(u8)` | Upper bound the client may request |
| `0x08` | BITRATE_RANGE | `min_bps(u32 BE)` + `max_bps(u32 BE)` | Bounds the client may request |

Tag `0x04` is retired and reserved. The remaining tags keep their existing numbers (`0x01`, `0x02`,
`0x03`, `0x05`–`0x08`) and are not renumbered, since unknown tags are skipped by the TLV parser.

`BITRATE_RANGE`'s `max_bps` reflects the daemon's single bitrate ceiling (`--max-bitrate`, default
`20M`).

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
- bitrate clamps to `[500 Kbps, --max-bitrate]`
- a requested codec not in the daemon's advertised `CODECS` falls back to the daemon's default codec

### Backward compatibility

| Scenario | Behavior |
|---|---|
| Old client, new daemon | Client doesn't recognize `CAPS` and drops it, never sends `STNG`. Daemon waits ~2s for `STNG`, then proceeds with its CLI-configured defaults — identical to pre-negotiation behavior. |
| New client, old daemon | Client waits ~2s for `CAPS` after the control channel comes up; if it never arrives, the client assumes a legacy daemon with every feature available, sends no `STNG` at all, and connects exactly as before. Clients only ever send `STNG` in response to a received `CAPS`. |
| New client, new daemon | Full negotiation: `CAPS` then `STNG`, clamped as above. |

## Media Subprotocols

### Camera forwarding

Camera frames use:

`"CAM" + frame_id(u32 BE) + chunk_idx(u16 BE) + total_chunks(u16 BE) + jpeg_chunk`

The daemon reassembles the JPEG and writes it to the virtual webcam.

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

`KEY` combo packets do not define a Shift modifier bit. Shifted printable characters are carried in
the nested text payload; physical Shift key transitions, including external keyboards, use
`RAWKEY`.

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

## Audio / Camera / Speaker Model

### Speakers

- Linux creates a virtual sink named `screx_ipad` and captures from `screx_ipad.monitor`
- captured audio is encoded with libopus: 48 kHz, 2 channels, 10 ms frames (480 samples per
  channel), `Application::Audio`, 128 kbps, no inband FEC, no DTX
- the Opus packet is sent to the client as the UDP audio payload; the packet header (`flags` bit 1
  audio marker, timestamp, AES-GCM encryption) is unchanged — only the payload bytes are Opus
  instead of raw PCM
- the client decodes with `swift-opus` into 48 kHz stereo interleaved s16 and feeds the existing PCM
  jitter-buffer/drift-correction ring
- there is no negotiation and no `CAPS` tag for this — audio is always Opus
- the `SPKR` control message starts or stops the speaker-forwarding path: it attaches/detaches the
  virtual sink

### Microphone

- the client captures microphone audio and encodes it as Opus
- the daemon decodes and exposes a virtual microphone source via PipeWire/PulseAudio
- the daemon uses one Rust crate (`opus` 0.3, wrapping libopus) for both speaker encode and
  microphone decode

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
- `v4l2loopback` virtual webcam
- PipeWire / PulseAudio virtual sink for client speakers
- PipeWire virtual source for client microphone

## Notes on Compatibility

- Network mode is the only path for pairing and remote use.
- The virtual webcam uses `v4l2loopback`; some applications behave differently depending on `exclusive_caps` mode. See the v4l2loopback troubleshooting notes in the ArchWiki for compatibility context: [ArchWiki: v4l2loopback Troubleshooting](https://wiki.archlinux.org/title/V4l2loopback#Troubleshooting).

# Screx Architecture

## Overview

Screx is a Linux-to-iPad remote display system built around a virtual monitor on Linux and a custom low-latency client on iPad.

At a high level:

- the Linux daemon creates a virtual display via EVDI
- the desktop compositor renders to that virtual display
- the daemon captures, encodes, and streams video/audio to the iPad
- the iPad sends back touch, keyboard, mouse, controller, microphone, camera, and session control messages

The system supports two transports:

- **Network**
  - UDP for media
  - encrypted TCP for pairing and control
- **USB**
  - framed TCP over `iproxy`

## High-Level Data Flow

```text
Linux desktop/compositor
    -> EVDI virtual monitor
    -> capture
    -> encode
    -> transport
    -> iPad decode/display

iPad input/peripherals
    -> control transport
    -> Linux daemon
    -> uinput / PipeWire / v4l2loopback
```

## Main Components

### Linux daemon

Key files:

- `daemon/src/main.rs`
- `daemon/src/capture.rs`
- `daemon/src/encode.rs`
- `daemon/src/stream_server.rs`
- `daemon/src/pairing.rs`
- `daemon/src/usb.rs`
- `daemon/src/audio.rs`
- `daemon/src/camera.rs`
- `daemon/src/uinput.rs`

Responsibilities:

- create and manage the virtual display
- capture EVDI frames
- encode H.264/H.265
- stream media over network or USB
- manage pairing and session keys
- inject touch/keyboard/mouse/controller input
- expose virtual microphone and virtual webcam devices
- expose an audio sink for playback on the iPad

### iPad app

Key files:

- `app/Screx/ScrexApp.swift`
- `app/Screx/PairingService.swift`
- `app/Screx/NetworkControlClient.swift`
- `app/Screx/StreamClient.swift`
- `app/Screx/USBListener.swift`
- `app/Screx/Decoder.swift`
- `app/Screx/DisplayView.swift`
- `app/Screx/AudioPlayer.swift`
- `app/Screx/MicCapture.swift`
- `app/Screx/CameraCapture.swift`
- `app/Screx/Crypto.swift`

Responsibilities:

- manual connection UI for network and USB
- PIN pairing flow
- network media receive path
- USB media receive path
- low-latency video decode and display
- local audio playback
- input forwarding
- microphone and camera forwarding
- connection-health UI and session state

## Transport Model

### Network mode

- Pairing and control use a persistent TCP connection.
- Media uses UDP.
- Video and audio are sent daemon -> iPad over UDP.
- Mic and camera data are sent iPad -> daemon over UDP.
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
7. First decoded frame on iPad moves session into streaming state.

### USB session

1. iPad enters `Connect via USB` listening mode.
2. Daemon detects iOS device and starts `iproxy`.
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
- `APPBG`
- `APPFG`
- `SPKR`
- `HOST<hostname>`
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
- `APPBG`
- `APPFG`
- `SPKR<1-byte-flag>`
- `HOST<hostname>`
- `TOUCH...`
- `KEY...`
- `MOUSE...`
- `RAWKEY...`
- `PERIPH...`
- `GPAD...`
- `CAM...`
- `MIC...`

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

- Linux creates a virtual sink named `screx_ipad`
- daemon captures from `screx_ipad.monitor`
- audio is sent to the iPad
- the speaker toggle can hard-detach the sink using the `SPKR` control message

### Microphone

- iPad captures microphone audio
- encodes as Opus
- daemon decodes and exposes a virtual microphone source

### Camera

- iPad captures camera frames
- frames are JPEG-compressed
- daemon writes them into a `v4l2loopback` webcam device

## Background Session Mode

When the iPad backgrounds:

- app suspends decoder rendering
- app sends `APPBG`
- daemon marks client as backgrounded
- daemon pauses video encode/send while keeping the session alive
- audio/control can continue

When the iPad returns to foreground:

- app sends `APPFG`
- daemon clears background mode
- daemon forces a refresh / IDR
- video resumes

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
- Background audio mode

These states are driven by transport events instead of only raw status strings.

## Virtual Devices on Linux

The daemon may create:

- EVDI virtual display
- `uinput` virtual touchscreen
- `uinput` virtual keyboard
- `uinput` virtual mouse
- up to 4 `uinput` virtual gamepads
- `v4l2loopback` virtual webcam
- PipeWire / PulseAudio virtual sink for iPad speakers
- PipeWire virtual source for iPad microphone

## Notes on Compatibility

- Network mode is the main path for pairing and remote use.
- USB mode is lower-latency and more stable when tethered.
- The virtual webcam uses `v4l2loopback`; some applications behave differently depending on `exclusive_caps` mode. See the v4l2loopback troubleshooting notes in the ArchWiki for compatibility context: [ArchWiki: v4l2loopback Troubleshooting](https://wiki.archlinux.org/title/V4l2loopback#Troubleshooting).

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
- `daemon/src/display_manager.rs` — transactional topology and presentation acknowledgement
- `daemon/src/pipeline.rs` — independently owned capture/encode/send workers
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

- `client/Screx/ScrexApp.swift`
- `client/Screx/PairingService.swift`
- `client/Screx/NetworkControlClient.swift`
- `client/Screx/StreamClient.swift`
- `client/Screx/Decoder.swift`
- `client/Screx/DisplayView.swift`
- `client/Screx/AudioPlayer.swift`
- `client/Screx/MicCapture.swift`
- `client/Screx/CameraCapture.swift`
- `client/Screx/Crypto.swift`
- `client/Screx/MediaPacket.swift` — media header parsing and per-display stream timelines
- `client/Screx/DisplayTopology.swift` — display modes, routing state, per-host preferences
- `client/Screx/ExternalDisplay.swift` — external-display scene and video window
- `client/Screx/DisplaySettingsView.swift` — Displays menu
- `client/Screx/ExternalControlPanel.swift` — laptop mode panel and pointer lock
- `client/Screx/LaptopKeyboardLayout.swift`, `LaptopKeyboardView.swift` — drawn laptop keyboard
- `client/Screx/DeviceDisplayIdentity.swift` — iPad model names for EDID

Responsibilities:

- manual connection UI
- PIN pairing flow
- capability negotiation (`CAPS`/`SETTINGS`) with the daemon
- network media receive path
- low-latency video decode and display
- local audio playback
- input forwarding
- microphone and camera forwarding
- external-display routing (Internal only, External only, Both) and laptop mode
- connection-health UI and session state
- host tiles with per-host settings (address, resolution, framerate, codec, bitrate) and per-host pairing

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

1. Client sends `SCREX_PAIR` + device ID + X25519 public key + name length (1 byte) + client name
   (UTF-8, up to 63 bytes; a label shown in the daemon's logs and `screx unpair` list)
2. daemon generates its own X25519 keypair and a 6-digit PIN
3. daemon sends `SCREX_PIN` + server public key
4. User enters the PIN in the client
5. Client sends `SCREX_ANSWER` + encrypted PIN
6. daemon verifies PIN
7. daemon derives and stores a pairing key
8. both sides derive a session key
9. daemon sends `SCREX_OK` + daemon device ID + session salt + HMAC

### Reconnect flow

1. Client sends `SCREX_HELLO` + device ID + client nonce (only if it has a key for this host
   tile; otherwise it starts with the new device flow)
2. daemon loads the pairing key for that device, or replies `SCREX_REJECT` if it doesn't know it
3. both sides derive a fresh session key using nonces
4. daemon sends `SCREX_OK` + daemon device ID + server nonce + HMAC
5. client verifies the HMAC with the key stored for the host tile

If the daemon rejects the HELLO, or the client's key doesn't verify, the client runs the new device
flow on the same connection.

On the iPad, every host tile has its own random 16-byte device ID and its own pairing key, stored in
the Keychain under the tile. Two tiles that reach the same computer (for example through two IP
addresses) are two separate pairings, and removing a tile deletes its key, so adding it back asks
for the PIN again. Editing a tile's address keeps its ID and key. The iPad's own IP address plays no
part in pairing.

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

Each UDP packet starts with a 22-byte header (followed by encrypted payload and a 16-byte tag):

| Field | Size | Description |
|---|---|---|
| `frame_id` | u32 BE | Frame number |
| `chunk_idx` | u16 BE | Chunk number within frame |
| `total_data` | u16 BE | Number of data chunks |
| `total_parity` | u16 BE | Number of parity chunks |
| `flags` | u8 | Bit 0 = IDR, Bit 1 = audio, Bit 4 = external video, Bit 7 = heartbeat |
| `codec_id` | u8 | Video: `0x00` = H.264, `0x01` = H.265. Audio: unused (`0x00`); audio is always Opus |
| `payload_len` | u16 BE | Payload size |
| `timestamp_ms` | u32 BE | Daemon timestamp |
| `generation` | u32 BE | Nonzero sender namespace, unique across sender recreation |

Notes:

- all 22 header bytes are authenticated as AAD, including heartbeats. The server nonce is
  `0x00 + frame_id(u32) + chunk_idx(u16) + flags(u8) + generation(u32)`, all big-endian
- each video sender, audio sender, and heartbeat sender reserves a process-unique generation.
  Frame IDs are reserved before encryption (even if transmission fails), and never wrap.
  Recreating a sender always allocates a fresh generation; namespace exhaustion fails closed
- internal video uses display ID 0 and external video uses display ID 1 (flag `0x10`). Each
  has independent FEC, frame completion, PLI throttling, codec parameters, and decoder state.
  Only authenticated newer generations may reset those states; retired generations are ignored
- heartbeats use flags `0x80`, zero data/parity counts, and an incrementing frame ID, not a
  fixed frame ID/nonce. Audio and heartbeats never set the external-video flag
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
- the iPad retains incomplete video assemblies for 50 ms, then reconstructs recoverable frames
  from parity or discards them and requests recovery

### Network Control Transport (TCP)

Each encrypted TCP control frame is:

| Field | Size | Description |
|---|---|---|
| `length` | u32 BE | Frame body length |
| `seq` | u32 BE | Control sequence number |
| `ciphertext+tag` | variable | AES-GCM encrypted payload |

Every decrypted control payload is `type(u8) + body`. Type values live in
`daemon/src/protocol.rs` and `client/Screx/Protocol.swift`. Integers are big-endian.

Client -> daemon:

| Type | Name | Body |
|---|---|---|
| `0x01` | SETTINGS | see Capability Negotiation |
| `0x02` | PLI | `display_id(u8) + generation(u32)` |
| `0x03` | PING | empty; answered with PONG |
| `0x04` | DISCONNECT | empty |
| `0x05` | DISPLAY_PRESENTED | `revision(u32) + display_id(u8) + generation(u32)` |
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
| `0x82` | DISPLAY_STATUS | see Runtime Display Topology |

The daemon ignores unknown types and wrong-length bodies.

`PLI` requests an IDR and raises a capture-refresh signal for only the named display and current
generation. Stale generations and invalid display IDs are ignored. This makes the capture loop
obtain or resend a frame so a static desktop can still answer the recovery request. Adaptive
bitrate/FEC tuning is independent per display. The 22-byte header and display-scoped PLI are a
breaking protocol change: daemon and iPad client must be updated together.

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
| 10 | enabled_mask | u8 | no default: 1 = internal, 2 = external, 3 = both |
| 11 | external_width | u16 | 0 |
| 13 | external_height | u16 | 0 |
| 15 | revision | u32 | nonzero, monotonically increasing within the session |
| 19 | external_fps | u8 | 0 = follow internal FPS |
| 20 | internal_display_name | 13 ASCII bytes | space-padded EDID name, required |

The iPad's "Daemon default" options map to these values; the daemon then uses its CLI-configured
default for that field.

The body is exactly 33 bytes. Width/height/FPS at offsets 0/2/4 describe the internal display.
External FPS is independent, inheriting internal FPS only when omitted. Codec and the base bitrate
target are shared settings; the target applies to **each** enabled video pipeline. Each pipeline
adapts bitrate/FEC independently from that target. Both
resolutions are independently clamped against the same advertised limits. Each width/height
pair must either be specified together or uses daemon defaults. Masks outside 1–3, zero
revisions, and legacy 10-/19-/20-byte bodies are rejected. The daemon's maximum FPS applies per output.
Monitor names must be nonempty printable ASCII, without single quotes or backslashes, so the same
validated identity can safely be used in both EDID and GNOME's touchscreen output mapping.

### Runtime Display Topology

`SETTINGS` may also be sent while streaming. The display manager coalesces pending requests and
ignores revisions older than or equal to the latest request. Settings, presentation ACKs, and
outbound status messages are all tagged with their owning network session internally, so delayed
work from a disconnected client cannot affect its replacement.

Each enabled output has its own capture, encoder, UDP sender, recovery controls, and stop/join
lifecycle. Adding/removing another output does not restart an unchanged pipeline. New or
reconfigured outputs start as candidates alongside retained committed monitors. The daemon reports
`streaming` after successfully sending a candidate keyframe, but retains the old topology until
the iPad acknowledges that generation's first frame is ready for display. Unchanged, already
presented generations need no new ACK. Only after **all** requested outputs are ready does the
daemon retire disabled/replaced monitors and report the committed topology.

`DISPLAY_STATUS` body (15 fixed bytes plus UTF-8 detail):

| Offset | Field | Size |
|---|---|---|
| 0 | revision | u32 |
| 4 | display_id | u8: 0 internal, 1 external |
| 5 | phase | u8: 0 streaming, 1 committed, 2 failed, 3 removed |
| 6 | active_mask | u8: currently committed outputs |
| 7 | generation | u32: 0 if no pipeline for this output |
| 11 | width | u16: 0 if absent |
| 13 | height | u16: 0 if absent |
| 15 | detail | UTF-8 remainder, bounded error detail |

**Monitor order.** Each new monitor takes the lowest free EVDI card, and GNOME numbers monitors by
card, so plug order decides which is 1 and 2. Whenever the requested topology has both monitors and
the internal one must (re)start (in practice External only → Both),
the daemon unplugs every monitor (stop all, then join), plugs the internal one, and plugs the
external one only after the internal pipeline is streaming. The internal monitor is therefore
always 1 and the external 2. This path does not keep old monitors alive during the switch; if it
fails or times out, the previous monitors are plugged back in the same order, and the failure
status reports that restored mask. Other changes (adding an external monitor to a running internal
one, removing one, external-only resolution/FPS) keep the transactional behavior below.

On candidate failure, cancellation, or a 15-second presentation timeout, candidates are stopped
and joined without removing the previously committed monitors. If a same-output replacement had
advanced the client's generation, the retained worker reissues a keyframe under a fresh namespace
so stale-generation filtering cannot prevent rollback. Failed requests report both retained output
states. A request superseding an in-flight transition cancels its candidates first; stale ACKs
cannot commit it. Disconnect/shutdown signals every worker before joining any of them. If no
committed output survives an initialization failure, the iPad reports the error and disconnects.

The renderer acknowledges readiness rather than merely receiving or reassembling a frame. On
iOS 17.4+ it checks `AVSampleBufferDisplayLayer.isReadyForDisplay`; earlier supported iOS releases
use the layer's rendering status. An ACK also requires the corresponding surface to be attached
to a window; decoding an offscreen external stream cannot retire a visible monitor.

### iPad external-display scenes

The app starts in Internal only, even with a monitor connected. An external noninteractive scene
is configured in the manifest for earlier iPadOS releases and registered as a scene accessory
on the primary controller on iPadOS 27+. The coordinator owns at most one external video window
and never makes it key; keyboard, mouse, trackpad, and app controls stay on the iPad.

The in-session Displays button (also available with the pill collapsed) selects Internal only,
External only, or Both. The Displays menu edits only external resolution and target FPS; the iPad's
resolution and FPS remain those from the host's settings. Changing external FPS replaces only its pipeline.
The selected mode, external resolution, and external FPS are stored per daemon hostname in
UserDefaults. On connect, the remembered mode is used when a monitor is attached (otherwise
internal-only until it appears), the remembered resolution is reused only while iPadOS still
reports it, and the display's recommended mode is the fallback and default. A monitor appearing
mid-session reapplies the host's remembered external mode once.
Both uses independent desktops, not a mirrored video layer. Requested and committed output masks
keep retiring surfaces attached during transitions. Once Internal only commits, the external
window is released entirely so iPadOS can resume normal screen mirroring. External scene teardown
never invokes the primary scene's background/disconnect handler. Unplugging requests Internal only
on the same network session. Mode changes cancel in-flight touch gestures and release held input.

External only keeps the normal interface (pill, floating trackpad, iOS keyboard) over a black
placeholder with a **Switch to laptop mode** button. The floating trackpad is enabled automatically
on entry; leaving restores its previous enabled state and layout. No touchscreen coordinates are
forwarded, and indirect pointer touches from a connected mouse/trackpad are ignored by trackpad
surfaces. A `PointerLockRegion` behind the placeholder/panel requests pointer lock and hides the
iPad cursor while a physical mouse is connected, replacing the video view that owns this in the
other modes.

Laptop mode is per-session state (`StreamViewModel.isLaptopMode`), off on every entry to External
only and cleared when it ends. `ExternalControlPanel` then replaces the placeholder, pill and iOS
keyboard with a top bar (options left, inline connection info right), `LaptopKeyboardView`, and the
trackpad at the bottom. `LaptopKeyboardView` is a UIKit surface that
fires on touch-down for multi-touch typing and ignores indirect-pointer touches. Layouts are pure
data in `LaptopKeyboardLayout.swift` (15-unit rows; the function row is esc, F1–F12, home, end,
ins, del). The initial layout comes from
`UITextInputMode.activeInputModes` primary languages, then the device language, then US; a stored
choice wins. iPadOS exposes keyboard languages, not key geometry, so the language→layout mapping is
an approximation with a manual 🌐 override. Keys reuse existing KEY messages: characters as text
(composed client-side for dead keys), editing/navigation as special codes, and Ctrl/Alt/Super as
combos. Special codes `0x10`–`0x1B` are F1–F12. Modifier masks are ctrl `0x01`, alt `0x02`, super
`0x04`, shift `0x08` (shift is only needed for special keys; characters are already shifted).
Modifiers are held per touch, as on a physical keyboard, and an unused modifier release sends the
lone key.

The external coordinator reads `UIScreen.availableModes`, `preferredMode`, and
`maximumFramesPerSecond`, refreshing on screen-mode changes. These expose resolution modes and a
screen-wide FPS ceiling, not the monitor's complete EDID or per-resolution refresh-rate pairs.
The resolution picker marks the preferred mode Recommended and lists only resolutions exposed by
iPadOS, filtered by daemon limits. No standard/scaled/custom sizes are synthesized. If none fit,
external activation is disabled instead of inventing a mode. Target FPS choices respect both screen
and daemon ceilings. These select the remote stream's format; they do not switch physical monitor timing.

Audio uses the system-selected output route. With mic off, the session uses `.playback` so
output-only HDMI/AirPlay/A2DP routes remain eligible; mic on uses `.playAndRecord` for duplex
input/output, subject to iPadOS route restrictions. Audio engine configuration changes restart
requested playback if the engine was stopped by a route-format change. Output route names/types
are logged for physical HDMI troubleshooting; no speaker-output override is forced.

`DeviceDisplayIdentity.swift` maps 107 released iPad hardware identifiers (46 model/size groups,
snapshot 2026-11-15) to full marketing names, known diagonal sizes, and 13-byte ASCII EDID labels.
Sources are Apple's model/specification pages and the IPSW hardware-ID catalog, referenced in the
file. Unknown identifiers and other device families use Screx. For example, iPad (A16) 11″ uses
`iPad A16 11"` in EDID. The external monitor is always `iPad External`, exactly 13 ASCII characters.
UIKit supplies no physical external diagonal, so no inch suffix is inferred from pixels. Instead of
guessing DPI, the EDID sets 16×10 cm, the specification's marker for "aspect ratio encoded instead
of size". Mutter/gnome-control-center recognize that marker, keep the output at 100% scale, and
name it `<three-letter vendor> <product descriptor>`, so GNOME shows `SRX iPad A16 11"` and
`SRX iPad External`. With the size fields left at zero (the previous behavior), Mutter ignores the
product descriptor and shows only `SRX`. Product IDs and serials remain stable and distinct; the
internal touchscreen's GNOME mapping is updated to the session's EDID name before capture starts.

The compact connection overlay is four short lines: hostname, ↓/↑ transfer rates, RTT and jitter,
then a plain Disconnect button. It shows no codec or display-mode row; display selection belongs
only in Displays.

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

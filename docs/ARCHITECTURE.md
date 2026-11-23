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

All traffic goes over the network:

- one QUIC connection over UDP, with separate reliable control and input streams
- unreliable QUIC datagrams for media

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
    -> uinput + screx_snd ALSA cards + v4l2loopback
```

The iPad sends **relative** pointer deltas under pointer lock when an external mouse/trackpad is
attached; touch stays absolute.

## Main Components

### Daemon

Key files:

- `daemon/src/main.rs`
- `daemon/src/capture.rs`
- `daemon/src/display_manager.rs` — transactional topology and presentation acknowledgement
- `daemon/src/pipeline.rs` — independently owned capture/encode workers with a decoupled per-display sender thread
- `daemon/src/encode.rs` — hardware (VA-API, NVENC) and software (libx264/libx265) encoding, BT.709 color conversion, 10-bit HEVC Main10 with NV12 fallback
- `daemon/csrc/drm_helper.c` — DRM PRIME DMA-BUF export for EVDI primary & cursor planes and zero-copy vblank ack
- `daemon/csrc/vaapi_vpp_helper.c` — single-pass VA-API VPP BT.709 conversion and premultiplied-alpha GPU cursor compositing
- `daemon/src/stream_server.rs`
- `daemon/src/quic_transport.rs` — QUIC/TLS, authentication, session admission, streams and datagrams
- `daemon/src/cpace.rs` — CPace and the eight-digit PIN
- `daemon/src/pairing.rs` — `screx unpair`
- `daemon/src/media_sequence.rs` — media frame IDs and generations
- `daemon/src/audio.rs`
- `daemon/src/camera.rs`
- `daemon/src/input.rs`

Responsibilities:

- create and manage the virtual display
- capture frames from it
- encode H.264/H.265
- stream media over the network
- manage pairing identities and authenticated QUIC sessions
- parse touch/keyboard/mouse/controller input messages
- expose optional virtual microphone, webcam, speaker, and gamepad devices

### Daemon (Linux backends)

Host-specific code lives in `daemon/src/platform/linux/`: EVDI virtual display (`evdi.rs`),
`uinput` virtual input devices (`uinput.rs`), ALSA sound cards (`screx_snd.rs` and `kernel/screx_snd/`), the
`v4l2loopback` webcam (`v4l2.rs`). QUIC owns network sockets. See
[DAEMON_LINUX.md](DAEMON_LINUX.md) for the dependencies each backend needs.

### iPad app

Key files:

- `client/Screx/ScrexApp.swift`, `ContentView.swift`, `StreamPresets.swift`
- `client/Screx/PairingService.swift`
- `client/Screx/NetworkControlClient.swift`
- `client/Screx/StreamClient.swift`
- `client/Screx/FEC.swift`, `GF256Kernel.c`, `GF256Kernel.h` — flat-matrix Reed-Solomon recovery with ARM64 NEON SIMD GF(2^8) kernels
- `client/Screx/Decoder.swift`
- `client/Screx/DisplayView.swift`
- `client/Screx/AudioPlayer.swift`
- `client/Screx/MicCapture.swift`
- `client/Screx/CameraCapture.swift`
- `client/Screx/QUICSession.swift` — QUIC connection, streams, datagrams and certificate pinning
- `client/Screx/CPace.swift`, `CPacePrimitives.c` — CPace, using the adapter in `shared/`
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

Everything travels over one QUIC v1 connection (TLS 1.3, ALPN `screx-quic-1`) on UDP port 9000.
The daemon binds `[::]`, which also accepts IPv4. There is no TCP listener; firewalls must allow
UDP 9000.

- The first client-initiated bidirectional stream carries authentication, then control.
- The second carries input, so input never waits behind control messages.
- RFC 9221 datagrams carry video, speaker audio, microphone, camera and audio reports. They are
  never retransmitted.
- QUIC owns encryption, replay protection, retransmission of stream data, flow and congestion
  control (BBR on the daemon), path validation and liveness. The daemon sends a keep-alive every 2 s and both sides
  close the connection after 15 s without traffic. Zero-RTT is disabled.
- A stream is ordered, so a lost packet still delays later messages **on the same stream**.
- Per-display encoder adaptation combines QUIC transport feedback (RTT inflation, `cwnd`/BDP, congestion
  events, and send-buffer backpressure) with PLI recovery signals to adapt bitrate (down to 500 Kbps) and FPS.

Network.framework opens an unused bootstrap stream before the control and input streams. The
daemon allows three bidirectional streams, keeps that placeholder open, and authenticates the
first stream that sends HELLO.

Nothing is dispatched before authentication finishes: no devices start, no input is injected and
no datagrams are accepted. Afterwards the daemon rejects control messages on the input stream and
input messages on the control stream.

## Connection Lifecycle

1. The client opens the QUIC connection. QUIC setup fails after 8 seconds if the daemon can't be
   reached.
2. Authentication on the first stream: eight-digit CPace pairing for a new device, or a signature
   with the saved identity for a known one (see Pairing Protocol).
3. The daemon answers `READY` if the session is free, or `BUSY` if another device holds it. A
   device reconnecting while its own earlier session is still open (app restarted, network
   changed) replaces that session instead of waiting for it to time out. The iPad gives up after
   8 seconds without `READY` or `BUSY`.
4. After `READY`, the client opens the input stream and the datagram flow, and the daemon sends
   `CAPS` on the control stream.
5. The first decoded video frame moves the client into the streaming state.

The daemon runs each connection in its own task. It validates the source address with QUIC Retry
and limits concurrent authentication attempts both globally and per source. A separate global
limit allows only one PIN exchange at a time and at most one new PIN attempt per second.
Already-paired devices can authenticate while someone is entering a PIN.

## Pairing Protocol

### Identities

- The daemon keeps a self-signed TLS identity in `quic-identity-v1.json` and the public signing
  keys of paired devices in `quic-pairings-v1.json`, both in its config directory
  (`/root/.config/screx` when run as root).
- Each iPad host tile has its own device ID, Ed25519 signing key and the SHA-256 fingerprint of
  the host's TLS certificate, stored in the Keychain.

This is server-certificate pinning plus application-level client authentication, not TLS client
certificates. If the daemon's identity changes, remove the host tile and add it again to pair with
the new identity. `screx unpair <id>` removes one paired device and `screx unpair --all` removes
all of them.

### New devices

Pairing uses **CPACE-X25519-SHA512** (`draft-irtf-cfrg-cpace-21`, initiator/responder
transcript). The daemon prints an eight-digit PIN shown as `1234-5678`; leading zeroes count.
CPace's context binds:

1. the protocol identifier
2. the client and server device IDs
3. the client's public Ed25519 signing key
4. the SHA-256 fingerprint of the server's actual TLS certificate
5. this connection's TLS exporter (`EXPORTER-screx-quic-pair-v1`, 32 bytes)

The daemon supplies a fresh random 32-byte session ID. Both sides confirm the result with
role-separated HMAC-SHA256 tags before storing anything, so an unauthenticated TLS connection is
never enough to trust a certificate or use the host. Each PIN allows one attempt and expires after
120 seconds; a wrong PIN or failed confirmation closes the connection, and the next attempt gets a
new PIN.

The Elligator2 map CPace needs comes from Monocypher 4.0.2 through a small adapter in `shared/`,
compiled into both the daemon and the iPad app.

### Known devices

A reconnect requires the pinned server certificate and an Ed25519 signature over the fresh session
ID and the same binding, which includes the TLS exporter. A captured proof can't authenticate
another connection.

### Authentication frames

Frames use the stream framing below (`length(u32 BE) + body`).

| Type | Direction | Body after type |
|---|---|---|
| `e0` HELLO | client → daemon | device ID (16), client public key (32), UTF-8 name (1..63) |
| `e1` CHALLENGE | daemon → client | known (1), server ID (16), certificate hash (32), random session ID (32), daemon PAKE share (32, new devices only) |
| `e2` PROOF | client → daemon | Ed25519 signature (64), known devices only |
| `e3` PAIR | client → daemon | client PAKE share (32), client confirmation (32) |
| `e4` PAIRED | daemon → client | daemon confirmation (32) |
| `e5` READY | daemon → client | empty; the session is this client's |
| `e6` BUSY | daemon → client | empty; another device holds the session, and the client closes |

After `READY` the first stream carries control messages.

## Encryption Model

QUIC's TLS 1.3 encrypts and authenticates every stream and datagram; the application adds no
cipher, tag or nonce of its own. Frame IDs and generations are decoder timelines, not nonces.

## Protocol Details

### Network Media Transport (QUIC datagrams)

Each server media datagram contains a 22-byte header followed by its payload,
inside QUIC's authenticated encryption:

| Field | Size | Description |
|---|---|---|
| `frame_id` | u32 BE | Frame number |
| `chunk_idx` | u16 BE | Chunk number within frame |
| `total_data` | u16 BE | Number of data chunks |
| `total_parity` | u16 BE | Number of parity chunks |
| `flags` | u8 | Bit 0 = IDR, Bit 1 = audio, Bit 4 = external video |
| `codec_id` | u8 | Video: `0x00` = H.264, `0x01` = H.265. Audio: unused (`0x00`); audio is always Opus |
| `payload_len` | u16 BE | Payload size |
| `timestamp_ms` | u32 BE | Daemon timestamp |
| `generation` | u32 BE | Nonzero sender namespace, unique across sender recreation |

Notes:

- all header and payload bytes are protected by QUIC's authenticated encryption
- each video and audio sender reserves a process-unique generation.
  Frame IDs are reserved before transmission (even if transmission fails), and never wrap.
  Recreating a sender always allocates a fresh generation; namespace exhaustion fails closed
- internal video uses display ID 0 and external video uses display ID 1 (flag `0x10`). Each
  has independent FEC, frame completion, PLI throttling, codec parameters, and decoder state.
  Only authenticated newer generations may reset those states; retired generations are ignored
- video is split into chunks sized to the negotiated datagram limit (between 1000 and 1350 bytes);
  1000 + 22 = 1022 bytes is the minimum required path size, checked at connection setup. Every data chunk except a frame's last is full-size,
  and parity chunks are always full-size (receivers infer the frame's shard size and zero-pad short chunks before FEC recovery)
- optional Reed-Solomon parity is added for video
- a frame that fits in 255 data+parity chunks is one Reed-Solomon block. Larger frames (usually
  keyframes) are split into `n = ceil((total_data + total_parity) / 254)` blocks. Data chunks `0..total_data` and parity chunks `total_data..` are each divided
  into `n` contiguous runs as evenly as possible (earlier blocks take the extra chunk), and block
  `b`'s parity protects only block `b`'s data
- large frames (and medium frames below 10 Mbps) are paced in small batches rather than sent as one burst
- the daemon's QUIC datagram send buffer is 64 KB. When it fills, video waits (backpressure)
  instead of dropping chunks already queued, and part of the buffer is kept free for audio
- audio goes without FEC; it carries adaptive redundant copies of earlier packets when loss is observed. Speaker audio is
  one datagram per frame_id (always `chunk_idx` 0, `total_data` 1), each holding the 10 ms Opus
  packet with that frame_id plus optional copies of earlier ones: 48 kHz stereo Opus at 48, 64, 96, 128 (default),
  192 or 256 kbps, selected in the host's settings. See "Speaker audio" below
- client-to-daemon datagrams are `type(u8) + body`
- the iPad retains incomplete video assemblies with an adaptive inactivity timeout (minimum 80 ms,
  scaling with measured RTT and jitter) and briefly holds out-of-order frames before declaring a gap,
  then reconstructs recoverable frames from parity (using flat contiguous matrices and an ARM64 NEON
  SIMD GF(2^8) kernel in `GF256Kernel.c`) or discards them and requests recovery via a `PLI` datagram

### Video Capture & Encoding Pipeline

Each enabled display runs two decoupled threads in `daemon/src/pipeline.rs`:

1. **Capture + encode thread (`capture-{id}`):**
   - **Zero-copy DRM PRIME path (default on VA-API):** When the compositor commits a frame to EVDI,
     `evdi.rs` + `drm_helper.c` export the active CRTC primary framebuffer (and, when active, the
     standard KMS `DRM_PLANE_TYPE_CURSOR` plane with its `CRTC_X/Y/W/H` atomic properties) as
     DMA-BUF file descriptors, immediately closing temporary GEM handles. `DmaBufVppScaler` maps
     them directly into VA-API surfaces (`av_hwframe_map`), runs a single-pass `VAEntrypointVideoProc`
     blit (`vaapi_vpp_helper.c`) to convert sRGB → BT.709 limited-range YUV and alpha-blend the
     hardware cursor straight into the encoder's surface pool, and then acknowledges EVDI's vblank
     without CPU pixel readback.
   - **Automatic CPU fallback:** On non-VA-API backends (NVENC, software), `--no-vpp`,
     `--no-zero-copy` (`SCREX_ZEROCOPY=0`), or if DRM export / VA-API import fails, capture falls
     back transparently to `evdi_grab_pixels` + VA-API VPP `hwupload` or multi-band CPU `swscale`.
   - **Encoding:** VA-API H.265 prefers 10-bit `P010` (`HEVC Main10`) with silent fallback to 8-bit
     `NV12` on unsupported GPUs; H.264 uses 8-bit `NV12`. All converter and encoder paths signal
     matching BT.709 limited-range (`MPEG`) color metadata. The default GOP is infinite (`--keyframe 0`,
     PLI-only IDRs) to avoid periodic bitrate spikes on constrained links.
2. **Sender thread (`send-{id}`):**
   - Fed by a bounded 2-slot `sync_channel` carrying zero-copy `EncodedAccessUnit` buffers (plus
     ordered sequence/FPS control updates). Reed-Solomon FEC encoding and paced QUIC datagram sends
     overlap with the next frame's capture and GPU encode.

### Network Control and Input Transport (QUIC streams)

Each stream message is:

| Field | Size | Description |
|---|---|---|
| `length` | u32 BE | Frame body length |
| `payload` | variable | `type(u8) + body`, protected by QUIC |

Bodies are 1..65536 bytes. Messages `0x10` through `0x15` travel on the input stream, all other
messages on control. Type values live in
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
bitrate/FEC tuning is independent per display.

### Client -> daemon QUIC datagram payloads

| Type | Name | Body |
|---|---|---|
| `0x02` | CAMERA | `frame_id(u32) + chunk_idx(u16) + total_chunks(u16) + jpeg_chunk` |
| `0x03` | MIC | `seq(u32) + opus_packet` |
| `0x04` | AUDIO_REPORT | speaker playout report, see "Speaker audio" below |
| `0x05` | PLI | `display_id(u8) + generation(u32)` (preferred over control-stream `PLI`) |

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
| 33 | internal_width_mm | u16 | 0 = unknown physical panel size |
| 35 | internal_height_mm | u16 | 0 = unknown physical panel size |
| 37 | audio_bitrate | u32 (bps) | 0 = 128000; otherwise 48000, 64000, 96000, 128000, 192000 or 256000 |

The iPad's "Daemon default" options map to these values; the daemon then uses its CLI-configured
default for that field.

The body is exactly 41 bytes. Width/height/FPS at offsets 0/2/4 describe the internal display.
External FPS is independent, inheriting internal FPS only when omitted. Codec and the base bitrate
target are shared settings; the target applies to **each** enabled video pipeline. Each pipeline
adapts bitrate/FEC independently from that target. Both
resolutions are independently clamped against the same advertised limits. Each width/height
pair must either be specified together or uses daemon defaults. Other body lengths, masks outside
1–3 and zero revisions are rejected. The daemon's maximum FPS applies per output.
Monitor names must be nonempty printable ASCII, without single quotes or backslashes.
Panel dimensions are in mm; both must be in 50–1000 mm or the size is treated as unknown.
Audio bitrate controls speaker Opus encoding only; microphone encoding is unchanged.
Unsupported audio bitrates are rejected.

### Runtime Display Topology

`SETTINGS` may also be sent while streaming. The display manager coalesces pending requests and
ignores revisions older than or equal to the latest request. Settings, presentation ACKs, and
outbound status messages are all tagged with their owning network session internally, so delayed
work from a disconnected client cannot affect its replacement.

Each enabled output has its own capture, encoder, video sender, recovery controls, and stop/join
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
The iPad estimates its panel dimensions from its model's marketing diagonal and native pixel aspect
ratio, independently of stream resolution. The daemon stores that size in EDID, rounded to whole
centimetres, and gives the touchscreen the same size. Desktops can then choose scaling from DPI
and match touch to the display. GNOME displays the vendor and diagonal rather than the model
when a real size is supplied.

UIKit supplies no physical external diagonal, so no size is inferred from external pixels. Unknown
sizes retain the 16×10 cm compatibility marker. Product IDs and serials remain stable and distinct.
The touchscreen is named `Screx Touchscreen SRX 001` to identify the internal monitor to compositors
that match by vendor and serial. The daemon writes no per-user touchscreen settings or udev rules.

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

Camera JPEGs are split into 1000-byte `CAMERA` datagrams (see above). The daemon reassembles each JPEG
and writes it to the virtual webcam.

### Speaker audio

Nothing here depends on the network type: the same mechanisms settle near the minimum delay on a
LAN and grow toward smoothness on a worse remote link, driven only by what the iPad measures.

Payload of a speaker datagram, integers big-endian:

| Field | Size | Description |
|---|---|---|
| `count` | u8 | Number of entries, 1-4 |
| entries | | `count` x (`distance(u8) + len(u16) + opus[len]`) |

The first entry has distance 0 and is the packet named by the header's `frame_id` and
`timestamp_ms`. Every further entry is a copy of packet `frame_id - distance`, whose timestamp is
`timestamp_ms - 10 * distance`. Distances strictly increase and are at most 8. The daemon only
includes copies of packets it actually sent in the current speaker-card session (each one starts a new
generation), and drops copies that would not fit in one datagram. Speaker frame IDs are consecutive within a generation, so a
missing ID is a lost packet. The redundancy level follows the iPad's reports: none on a clean link,
distance 1 for 10 s after any primary packet loss, and distances 1 and 3 for 10 s after a burst of 2
or more consecutive losses. Copies at distances 1 and 3 recover any burst of up to 3 lost packets.

The iPad keeps a jitter buffer of Opus packets ordered by packet ID and decodes each packet when it
is due to play, so late packets never play out of order and delay can't build up:

- a packet that hasn't arrived when it is due is filled with Opus packet-loss concealment and
  dropped if it arrives later. After 30 ms of concealment with nothing buffered, playback goes
  silent and restarts once the buffer refills
- the target depth adapts to measured arrival jitter (the 99th percentile of arrival delay over
  the last 4 s, plus one packet), between 15 and 200 ms. It rises immediately and falls by at most 2 ms per
  second. A new session starts from the initial target, not the last session's, since it may be
  on another network. While video is flowing, the target is also capped so audio plays no more than about
  100 ms behind the matching video, measured through the shared timestamp clock; past that, late
  packets are concealed instead of adding delay
- once the smoothed depth is more than 15 ms (or 20% of the target, if larger) above the target,
  it merges two packets into 10 ms with a crossfade; that far below, it inserts 10 ms of
  concealment. At most one such adjustment happens per 200 ms. This drains the excess left when
  playback starts while packets are still delayed (for example behind the first keyframes)
  within about a second; smaller errors are clock drift, which the daemon's pace follows
- the iPad handles audio datagrams on its receive queue as soon as they are decrypted. Video
  reassembly, FEC recovery and decoding run on a separate queue, so audio never waits behind them

Every 200 ms while speakers play, the iPad sends a 31-byte `AUDIO_REPORT` datagram:

| Field | Size | Description |
|---|---|---|
| `generation` | u32 | Speaker audio generation the report is about |
| `depth_us` | u32 | Smoothed jitter-buffer depth |
| `target_us` | u32 | Current target depth |
| `due` | u16 | Packets that came due for playback since the last report |
| `primary_lost` | u16 | Of those, packets whose own datagram never arrived in time |
| `recovered` | u16 | Of those, packets played from a redundant copy |
| `concealed` | u16 | Of those, packets filled with concealment |
| `late` | u16 | Datagrams that arrived after their packet was due |
| `max_burst` | u8 | Longest run of consecutive `primary_lost` packets |
| `jitter_us` | u32 | Jitter estimate before the render quantum and the video cap are applied |
| `cap_us` | u32 | The video cap on the target; 0 when there is none |

Reports are unreliable; the daemon keeps its current settings while none arrive and ignores
reports for other generations. The daemon uses them to:

- **follow the iPad's clock.** The card has no clock of its own; the daemon's reads set its pace.
  The daemon shortens or lengthens its 10 ms read interval, by at most 500 ppm, to hold the iPad's
  depth at its target, acting on errors of at most 5 ms. The card then runs at the rate the iPad actually plays, and the iPad never
  resamples. Large, fast changes are left to the iPad's merge and insert
- choose the redundancy level (above)
- tell the Opus encoder the measured loss rate, so packets depend less on earlier ones while
  packets are being lost
- log, with `-v`, a summary every 5 s: depth and target, the jitter estimate and video cap,
  clock correction, redundancy, loss counts and the longest loss burst. These are diagnostics for
  any connection, not tuning for one

### Microphone forwarding

Each `MIC` datagram carries one 20 ms Opus packet and a mic-only sequence number. Because the
number counts mic packets only, the daemon can tell when one was lost. The daemon keeps a jitter
buffer of mic packets and writes 10 ms to the microphone card on a steady timer, instead of
whenever a packet arrives, so programs recording from it see a steady device rather than the
network's arrival pattern:

- the target depth adapts to arrival jitter like the iPad's speaker buffer, between 30 and 200 ms,
  and is drained or refilled the same way (band of 20 ms or 20% of the target)
- lost or late packets are filled with Opus packet-loss concealment; late packets are dropped.
  After 60 ms of concealment with nothing buffered, the card gets silence until the buffer refills
- the timer is shortened or lengthened, by at most 500 ppm, to hold the depth at its target, so
  the microphone card runs at the rate the iPad's microphone records
- mic datagrams share the QUIC connection with camera and receiver reports

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

- the daemon creates an ALSA playback card named after the iPad through `/dev/screx_audio`
- audio is encoded as Opus and sent to the client
- the `SPEAKER` control message attaches/detaches the card
- the daemon reads 10 ms of PCM about every 10 ms, advancing the card's playback position; the
  exact interval follows the iPad's playback clock (see "Speaker audio")
- video backpressure keeps part of the QUIC datagram send buffer free for audio; audio and video
  share QUIC packets, so there is no separate IP priority marking for audio

### Microphone

- the client captures microphone audio and encodes it as Opus
- the daemon decodes it and writes PCM to an ALSA capture card named after the iPad
- the `MIC` control message attaches/detaches this microphone card

The `screx_snd` kernel module owns these cards. Closing the daemon's device files removes them,
including on daemon exit or crash. Desktop audio servers discover them as normal sound hardware;
the daemon does not select a user or access a user's audio server.

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
- `screx_snd` ALSA playback card for iPad speakers
- `screx_snd` ALSA capture card for the iPad microphone

## Notes on Compatibility

- The Linux virtual webcam uses `v4l2loopback`; some applications behave differently depending on `exclusive_caps` mode. See the v4l2loopback troubleshooting notes in the ArchWiki for compatibility context: [ArchWiki: v4l2loopback Troubleshooting](https://wiki.archlinux.org/title/V4l2loopback#Troubleshooting).

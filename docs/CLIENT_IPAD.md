# iPad Client

Native iPadOS app that connects to the [Linux daemon](DAEMON_LINUX.md) over the network
(Wi-Fi). Decodes the video/audio stream and forwards touch, keyboard, mouse, controller,
microphone, and camera input back to the host. Peripheral controls are gated by the capabilities
advertised by the daemon. Source lives in `client`.

## Requirements

- A Mac with Xcode 26 or later
- An iPad running iPadOS 16.0 or later
- An Apple Developer account for code signing
- Internet access on first build, so Xcode can resolve the Swift Package Manager dependency below

## Dependencies

Resolved automatically by Xcode via Swift Package Manager — no manual install step:

- [`swift-opus`](https://github.com/alta/swift-opus) — Opus: the wrapper encodes the microphone; speaker
  playback calls the bundled libopus (`Copus`) directly for packet-loss concealment

QUIC uses Apple's Network.framework; its datagram support needs iPadOS 16. CPace pairing also
compiles the Monocypher-based Elligator2 adapter in `shared/` (through
`Screx-Bridging-Header.h`), so check out the **whole repository**, not only `client/`.
Monocypher's license ships as an app resource.

## Build

1. Open `client/Screx.xcodeproj` in Xcode.
2. Select the `Screx` scheme and your iPad as the run destination.
3. Copy `client/Config/Local.xcconfig.example` to `client/Config/Local.xcconfig` and set
   your team ID and bundle ID prefix (the app becomes `<prefix>.screx`). That file is git-ignored,
   so the committed project carries no personal identifiers; without it the bundle ID is
   `com.example.screx` and no team is set. Don't set the team in **Signing & Capabilities**: Xcode
   would write it into `project.pbxproj`.
4. Build and run (`Cmd+R`). On the first run, trust the developer certificate on the iPad
   under Settings → General → VPN & Device Management.

Xcode builds and installs the app directly to the connected device — there's no separate CLI build
step or bundler for this target.

## Tests

The tests are standalone programs compiled with `swiftc` from `client/`:

```sh
swiftc Screx/AudioJitterBuffer.swift Tests/AudioJitterBufferTests.swift -o /tmp/screx-audio-tests && /tmp/screx-audio-tests
swiftc Screx/MediaPacket.swift Tests/MediaPacketTests.swift -o /tmp/screx-media-tests && /tmp/screx-media-tests
clang -c Screx/CPacePrimitives.c -o /tmp/cpace-primitives.o
swiftc -import-objc-header Screx/Screx-Bridging-Header.h \
  Screx/CPace.swift Tests/CPaceTests.swift /tmp/cpace-primitives.o -o /tmp/cpace-tests && /tmp/cpace-tests
```

`Tests/QUICInteropSmoke.swift` pairs with a daemon test server over real QUIC (see
[DAEMON_LINUX.md](DAEMON_LINUX.md#tests) for the Linux side):

```sh
swiftc -import-objc-header Screx/Screx-Bridging-Header.h \
  Screx/CPace.swift Screx/QUICSession.swift Screx/PairingService.swift \
  Tests/QUICInteropSmoke.swift /tmp/cpace-primitives.o -o /tmp/screx-quic-smoke
/tmp/screx-quic-smoke HOST_IP
```

Type the PIN the daemon test prints. It checks the PAKE exchange, the TLS exporter binding, both
streams and a 1022-byte datagram round trip. It keeps pairings in memory because macOS doesn't
allow Keychain writes from an SSH session.

## App Store notes

- Only public Network.framework, Security and CryptoKit APIs are used; TLS comes from
  Network.framework.
- The bundled Monocypher code is used only for pairing; its license must ship with the binary.
- `ITSAppUsesNonExemptEncryption` is not set. Answer the export-compliance questions for all of
  the app's cryptography, including the bundled pairing code, before submitting.
- The security design has not been independently audited.

## Permissions

The app requests these on first use:

- **Local Network** — to discover/connect to the daemon over Wi-Fi
- **Camera** — to forward video as a virtual webcam on the host
- **Microphone** — to forward audio as a virtual microphone on the host

## Use

See [README.md](../README.md#use) for the end-to-end connect flow (PIN pairing, enabling the
virtual display).

The connection screen shows a tile per host. Tap a tile to connect; progress, a Cancel button and
any error appear on the tile itself. Tap **+** to enter an address and choose its video and encoding
settings before pressing **Connect**. It becomes a tile after
its first successful connection, and typing the address of an existing tile connects through that
tile. Each tile's **⋯** menu (or a long press) has **Settings…**, **Pin** and **Remove**; the **⋯**
next to **+** clears unpinned hosts or all hosts.

**Settings are per host:** the address, resolution, framerate, codec, video bitrate and audio bitrate. After a host has
connected once, its settings only offer what it supports. A new host starts at this iPad's
resolution, 60 fps, the host's codec, 8 Mbps video and Medium (128 kbps) audio. Settings are validated against the daemon's
advertised capabilities and sent during connection setup. For the wire protocol, see
[ARCHITECTURE.md](ARCHITECTURE.md).

Video targets are Low (3 Mbps), Medium (8 Mbps), High (15 Mbps) and Very high (20 Mbps),
with Host default and Custom also available. Targets apply per display, do not scale with
resolution or frame rate, and may adapt downward on network loss.

Audio offers Low (96 kbps), Medium (128 kbps), High (192 kbps) and Very high (256 kbps).
This controls Opus speaker audio only, not the microphone. Like video settings, the choice is
saved per host and sent on connection.

**Pairing is per host tile.** Removing a tile forgets its pairing, so adding the host back asks for
the PIN again; editing the address in Settings keeps it. Each tile keeps its own signing key and
the host's pinned certificate in the Keychain. Connecting fails after 8 seconds if the daemon
can't be reached. If another device is streaming from the host, the tile shows "In use by another
device"; reconnecting from the same iPad takes over its own earlier session. The connection is
closed after 15 seconds without traffic from the daemon. Active
touch gestures are clamped to the video boundary so dragging outside the letterboxed image still
produces a matching release at the nearest display edge.

## Pointer model

### External displays

Connect a monitor by cable or AirPlay, then open **Displays** (the two-display icon in the
in-session pill, available even when the pill is collapsed). Connection alone never changes the
output or takes over iPadOS mirroring.

- **Internal only:** stream to the iPad and release Screx's external window. iPadOS may mirror
  the iPad screen, including app controls, for an audience.
- **External only:** stream to the monitor. The iPad keeps its normal interface (pill, floating
  trackpad, iOS keyboard); the video area turns black with "You are running only in the external
  display" and a **Switch to laptop mode** button. The floating trackpad is enabled automatically
  and no touchscreen coordinates are sent. A physical mouse/trackpad keeps driving the desktop
  through relative events: the iPad cursor stays hidden and pointer-locked, and its clicks never act
  as finger gestures on the on-screen trackpad.

  **Laptop mode** replaces that screen, the pill and the iOS keyboard with: a top bar (Switch to
  normal mode, Displays, speaker, microphone, camera, controller, then connection info and a red ✕
  disconnect button at the far right), an edge-to-edge drawn keyboard (esc, F1–F12, home, end, ins, del; number and letter rows;
  ctrl/🌐/super/alt/space/alt gr/arrows), and the trackpad at the bottom. It always starts off when
  entering External only. The layout (US, Spanish, French AZERTY, German QWERTZ) is detected from the
  iPad's enabled keyboards, in Settings order; the 🌐 key cycles layouts and the choice is
  remembered. Shift/AltGr/Ctrl/Alt/Super work like physical keys: active while held (hold Shift with one
  finger and type with another; the legends switch while held), and a modifier tapped on its own
  is sent alone (e.g. Super opens the overview). Caps lock toggles. Held Shift also applies to
  arrows, Tab and other editing keys, so Shift+arrows selects text. Accent dead keys compose (´ + e → é). Backspace,
  delete and arrows repeat when held. Dictation, autocorrect and emoji need the normal mode.
- **Both:** two independent Linux virtual monitors, with a decoder for each desktop.

The host's settings configure the iPad's resolution and FPS. The Displays menu configures only the
external output, with its own resolution and frame rate. The preferred resolution reported by
iPadOS is marked Recommended. Only resolutions exposed by iPadOS appear; no extra scaled sizes
are added. Frame rate choices
respect the display and daemon limits. iPadOS does not expose the full list of EDID refresh timings.
The previous
output stays alive until the requested surfaces are attached and their video is ready. Unplugging
the external monitor restores internal video without reconnecting; failures report an error and
retain the previous working topology when possible.

Display choices are remembered **per daemon hostname**. Reconnecting restores the last mode
(Internal only, External only, or Both) and the external resolution and frame rate, as long as the
display still reports that resolution; otherwise it falls back to the display's **Recommended**
mode. With nothing remembered, Recommended is used. A remembered External only/Both choice waits
until a monitor is attached, then applies automatically; choosing Internal only is remembered too.
The host's settings own the iPad's own resolution and FPS.

Audio follows the iPadOS output route, including HDMI when offered by the monitor/adapter. Test
monitor sound with Screx's microphone off first: recording can restrict the routes iPadOS permits.
If HDMI remains silent, compare with another iPad audio app and check the monitor's volume/input.

Linux monitor names identify the iPad model (abbreviated to fit EDID's 13-character ASCII limit)
and **iPad External**. Unknown devices use **Screx**. The full mapping of released iPad models is in
`client/Screx/DeviceDisplayIdentity.swift`. External physical size in inches is not exposed by
iPadOS and is not guessed from pixel dimensions.

The info overlay is a short card: hostname, ↓/↑ transfer rates, RTT and jitter, then Disconnect.
It shows no codec or display-mode row; the mode lives only in Displays.

### On-screen trackpad

Expand the in-session pill and tap the trackpad icon to show a semi-transparent trackpad.
It is off by default and resets to off when the session ends.

- Slide one finger to move the pointer; tap to left-click.
- Tap with two fingers to right-click; slide two fingers to scroll.
- Tap, then touch again and drag to hold the left button; lift to release.
- Drag the top grip to move the trackpad, or the bottom-right handle to resize it.

Size and position are remembered. Touches starting inside the trackpad are consumed locally,
not forwarded as touchscreen input to the display underneath. The trackpad reuses the existing
relative mouse messages and works alongside a physical mouse; no daemon update is needed.

If the on-screen keyboard covers the trackpad, it moves above the keyboard without changing
size. If it is too tall for the remaining space, its height temporarily shrinks to fit.
The saved size and position return when the keyboard closes.

### Direct touch and external pointers

Touch input is **absolute** — a tap maps directly to that point on the remote display (translated
to pointer + scroll gestures).

When an external mouse or trackpad is connected, the iPad engages **pointer lock**
(`prefersPointerLocked`) and forwards **relative** motion deltas. This gives a true extended-display
feel: the remote cursor moves by relative motion and can roam the remote desktop freely, rather than
being pinned to the viewport.

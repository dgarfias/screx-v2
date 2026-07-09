# iPad Client

Native iPadOS app that connects to a Screx daemon — [Linux](DAEMON_LINUX.md) or
[Windows](DAEMON_WINDOWS.md); the wire protocol is identical on both. Decodes the video/audio
stream and forwards touch, keyboard, mouse, controller, microphone, and camera input back to the
host. Source lives in `client/ipad`.

## Requirements

- A Mac with Xcode 26 or later
- An iPad running iPadOS 16.0 or later
- An Apple Developer account for code signing
- Internet access on first build, so Xcode can resolve the Swift Package Manager dependency below

## Dependencies

Resolved automatically by Xcode via Swift Package Manager — no manual install step:

- [`swift-opus`](https://github.com/alta/swift-opus) — Opus encode/decode for microphone forwarding

Everything else is a first-party Apple framework (AVFoundation, VideoToolbox, CryptoKit, Network,
CoreImage, GameController) with no additional package manager setup required.

## Build

1. Open `client/ipad/Screx.xcodeproj` in Xcode.
2. Select the `Screx` scheme and your iPad as the run destination.
3. In **Signing & Capabilities**, set your own development team, and change the bundle identifier
   if it collides with an existing provisioning profile.
4. Build and run (`Cmd+R`). On the first USB run, trust the developer certificate on the iPad
   under Settings → General → VPN & Device Management.

Xcode builds and installs the app directly to the connected device — there's no separate CLI build
step or bundler for this target.

## Permissions

The app requests these on first use:

- **Local Network** — to discover/connect to the daemon over Wi-Fi
- **Camera** — to forward video as a virtual webcam on the host
- **Microphone** — to forward audio as a virtual microphone on the host

## Use

See [README.md](../README.md#use) for the end-to-end connect flow (network vs. USB, PIN pairing,
enabling the virtual display).

Before connecting, tap **Stream Settings** to choose the resolution, framerate, codec, and bitrate
for the next session. These settings are validated against the daemon's advertised capabilities and
sent during connection setup. For the wire protocol, see [ARCHITECTURE.md](ARCHITECTURE.md).

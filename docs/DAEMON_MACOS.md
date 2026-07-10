# macOS Daemon

The macOS daemon is the same `daemon/` crate as the [Linux](DAEMON_LINUX.md) and
[Windows](DAEMON_WINDOWS.md) daemons — same `screx` binary, same CLI, same wire protocol — with a
`#[cfg(target_os = "macos")]` backend (`daemon/src/platform/macos/`) that swaps
EVDI/uinput/v4l2loopback/PipeWire (or DXGI/SendInput/DirectShow/WASAPI/ViGEmBus) for a private
`CGVirtualDisplay` virtual monitor, public `CGDisplayStream` capture, VideoToolbox hardware
encode, `CGEventPost` input injection, and a ScreenCaptureKit audio-only stream for speaker
capture. See [ARCHITECTURE.md](ARCHITECTURE.md) for the protocol.

## Requirements

| Requirement | Provides | Source |
|---|---|---|
| macOS 13 (Ventura) or later | Required OS floor — the ScreenCaptureKit audio-only capture path (`capturesAudio`) the daemon uses for speaker forwarding is a macOS 13+ API | Preinstalled/OS update |
| Xcode Command Line Tools | System headers, `clang`, linker | `xcode-select --install` |
| Rust toolchain | Builds the `screx` binary | [rustup.rs](https://rustup.rs) |
| Homebrew `ffmpeg` | VideoToolbox encoders (`h264_videotoolbox`, `hevc_videotoolbox`) — included by default in Homebrew's build, no special flags needed | `brew install ffmpeg` |
| Homebrew `libimobiledevice` (optional) | `idevice_id` / `iproxy` CLI tools for USB transport (iPad only) — macOS ships `usbmuxd` itself natively, so this is the only extra piece needed | `brew install libimobiledevice` |

## Build

```bash
cd daemon
cargo build --release
```

Output: `daemon/target/release/screx`.

If `cargo build` can't find FFmpeg, confirm Homebrew's `ffmpeg` is installed and that
`pkg-config` can see it (`pkg-config --modversion libavcodec`); on Apple Silicon Homebrew installs
under `/opt/homebrew`, which needs to be on `PKG_CONFIG_PATH`/`PATH` — this is normally handled by
`brew shellenv` in your shell profile already.

## Run

```bash
cd daemon
./target/release/screx
./target/release/screx -w 1920 -H 1080 -f 60 -c h264
./target/release/screx -e videotoolbox -c h265
./target/release/screx --network-only
./target/release/screx --usb-only
./target/release/screx unpair --all
```

Run this **as your normal logged-in user — never with `sudo` or as root.** `CGVirtualDisplay`,
`CGDisplayStream`, `CGEventPost`, and ScreenCaptureKit all operate against the logged-in user's
WindowServer session, and the TCC permission grants below are per-user. Running as root has no
session to attach to and no TCC grants of its own, so the daemon checks for this at startup
(`geteuid() == 0`) and refuses to run with an explanatory message rather than failing deep inside
display/capture/input setup.

Same CLI flags as the [Linux daemon](DAEMON_LINUX.md#run) — see that page's flag table (`-w,
--max-width`, `-H, --max-height`, `-f, --max-framerate`, `-k, --keyframe`, `-b, --max-bitrate`,
`--max-bitrate-usb` (default `100M`), `-p, --port`, `-c, --codec`, `-v, --verbose`,
`--network-only`, `--usb-only`); `-w`/`-H`/`-f`/`-b`/`--max-bitrate-usb` are per-daemon ceilings
and defaults, not fixed values — connecting clients may request lower values during connection
(see [ARCHITECTURE.md](ARCHITECTURE.md#capability-negotiation)). `--no-camera-exclusive-caps` is
Linux-only and has no effect on macOS.

`-e/--backend` accepts `auto` (default), `videotoolbox` (or its alias `vt`), or `software`;
`vaapi`/`nvenc`/`amf`/`qsv`/`mf` are other platforms' backends and aren't recognized here (an
unrecognized value falls back to `auto`). `auto` prefers VideoToolbox and falls back to the
software x264/x265 encoder if VideoToolbox is unavailable for the requested codec.

## Required permissions (TCC)

The daemon needs two macOS privacy permissions granted to the built `screx` binary. Both are
preflighted at startup with an actionable error/warning if missing, rather than failing silently
or partway through a session:

1. **Screen Recording** — required for display capture (`CGDisplayStream`) and for the
   ScreenCaptureKit speaker-audio stream. Grant at
   **System Settings → Privacy & Security → Screen Recording**, enable `screx`, then restart the
   daemon (macOS does not apply a freshly granted Screen Recording permission to an already-running
   process).
2. **Accessibility** — required for `CGEventPost` input injection (mouse, touch-translated
   pointer, keyboard). Grant at
   **System Settings → Privacy & Security → Accessibility**, enable `screx`, then restart the
   daemon.

The first run will prompt you to add `screx` to these lists (or print an actionable error naming
the exact setting to open, if the OS didn't prompt automatically) — grant both, then re-run.

### Gotcha: re-prompts after every rebuild

TCC identifies the granted binary by its code signature. A binary built by `cargo build` gets an
ad-hoc signature that changes on every rebuild, so macOS treats each new build as a "new" binary
and re-prompts for both permissions — tedious during development. Fix it by signing `screx` with a
stable, self-signed certificate instead of the ad-hoc one:

1. Open **Keychain Access** → menu **Keychain Access → Certificate Assistant → Create a
   Certificate…**
2. Name it `screx-dev`, set **Identity Type** to `Self Signed Root`, **Certificate Type** to
   `Code Signing`, and create it.
3. Sign the binary after every build:

   ```bash
   codesign -s screx-dev target/release/screx
   ```

With a stable certificate, TCC recognizes the binary across rebuilds and the grants persist —
sign once per build, no need to re-grant permissions each time.

## External display behavior

The daemon creates the virtual display as an **extended** desktop — exactly like plugging in a
real external monitor — deterministically placed at the right edge of your existing display via a
session-scoped `CGConfigureDisplay` transaction. It does not come up mirrored, and the daemon
doesn't fight you if you later switch it to mirrored: enable mirroring anytime in
**System Settings → Displays** and the daemon leaves your choice alone for the rest of that
session.

## Private API caveat

`CGVirtualDisplay` is a private, undocumented CoreGraphics API (the same one apps like DeskPad use)
— there is no public, Apple-supported API for creating a virtual display on macOS as of this
writing. It is not gated by an Apple Developer account or entitlement, but it is not guaranteed to
keep working across macOS versions. If a future macOS major release removes or changes this API,
the daemon is designed to fail fast with a clear, actionable error (e.g. "class ... not found
(unsupported macOS version?)") rather than corrupting display state or crashing silently.

## Not supported in v1

- **Camera** (virtual webcam forwarding) — deferred, no design work has landed yet.
- **Microphone forwarding** — deferred; macOS has no built-in equivalent to a PulseAudio null-sink,
  so this needs a third-party virtual audio driver design that hasn't been done.
- **Gamepad passthrough** — there is no public macOS API for a virtual gamepad/HID controller
  device (unlike ViGEmBus on Windows or `uinput` on Linux).

These are honestly reported as unavailable in the daemon's `CAPS` capability message
(camera/mic `available = 0`; gamepad `available = 0`), so connecting clients automatically hide
those toggles instead of offering a control that would fail.

Speaker audio forwarding **is** supported (ScreenCaptureKit audio-only capture, 48 kHz stereo).
Host audio keeps playing locally through your Mac's normal output while it's also being sent to
the client — ScreenCaptureKit taps the audio stream rather than rerouting it, so nothing goes
silent on the host side.

## USB transport

USB transport (iPad only) uses `idevice_id`/`iproxy` from Homebrew's `libimobiledevice`, the same
tools the Linux daemon uses — macOS already ships Apple's own `usbmuxd`, so no extra mux daemon is
needed, just those two CLI tools. If `libimobiledevice` isn't installed, USB transport won't be
available; network transport is unaffected. Use `--network-only` to disable USB outright, or
`--usb-only` to disable network pairing/streaming.

## Use

1. Start the daemon.
2. Open Screx on the iPad or launch the desktop client.
3. Choose one transport:
    - **Network**: enter the host/IP and tap `Connect`
    - **USB**: tap `Connect via USB`
4. If it is the first network connection, enter the PIN shown by the daemon.
5. The virtual display comes up automatically — no manual step is needed to enable it (unlike
   GNOME Settings on Linux).

## Running headless via LaunchAgent

To have the daemon start automatically at login without a Terminal window, install it as a
per-user LaunchAgent. **Grant both TCC permissions interactively first** (see above) — a
LaunchAgent-launched process can't respond to a permission prompt, so if the grants aren't already
in place, the daemon will just fail its preflight checks silently in the background every time it's
launched.

Create `~/Library/LaunchAgents/com.screx.daemon.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.screx.daemon</string>

    <key>ProgramArguments</key>
    <array>
        <string>/absolute/path/to/screx/daemon/target/release/screx</string>
        <string>--network-only</string>
    </array>

    <key>RunAtLoad</key>
    <true/>

    <key>KeepAlive</key>
    <true/>

    <key>StandardOutPath</key>
    <string>/tmp/screx-daemon.log</string>

    <key>StandardErrorPath</key>
    <string>/tmp/screx-daemon.log</string>
</dict>
</plist>
```

Replace `/absolute/path/to/screx/daemon/target/release/screx` with the real path (LaunchAgents
don't expand `~` or resolve `PATH`), and adjust `ProgramArguments` for whichever flags you want
(the array form above, minus the flags, is equivalent to running `screx --network-only`).

Load and unload it with `launchctl`:

```bash
launchctl load ~/Library/LaunchAgents/com.screx.daemon.plist
launchctl unload ~/Library/LaunchAgents/com.screx.daemon.plist
```

Since the plist has no `UserName`/`GroupName` keys, `launchd` runs it as the same user who loaded
it via `launchctl load` in their own login session — never as root — which is exactly the session
the daemon needs for `CGVirtualDisplay`/`CGDisplayStream`/`CGEventPost`/TCC to work. `KeepAlive`
restarts the daemon if it crashes or exits; remove that key (or set it `false`) if you'd rather it
stay stopped after a failure while you investigate `/tmp/screx-daemon.log`.

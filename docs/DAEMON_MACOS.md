# macOS Daemon

The macOS daemon is the same `daemon/` crate as the [Linux](DAEMON_LINUX.md) and
[Windows](DAEMON_WINDOWS.md) daemons — same `screx` binary, same CLI, same wire protocol — with a
`#[cfg(target_os = "macos")]` backend (`daemon/src/platform/macos/`) that swaps
EVDI/uinput/v4l2loopback/PipeWire (or DXGI/SendInput/DirectShow/WASAPI/ViGEmBus) for a private
`CGVirtualDisplay` virtual monitor, ScreenCaptureKit video capture, zero-copy CVPixelBuffer-to-
VideoToolbox hardware encode, `CGEventPost` input injection, and a separate ScreenCaptureKit
audio-only stream for speaker capture. See [ARCHITECTURE.md](ARCHITECTURE.md) for the protocol.

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
`CGEventPost`, and ScreenCaptureKit all operate against the logged-in user's
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

The practical macOS choices for `-e/--backend` are `auto` (default), `videotoolbox` (or `vt`), and
`software`. `nvenc` is also recognized and attempted when explicitly selected, although it is not a
normal macOS configuration. Linux- and Windows-only names such as `vaapi`, `amf`, `qsv`, and `mf`
are treated as `auto` on a macOS build. `auto` prefers VideoToolbox and falls back to software
x264/x265 if VideoToolbox is unavailable for the requested codec.

The normal VideoToolbox path retains ScreenCaptureKit's original IOSurface-backed `420v`
`CVPixelBuffer` and submits it directly to FFmpeg as `AV_PIX_FMT_VIDEOTOOLBOX`, without a CPU pixel
copy. The CoreVideo reference remains alive until VideoToolbox's asynchronous encode releases the
FFmpeg frame. Software encoding reads the native NV12 IOSurface into YUV420P; byte-backed NV12 and
BGRA paths remain for bootstrap, synthetic, and other fallback inputs.

VideoToolbox is configured with best-effort real-time, speed-priority, and constant-bitrate hints.
FFmpeg or OS versions that do not expose a hint continue without it; if opening with the optional
hints fails, the daemon retries with VideoToolbox defaults. Because VideoToolbox reads bitrate
settings when it creates the compression session, a runtime bitrate change constructs a replacement
encoder and begins the new coding sequence with an IDR frame instead of attempting an in-place
retune.

## Required permissions (TCC)

The daemon needs two macOS privacy permissions granted to the built `screx` binary. Both are
preflighted at startup with an actionable error/warning if missing, rather than failing silently
or partway through a session:

1. **Screen Recording** — required for the ScreenCaptureKit display and speaker-audio streams.
   Grant at
   **System Settings → Privacy & Security → Screen Recording**, enable `screx`, then restart the
   daemon (macOS does not apply a freshly granted Screen Recording permission to an already-running
   process).
2. **Accessibility** — required for `CGEventPost` input injection (mouse, touch-translated
   pointer, keyboard). Grant at
   **System Settings → Privacy & Security → Accessibility**, enable `screx`, then restart the
   daemon.

Screen Recording may prompt on first use. Accessibility is preflighted but not requested through a
system prompt, so you may need to add or enable `screx` manually in the Accessibility list. Grant
both permissions, then restart the daemon.

### Gotcha: permissions after rebuilds

TCC identifies the granted binary by its code signature. A binary built by `cargo build` gets an
ad-hoc signature that can change on rebuild, so macOS may treat the result as a new binary and stop
recognizing the existing grants. Screen Recording may prompt again; Accessibility may instead need
to be removed and added or enabled manually. A stable, self-signed certificate avoids this churn:

1. Open **Keychain Access** → menu **Keychain Access → Certificate Assistant → Create a
   Certificate…**
2. Name it `screx-dev`, set **Identity Type** to `Self Signed Root`, **Certificate Type** to
   `Code Signing`, and create it.
3. Sign the binary after every build:

   ```bash
   codesign -s screx-dev target/release/screx
   ```

With a stable certificate, TCC can recognize the binary across rebuilds. Sign once per build so the
existing grants can persist.

## External display behavior

The daemon ensures that the virtual display is extended and does not overlap another display. If
WindowServer already provides a valid non-overlapping arrangement, that placement is left
unchanged. If the display is mirrored or overlapping, the daemon attempts to unmirror it and place
it immediately to the right of another online display. It retries a failed configuration once,
then continues with the live arrangement instead of aborting the stream. It does not keep
reapplying placement, so you can rearrange displays or enable mirroring later in **System Settings
→ Displays**.

The ScreenCaptureKit video stream includes the macOS host cursor. When an external pointer is
active, the iPad client hides its local pointer over the video surface so the streamed host cursor
is the visible cursor.

## Input behavior

macOS has no native remote-touch injection API, so direct touch is translated into pointer
gestures. A tap produces a left click, movement beyond a small threshold becomes a left drag, a
stationary 500 ms long press produces a right click, and two-finger movement produces pixel-based
scrolling with begin/change/end phases. Two-finger touch scrolling follows the host Mac's Natural
scrolling preference. External mouse-wheel messages remain discrete line-scroll events.

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

Speaker audio forwarding uses a dedicated ScreenCaptureKit audio-only stream at 48 kHz stereo. To
provide client-only playback, activation requires the current default output device to expose a
settable mute control. The daemon temporarily mutes that device, checks its mute state periodically,
follows changes to the default output device, and restores each device's prior mute state when
forwarding stops. If no usable mute control is available, speaker activation fails even though the
current macOS capability probe advertises the capture backend as available.

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
   - **USB** (iPad only): tap `Connect via USB`
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
the daemon needs for `CGVirtualDisplay`/ScreenCaptureKit/`CGEventPost`/TCC to work. `KeepAlive`
restarts the daemon if it crashes or exits; remove that key (or set it `false`) if you'd rather it
stay stopped after a failure while you investigate `/tmp/screx-daemon.log`.

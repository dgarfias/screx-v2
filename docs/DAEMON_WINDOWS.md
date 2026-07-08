# Windows Daemon

The Windows daemon is the same `daemon/` crate as the [Linux daemon](DAEMON_LINUX.md) — same
`screx` binary, same CLI, same wire protocol — with a `#[cfg(target_os = "windows")]` backend
(`daemon/src/platform/windows/`) that swaps EVDI/uinput/v4l2loopback/PipeWire for DXGI desktop
duplication, `SendInput`, a DirectShow virtual camera filter, WASAPI, and ViGEmBus. See
[ARCHITECTURE.md](ARCHITECTURE.md) for the protocol.

## Build

### Prerequisites

- Rust with the MSVC toolchain: `rustup default stable-x86_64-pc-windows-msvc`
- Visual Studio Build Tools (C++ workload) or full Visual Studio — provides the MSVC linker and
  Windows SDK headers/libs
- LLVM, for `libclang.dll` (`ffmpeg-sys-next` uses `bindgen` to generate its FFmpeg bindings) — an
  installer from [releases.llvm.org](https://releases.llvm.org/) or a portable `clang+llvm-*`
  zip extraction both work; you just need `LIBCLANG_PATH` pointing at the directory containing
  `libclang.dll`
- A prebuilt FFmpeg **shared** archive (`full_build-shared`, not `full_build`) — e.g. from
  [BtbN's ffmpeg-builds](https://github.com/BtbN/FFmpeg-Builds/releases) or
  [gyan.dev](https://www.gyan.dev/ffmpeg/builds/)

### Environment variables

```bat
set FFMPEG_DIR=C:\ffmpeg\ffmpeg-8.1.2-full_build-shared
set LIBCLANG_PATH=C:\llvm\clang+llvm-22.1.8-x86_64-pc-windows-msvc\bin
```

- `FFMPEG_DIR` is read by `ffmpeg-sys-next`'s build script (expects `include/` and `lib/` under it).
- `LIBCLANG_PATH` points `bindgen` at `libclang.dll`.

If you set these with `$env:FFMPEG_DIR = "..."` in PowerShell (or `set` in `cmd.exe`), they only
last for that shell session — a new terminal window won't have them, and `cargo build` will fail
looking for FFmpeg/libclang again. Persist them with `setx FFMPEG_DIR "..."` (takes effect in new
shells only) or just re-export them at the start of every session before building.

### Compiling

From an x64 Visual Studio developer prompt:

```bat
cd daemon
cargo build --release
```

This produces two files in `daemon\target\release\`:

- `screx.exe` — the daemon binary
- `screx_vcam.dll` — a `cdylib` built from the same crate
  (`daemon/src/platform/windows/vcam_filter/lib.rs`), a DirectShow capture filter that the daemon
  registers with COM at runtime the first time the virtual webcam is used. No `regsvr32` step is
  needed — keep the two files together. If you move the DLL elsewhere, set `SCREX_VCAM_DLL_PATH`
  to its full path.

Keep the FFmpeg DLLs from `%FFMPEG_DIR%\bin` on `PATH` (or copy them alongside `screx.exe`) —
there's no bundler script for the daemon like the desktop client's `bundle-windows.bat`.

### Rebuilding after the virtual camera has run

Once `screx_vcam.dll` has been registered and used as a camera, Windows' Frame Server service
holds it open. A subsequent `cargo build --release` can then fail relinking the DLL with
`Access is denied`. If you've only changed daemon code (not the vcam filter itself), skip
rebuilding the DLL:

```bat
cargo build --release --bin screx
```

Otherwise, close any app that had the camera open (or restart the Frame Server service) before
rebuilding `screx_vcam.dll`.

## Runtime drivers

Install these once on the Windows machine that will run the daemon, before first use:

| Requirement | Provides | Source |
|---|---|---|
| **Virtual Display Driver** (VDD) | The virtual monitor (`MttVDD`) | [VirtualDrivers/Virtual-Display-Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver) releases, the "Driver Only" asset — install to the default `C:\VirtualDisplayDriver`, or set `SCREX_VDD_INF_PATH` to your `MttVDD.inf` |
| **Steam**, with Remote Play enabled once | "Steam Streaming Speakers" virtual audio output, for client speaker playback | Install Steam from [steampowered.com](https://store.steampowered.com/) (Valve's official installer), then open Steam → Settings → Remote Play and enable it once. Installing Steam alone does not install the audio driver — it's installed the first time Remote Play is turned on. Override the INF search path with `SCREX_STEAM_SPK_INF_PATH` if needed |
| **ViGEmBus** | Virtual Xbox 360 gamepads, for controller passthrough | [ViGEm/ViGEmBus releases](https://github.com/ViGEm/ViGEmBus/releases) (tested with v1.22.0) |
| **VB-Audio VB-CABLE** | Virtual microphone input | [vb-audio.com](https://vb-audio.com/Cable/) — Screx sends client mic audio into `CABLE Input`, apps record it from `CABLE Output`. VB-CABLE is a global system device: if you already use it for other audio routing, avoid enabling Screx mic forwarding or install a separate VB-Audio cable instance reserved for Screx |
| **Apple Mobile Device Service** (AMDS) | USB transport for the iPad app — speaks the usbmuxd wire protocol over `127.0.0.1:27015` | Installed by iTunes or the "Apple Devices" app (Microsoft Store); not needed for network-only use, and not applicable to the desktop client (network transport only) |

Avoid third-party mirrors or standalone repackagings of the Steam Streaming Speakers driver — an
unsigned or improperly-signed build will fail to load without enabling Windows Test Mode. The
official Steam route above installs a Valve-signed driver that loads normally.

The daemon detects and (for VDD and Steam Streaming Speakers) auto-installs/enables these devnodes
on startup where possible — but the driver package/INF has to already be present on the system via
the installers above.

## Run

Must be run as **Administrator** — like `sudo` on Linux, this is required to create display/audio
devnodes, write registry entries, and inject input.

```bat
cd daemon
.\target\release\screx.exe
.\target\release\screx.exe -w 1920 -H 1080 -f 60 -c h264
.\target\release\screx.exe --network-only
.\target\release\screx.exe unpair --all
```

Same CLI flags as the [Linux daemon](DAEMON_LINUX.md#run) — see that page's flag table (`-w,
--max-width`, `-H, --max-height`, `-f, --max-framerate`, `-k, --keyframe`, `-b, --max-bitrate`,
`-p, --port`, `-e, --backend`, `-c, --codec`, `-v, --verbose`, `--network-only`, `--usb-only`);
`-w`/`-H`/`-f`/`-b` are per-daemon ceilings and defaults, not fixed values — connecting clients may
request lower values during connection (see
[ARCHITECTURE.md](ARCHITECTURE.md#capability-negotiation)). `-e/--backend vaapi` and
`--no-camera-exclusive-caps` are Linux-only and have no effect on Windows.

## Use

Same as the [Linux daemon's "Use" section](DAEMON_LINUX.md#use) — connect from the iPad app or
desktop client over network or USB, enter the PIN on first connection. There's no GNOME-Settings
equivalent step: the virtual display activates automatically once the VDD devnode is enabled.

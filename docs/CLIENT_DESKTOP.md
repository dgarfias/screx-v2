# Desktop Client

Cross-platform desktop client (macOS, Windows, Linux) that connects to a Screx daemon -
[Linux](DAEMON_LINUX.md), [Windows](DAEMON_WINDOWS.md), or [macOS](DAEMON_MACOS.md).
Decodes the stream and forwards input, audio, and camera back to the host. Source lives in
`client/desktop`; the UI is Qt Quick/QML (`qml/Main.qml`) driven by a Rust backend
(`src/backend.rs`). Network transport only — no USB support. It negotiates capabilities with the
daemon and exposes a Stream Settings UI for choosing resolution, framerate, codec, and bitrate
before connecting.

## Requirements

- Rust (stable toolchain)
- Qt 6 with Qt Quick/QML, and `qmake` (or `qmake6`) resolvable on `PATH`
- FFmpeg development libraries
- A C/C++ toolchain, plus `clang`/libclang for `bindgen`
- CMake (used to build the bundled Opus encoder/decoder) — a compatibility shim in
  `client/desktop/.cargo/config.toml` handles newer CMake versions automatically

## macOS

```bash
brew install qt ffmpeg
export PATH="$(brew --prefix qt)/bin:$PATH"
cd client/desktop
make build
```

`make build` runs `cargo build --release`, then `bundle-macos.sh`, which assembles a self-contained
`.app` via `macdeployqt` and ad-hoc code-signs it.

- Output: `client/desktop/target/release/Screx.app`
- Run: `open client/desktop/target/release/Screx.app`
- Minimum OS: macOS 12
- Video display uses VideoToolbox decoding and Metal rendering where available

## Linux

### Arch Linux

```bash
sudo pacman -S --needed \
  rust pkgconf clang \
  ffmpeg libva mesa libdrm \
  qt6-base qt6-declarative \
  alsa-lib libpulse \
  v4l-utils
```

### Ubuntu / Debian

```bash
sudo apt-get install -y \
  cargo pkg-config clang \
  libavcodec-dev libavformat-dev libavfilter-dev libavutil-dev libswscale-dev libswresample-dev \
  libva-dev mesa-va-drivers va-driver-all libdrm-dev libegl1-mesa-dev libgles2-mesa-dev \
  qt6-base-dev qt6-declarative-dev qml6-module-qtquick qt6-base-dev-tools \
  libasound2-dev libpulse-dev \
  libv4l-dev
```

### Build

```bash
cd client/desktop
make build
```

- Output: `client/desktop/target/release/screx-desktop`
- Run: `./client/desktop/target/release/screx-desktop`

Video display uses VA-API for zero-copy hardware decode; audio uses PulseAudio (works with
PipeWire's compat layer); webcam capture goes through `nokhwa`'s V4L2 backend.

## Windows

### Prerequisites

- Rust with the MSVC toolchain: `rustup default stable-x86_64-pc-windows-msvc`
- Visual Studio Build Tools (C++ workload) or full Visual Studio
- LLVM, for `libclang.dll` — [releases.llvm.org](https://releases.llvm.org/) or
  `winget install LLVM.LLVM`
- Qt 6 for MSVC (e.g. `msvc2019_64` kit) with Qt Quick/QML
- A prebuilt FFmpeg `full_build-shared` archive, e.g. from
  [BtbN's ffmpeg-builds](https://github.com/BtbN/FFmpeg-Builds/releases) or
  [gyan.dev](https://www.gyan.dev/ffmpeg/builds/)

### Environment variables

```bat
set FFMPEG_DIR=C:\ffmpeg\ffmpeg-7.1.1-full_build-shared
set LIBCLANG_PATH=C:\Program Files\LLVM\bin
set PATH=C:\Qt\6.7.3\msvc2019_64\bin;%FFMPEG_DIR%\bin;%PATH%
```

Qt's `bin` directory must be on `PATH` so `qmake` and later `windeployqt` are found.

### Build

From an x64 Visual Studio developer prompt:

```bat
cd client\desktop
cargo build --release
bundle-windows.bat release
```

`bundle-windows.bat` copies the exe, runs `windeployqt` for Qt's runtime DLLs/QML modules, and
copies every DLL from `%FFMPEG_DIR%\bin` alongside the executable.

- Output: `client\desktop\target\release\dist\screx-desktop.exe` (zip the `dist` folder to
  distribute — it's self-contained)
- Video display uses D3D11VA for zero-copy hardware decode

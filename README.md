# screx-v2

Screx V2 is a low-latency Linux-to-iPad screen streaming MVP with a Rust daemon sender and a Swift receiver.

## Repository Layout

```text
screx-v2/
├── daemon/                    # Rust Linux daemon
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs            # CLI entry and orchestration
│       ├── capture.rs         # Portal flow scaffold + frame producer
│       ├── encode.rs          # HEVC access-unit generator + control hooks
│       ├── transport.rs       # RTP packetization + UDP transport + control socket
│       └── discovery.rs       # Avahi advertisement wrapper
└── app/                       # Swift iPad companion source files
    ├── ScrexApp.swift
    ├── Discovery.swift
    ├── Transport.swift
    ├── Decoder.swift
    └── DisplayView.swift
```

## MVP Technical Targets

- Glass-to-glass latency: `< 35ms` median
- Frame rate: `60fps` sustained
- Linux daemon CPU: `< 5%` target on representative hardware
- Resolution: `1080p` (MVP), `1440p` (stretch)
- Bitrate: `8-15 Mbps` for 1080p

## Linux Dependencies

Install runtime/build prerequisites:

```bash
sudo pacman -S --needed \
  pipewire libpipewire libspa libclang \
  ffmpeg libva mesa \
  avahi
```

For Debian/Ubuntu-style systems:

```bash
sudo apt-get install -y \
  libpipewire-0.3-dev libspa-0.2-dev libclang-dev \
  libavcodec-dev libavformat-dev libavfilter-dev libavutil-dev \
  libva-dev mesa-va-drivers avahi-daemon
```

## Build and Run (Daemon)

From repo root:

```bash
cd daemon
cargo build
```

Build with real capture/encode feature toggles:

```bash
cd daemon
cargo build --features real-capture,real-encode
```

Run host readiness checks:

```bash
cd daemon
cargo run -- doctor
```

Run with defaults:

```bash
SCREX_TARGET_IP=192.168.1.20 \
SCREX_TARGET_PORT=5004 \
SCREX_WIDTH=1920 \
SCREX_HEIGHT=1080 \
SCREX_FPS=60 \
SCREX_BITRATE_BPS=10000000 \
SCREX_CAPTURE_BACKEND=auto \
SCREX_CAPTURE_SOURCE=virtual \
SCREX_ENCODER_BACKEND=auto \
cargo run
```

Environment variables:

- `SCREX_TARGET_IP` / `SCREX_TARGET_PORT`: iPad receiver destination
- `SCREX_CONTROL_PORT`: daemon UDP control channel bind (defaults to `SCREX_TARGET_PORT`)
- `SCREX_COMMAND`: set to `doctor` to run readiness checks without streaming
- `SCREX_DISCOVERY_NAME`: mDNS service instance name (default `screx-daemon`)
- `SCREX_DISCOVERY_SERVICE`: mDNS service type (default `_screenstream._udp`)
- `SCREX_CAPTURE_BACKEND`: `auto`, `portal-pipewire`, or `synthetic`
- `SCREX_CAPTURE_SOURCE`: `virtual` (default) or `monitor`
- `SCREX_ENCODER_BACKEND`: `auto`, `vaapi`, or `bootstrap`
- `SCREX_WIDTH`, `SCREX_HEIGHT`, `SCREX_FPS`, `SCREX_BITRATE_BPS`, `SCREX_MTU`

Notes:

- `SCREX_CAPTURE_SOURCE=virtual` requests a second virtual display source from portal.
- In virtual mode, capture resolution is locked to `1920x1080` for MVP consistency.

### Control Channel Commands

Send commands to `SCREX_CONTROL_PORT`:

- `IDR`
- `BITRATE:12000000`
- `RES:1280x720`
- `{"type":"resolution","resolution":{"width":1280,"height":720}}`

## iPad Receiver (Swift)

The repository includes iPad app source modules under `app/`:

- `DiscoveryService` browses `_screenstream._udp` with `NWBrowser`
- `TransportService` receives UDP RTP packets and reassembles HEVC FU packets
- `DecoderService` tracks VPS/SPS/PPS, converts Annex-B NALU payloads to AVCC, and enqueues samples to `AVSampleBufferDisplayLayer`

Minimum iOS target is intended to be iOS 16+.

## Current Implementation State

This MVP bootstrap includes:

- Daemon orchestration with capture -> encode -> transport pipeline
- Real capture path for `real-capture` builds: `ashpd` screencast session + PipeWire stream consumer connected with portal remote fd
- Real encode path for `real-encode` builds: persistent `ffmpeg` `hevc_vaapi` worker fed by raw BGRA frames
- RTP packetization logic for HEVC single-NAL and FU fragmentation
- UDP control channel for IDR and bitrate/resolution update messages
- Avahi advertisement wrapper (`avahi-publish-service`) with graceful fallback
- Swift receiver modules for discovery, RTP ingest, depacketization, decode surface wiring, and status UI

Known limitations in this bootstrap:

- Real `ashpd` + `PipeWire` and VA-API paths are feature-gated and include fallback behavior for environments where portal permission is denied or unsupported frame memory types are returned
- Capture falls back to synthetic frames if portal/PipeWire setup is unavailable in `auto` mode
- Encode falls back to bootstrap Annex-B access units if VA-API init or encoder worker startup is unavailable in `auto` mode
- Full VA-API zero-copy DMA-BUF path and production-grade jitter buffering are the next hardening steps

Daemon crate dependency notes:

- `ashpd = "0.13"` with `screencast` feature for portal integration
- `pipewire = "0.9"` for PipeWire runtime initialization
- `ffmpeg-next = "8"` for VA-API capability probing and encode-path integration

## Validation and Smoke Tests

Run Rust tests:

```bash
cd daemon
cargo test
```

Suggested manual smoke test:

1. Launch daemon on Linux with iPad IP configured.
2. Start iPad app and verify Bonjour discovery.
3. Confirm status changes to connected and display layer starts receiving samples.
4. Send control message `IDR` and verify decoder recovery path is exercised.

## Launch Checklist

- Confirm buildability on target Linux hardware.
- Validate 30-minute `1080p60` session stability.
- Measure end-to-end latency under idle and motion-heavy scenes.
- Verify reconnect behavior after Wi-Fi disruption and sender restarts.
- Document known MVP limitations (no virtual display creation, no input remoting yet).

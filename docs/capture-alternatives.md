# Capture Alternatives (Saved for Reference)

## Option A: VKMS + ext-image-copy-capture-v1
- VKMS built into kernel (`modprobe vkms`), zero external deps
- Capture via new Wayland `ext-image-copy-capture-v1` protocol (no PipeWire)
- Risk: Mutter may not detect VKMS outputs; capture protocol is very new (2024) and Mutter support is uncertain
- Rust crates: `wayland-client`, `wayland-protocols` (staging feature)

## Option B: GNOME RecordVirtual (Mutter DBus)
- `org.gnome.Mutter.ScreenCast.RecordVirtual` creates a virtual monitor via DBus
- **Still uses PipeWire** under the hood for frame delivery
- Discarded because user wants PipeWire removed entirely

## Option C: EVDI (CHOSEN)
- Kernel module by DisplayLink, purpose-built for virtual displays
- Simple C API: `evdi_open`, `evdi_connect`, `evdi_grab_pixels`
- Damage-driven capture, no PipeWire
- Risk: kernel compatibility (breaks on major kernel releases)
- User used this successfully in screx v1

## Option D: Custom kernel module
- Write our own minimal DRM virtual display driver
- Total control, no external dependencies
- Too much development effort for an MVP

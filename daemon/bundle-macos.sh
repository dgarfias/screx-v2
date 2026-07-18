#!/bin/bash
#
# bundle-macos.sh — assemble "Screx Daemon.app" on macOS.
#
# Unlike client/desktop/bundle-macos.sh, this doesn't need macdeployqt or
# dylib-reference rewriting: the daemon's only non-system dependency is
# Homebrew ffmpeg, which docs/DAEMON_MACOS.md already documents as a host
# build prerequisite. This script just wraps the built binary in the
# standard bundle layout with an icon and an ad-hoc signature.

set -euo pipefail

PROFILE="${1:-release}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BINARY="$SCRIPT_DIR/target/$PROFILE/screx"
APP_DIR="$SCRIPT_DIR/target/$PROFILE/Screx Daemon.app"
CONTENTS="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"

log() { echo "[bundle] $*"; }
die() { echo "error: $*" >&2; exit 1; }

[ -f "$BINARY" ] || die "binary not found at $BINARY (run 'cargo build --release' first)"

CODESIGN_BIN="$(xcrun -f codesign 2>/dev/null || echo /usr/bin/codesign)"

log "Assembling $APP_DIR ..."
rm -rf "$APP_DIR"
mkdir -p "$MACOS_DIR" "$RESOURCES"

cp "$BINARY" "$MACOS_DIR/screx"
cp "$SCRIPT_DIR/Info.plist" "$CONTENTS/Info.plist"

ICON_SRC="$SCRIPT_DIR/assets/AppIcon.png"
if [ -f "$ICON_SRC" ]; then
    ICONSET_DIR="$(mktemp -d)/AppIcon.iconset"
    mkdir -p "$ICONSET_DIR"
    for size in 16 32 128 256 512; do
        sips -z "$size" "$size" "$ICON_SRC" --out "$ICONSET_DIR/icon_${size}x${size}.png" >/dev/null 2>&1
        double=$((size * 2))
        sips -z "$double" "$double" "$ICON_SRC" --out "$ICONSET_DIR/icon_${size}x${size}@2x.png" >/dev/null 2>&1
    done
    iconutil -c icns "$ICONSET_DIR" -o "$RESOURCES/AppIcon.icns"
    rm -rf "$(dirname "$ICONSET_DIR")"
    log "Icon created"
else
    log "warning: icon source not found at $ICON_SRC, bundling without a custom icon"
fi

log "Code signing ..."
"$CODESIGN_BIN" --force --sign - --timestamp=none "$APP_DIR"

echo
log "Done: $APP_DIR"
echo "  Run it with: open \"$APP_DIR\""
echo "  Or from a terminal (to see stdout/stderr): \"$APP_DIR/Contents/MacOS/screx\""

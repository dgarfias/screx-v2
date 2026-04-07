#!/bin/bash
#
# bundle-macos.sh — Assemble a macOS .app bundle for Screx Desktop.
#
# Usage:
#   ./bundle-macos.sh [release|debug]
#
# Run from the client/desktop directory after cargo build.

set -euo pipefail

PROFILE="${1:-release}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

if [ "$PROFILE" = "release" ]; then
    BINARY="$SCRIPT_DIR/target/release/screx-desktop"
else
    BINARY="$SCRIPT_DIR/target/debug/screx-desktop"
fi

if [ ! -f "$BINARY" ]; then
    echo "error: binary not found at $BINARY"
    echo "       run 'cargo build --${PROFILE}' first"
    exit 1
fi

APP_DIR="$SCRIPT_DIR/target/$PROFILE/Screx.app"
CONTENTS="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"

echo "[bundle] Assembling $APP_DIR ..."

rm -rf "$APP_DIR"
mkdir -p "$MACOS_DIR" "$RESOURCES"

# Copy binary
cp "$BINARY" "$MACOS_DIR/screx-desktop"

# Copy Info.plist
cp "$SCRIPT_DIR/Info.plist" "$CONTENTS/Info.plist"

# Build .icns icon from the shared source PNG
ICON_SRC="$SCRIPT_DIR/../ipad/Screx/Assets.xcassets/AppIcon.appiconset/AppIcon.png"
if [ -f "$ICON_SRC" ]; then
    ICONSET_DIR=$(mktemp -d)/AppIcon.iconset
    mkdir -p "$ICONSET_DIR"
    for SIZE in 16 32 128 256 512; do
        sips -z $SIZE $SIZE "$ICON_SRC" --out "$ICONSET_DIR/icon_${SIZE}x${SIZE}.png" >/dev/null 2>&1
        DOUBLE=$((SIZE * 2))
        sips -z $DOUBLE $DOUBLE "$ICON_SRC" --out "$ICONSET_DIR/icon_${SIZE}x${SIZE}@2x.png" >/dev/null 2>&1
    done
    iconutil -c icns "$ICONSET_DIR" -o "$RESOURCES/AppIcon.icns"
    rm -rf "$(dirname "$ICONSET_DIR")"
    echo "[bundle] Icon created from $ICON_SRC"
else
    echo "[bundle] warning: icon source not found at $ICON_SRC, skipping icon"
fi

echo "[bundle] Done: $APP_DIR"

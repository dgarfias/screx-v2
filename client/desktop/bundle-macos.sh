#!/bin/bash
#
# bundle-macos.sh — Assemble a self-contained macOS .app bundle for Screx Desktop.
#
# Embeds Qt frameworks, QML modules, and FFmpeg dylibs inside the .app so it
# can be distributed as a standalone application to any macOS machine.
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
FRAMEWORKS="$CONTENTS/Frameworks"

echo "[bundle] Assembling $APP_DIR ..."

rm -rf "$APP_DIR"
mkdir -p "$MACOS_DIR" "$RESOURCES" "$FRAMEWORKS"

# ─── Copy binary ───────────────────────────────────────────────────────────────
cp "$BINARY" "$MACOS_DIR/screx-desktop"

# ─── Copy Info.plist ───────────────────────────────────────────────────────────
cp "$SCRIPT_DIR/Info.plist" "$CONTENTS/Info.plist"

# ─── Build .icns icon ──────────────────────────────────────────────────────────
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
    echo "[bundle] Icon created"
else
    echo "[bundle] warning: icon source not found at $ICON_SRC, skipping icon"
fi

# ─── Embed FFmpeg dylibs (before macdeployqt, which only handles Qt) ──────────
echo "[bundle] Embedding FFmpeg dylibs ..."

EMBEDDED_COUNT=0

copy_dylib_recursive() {
    local src="$1"
    local name
    name=$(basename "$src")

    # Skip if already copied
    [ -f "$FRAMEWORKS/$name" ] && return

    # Resolve symlinks
    local real_src
    real_src=$(realpath "$src" 2>/dev/null || echo "$src")
    [ ! -f "$real_src" ] && return

    cp "$real_src" "$FRAMEWORKS/$name"
    chmod 755 "$FRAMEWORKS/$name"
    EMBEDDED_COUNT=$((EMBEDDED_COUNT + 1))

    # Fix the dylib's own install name
    install_name_tool -id "@executable_path/../Frameworks/$name" "$FRAMEWORKS/$name" 2>/dev/null || true

    # Recursively copy this dylib's homebrew dependencies
    local deps
    deps=$(otool -L "$FRAMEWORKS/$name" | grep -oE '/opt/homebrew[^ ]+\.dylib' || true)
    for dep in $deps; do
        local dep_name
        dep_name=$(basename "$dep")
        install_name_tool -change "$dep" "@executable_path/../Frameworks/$dep_name" "$FRAMEWORKS/$name" 2>/dev/null || true
        copy_dylib_recursive "$dep"
    done
}

# Copy all homebrew dylibs referenced by the binary
HOMEBREW_LIBS=$(otool -L "$MACOS_DIR/screx-desktop" | grep -oE '/opt/homebrew[^ ]+\.dylib' || true)
for lib in $HOMEBREW_LIBS; do
    lib_name=$(basename "$lib")
    install_name_tool -change "$lib" "@executable_path/../Frameworks/$lib_name" "$MACOS_DIR/screx-desktop" 2>/dev/null || true
    copy_dylib_recursive "$lib"
done

echo "[bundle] Embedded $EMBEDDED_COUNT dylibs"

# ─── Deploy Qt frameworks + QML modules via macdeployqt ────────────────────────
echo "[bundle] Running macdeployqt (this may take a minute) ..."

MACDEPLOYQT=$(which macdeployqt 2>/dev/null || echo "")
if [ -z "$MACDEPLOYQT" ]; then
    echo "error: macdeployqt not found on PATH"
    echo "       install Qt or add its bin dir to PATH"
    exit 1
fi

# macdeployqt can hang on some systems, so run with a timeout.
"$MACDEPLOYQT" "$APP_DIR" -qmldir="$SCRIPT_DIR/qml" -always-overwrite &
DEPLOY_PID=$!
WAITED=0
while kill -0 "$DEPLOY_PID" 2>/dev/null; do
    sleep 1
    WAITED=$((WAITED + 1))
    if [ "$WAITED" -ge 120 ]; then
        echo "[bundle] macdeployqt timed out after 120s, killing ..."
        kill "$DEPLOY_PID" 2>/dev/null || true
        sleep 1
        kill -9 "$DEPLOY_PID" 2>/dev/null || true
        break
    fi
done
wait "$DEPLOY_PID" 2>/dev/null || true
echo "[bundle] macdeployqt finished"

# ─── Fix any remaining homebrew references ─────────────────────────────────────
echo "[bundle] Fixing library references ..."

# Scan all Mach-O files for leftover /opt/homebrew references and fix them
while IFS= read -r macho; do
    file "$macho" 2>/dev/null | grep -q "Mach-O" || continue
    deps=$(otool -L "$macho" 2>/dev/null | grep -oE '/opt/homebrew[^ ]+' || true)
    for dep in $deps; do
        dep_name=$(basename "$dep")
        if [ ! -f "$FRAMEWORKS/$dep_name" ]; then
            copy_dylib_recursive "$dep"
        fi
        install_name_tool -change "$dep" "@executable_path/../Frameworks/$dep_name" "$macho" 2>/dev/null || true
    done
done < <(find "$APP_DIR" -type f \( -name "*.dylib" -o -name "screx-desktop" \) 2>/dev/null)

# Also fix inside frameworks (they have the actual binary inside Versions/A/)
while IFS= read -r fw_binary; do
    file "$fw_binary" 2>/dev/null | grep -q "Mach-O" || continue
    deps=$(otool -L "$fw_binary" 2>/dev/null | grep -oE '/opt/homebrew[^ ]+' || true)
    for dep in $deps; do
        dep_name=$(basename "$dep")
        if [ ! -f "$FRAMEWORKS/$dep_name" ]; then
            copy_dylib_recursive "$dep"
        fi
        install_name_tool -change "$dep" "@executable_path/../Frameworks/$dep_name" "$fw_binary" 2>/dev/null || true
    done
done < <(find "$FRAMEWORKS" -type f -path "*/Versions/A/*" ! -name "*.prl" ! -name "*.plist" 2>/dev/null)

# ─── Verify ────────────────────────────────────────────────────────────────────
echo "[bundle] Verifying ..."
REMAINING=$(find "$APP_DIR" -type f \( -name "*.dylib" -o -name "screx-desktop" \) -exec otool -L {} \; 2>/dev/null | grep '/opt/homebrew' || true)
if [ -n "$REMAINING" ]; then
    echo "[bundle] WARNING: Some files still reference /opt/homebrew:"
    echo "$REMAINING" | head -10
else
    echo "[bundle] All dylib references are self-contained."
fi

# ─── Ad-hoc code sign (required on Apple Silicon) ─────────────────────────────
echo "[bundle] Code signing ..."
codesign --force --deep --sign - "$APP_DIR" 2>/dev/null || true

echo ""
echo "[bundle] Done: $APP_DIR"
echo ""
echo "  To distribute, zip the .app:"
echo "    cd target/$PROFILE && zip -r Screx.zip Screx.app"
echo ""

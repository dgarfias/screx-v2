#!/bin/bash
#
# bundle-macos.sh — assemble a self-contained Screx.app on macOS.
#
# Qt frameworks / QML modules are deployed with macdeployqt.
# Any remaining non-system dylibs/frameworks are discovered via otool and
# copied into Contents/Frameworks, with install names rewritten to point at
# the bundled copies.

set -euo pipefail

PROFILE="${1:-release}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

if [ "$PROFILE" = "release" ]; then
    BINARY="$SCRIPT_DIR/target/release/screx-desktop"
else
    BINARY="$SCRIPT_DIR/target/debug/screx-desktop"
fi

APP_DIR="$SCRIPT_DIR/target/$PROFILE/Screx.app"
CONTENTS="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"
FRAMEWORKS="$CONTENTS/Frameworks"
QML_RESOURCES="$RESOURCES/qml"

LIBPATHS=()

log() {
    echo "[bundle] $*"
}

warn() {
    echo "[bundle] warning: $*" >&2
}

die() {
    echo "error: $*" >&2
    exit 1
}

realpath_portable() {
    perl -MCwd=abs_path -e 'print abs_path(shift)' "$1"
}

list_deps() {
    otool -L "$1" 2>/dev/null | tail -n +2 | awk '{print $1}'
}

list_rpaths() {
    otool -l "$1" 2>/dev/null | awk '
        $1 == "cmd" && $2 == "LC_RPATH" { want = 1; next }
        want && $1 == "path" { print $2; want = 0 }
    '
}

is_system_ref() {
    case "$1" in
        /System/*|/usr/lib/*)
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

add_unique_path() {
    local path="$1"
    [ -d "$path" ] || return 0

    local existing
    for existing in "${LIBPATHS[@]:-}"; do
        [ "$existing" = "$path" ] && return 0
    done
    LIBPATHS+=("$path")
}

qt_query() {
    if command -v qmake >/dev/null 2>&1; then
        qmake -query "$1" 2>/dev/null || true
        return 0
    fi

    if command -v qtpaths >/dev/null 2>&1; then
        qtpaths --query "$1" 2>/dev/null || true
        return 0
    fi

    true
}

find_macdeployqt() {
    if [ -n "${MACDEPLOYQT:-}" ] && [ -x "$MACDEPLOYQT" ]; then
        echo "$MACDEPLOYQT"
        return 0
    fi

    if command -v macdeployqt >/dev/null 2>&1; then
        command -v macdeployqt
        return 0
    fi

    local qt_bins
    qt_bins="$(qt_query QT_HOST_BINS)"
    if [ -n "$qt_bins" ] && [ -x "$qt_bins/macdeployqt" ]; then
        echo "$qt_bins/macdeployqt"
        return 0
    fi

    return 1
}

expand_special_path() {
    local raw="$1"
    local owner="$2"
    local owner_dir
    owner_dir="$(dirname "$owner")"

    case "$raw" in
        @loader_path/*)
            echo "$owner_dir/${raw#@loader_path/}"
            ;;
        @executable_path/*)
            echo "$MACOS_DIR/${raw#@executable_path/}"
            ;;
        *)
            echo "$raw"
            ;;
    esac
}

resolve_reference() {
    local ref="$1"
    local owner="$2"
    local candidate
    local rpath
    local expanded

    if is_system_ref "$ref"; then
        return 1
    fi

    case "$ref" in
        /*)
            [ -e "$ref" ] || return 1
            realpath_portable "$ref"
            return 0
            ;;
        @loader_path/*|@executable_path/*)
            candidate="$(expand_special_path "$ref" "$owner")"
            [ -e "$candidate" ] || return 1
            realpath_portable "$candidate"
            return 0
            ;;
        @rpath/*)
            while IFS= read -r rpath; do
                [ -n "$rpath" ] || continue
                expanded="$(expand_special_path "$rpath" "$owner")"
                candidate="$expanded/${ref#@rpath/}"
                if [ -e "$candidate" ]; then
                    realpath_portable "$candidate"
                    return 0
                fi
            done < <(list_rpaths "$owner")

            local libpath
            for libpath in "${LIBPATHS[@]:-}"; do
                candidate="$libpath/${ref#@rpath/}"
                if [ -e "$candidate" ]; then
                    realpath_portable "$candidate"
                    return 0
                fi
            done
            ;;
    esac

    return 1
}

bundle_ref_for_path() {
    local path="$1"

    case "$path" in
        "$FRAMEWORKS"/*.framework/*)
            local rel="${path#"$FRAMEWORKS"/}"
            echo "@executable_path/../Frameworks/$rel"
            ;;
        "$FRAMEWORKS"/*)
            echo "@executable_path/../Frameworks/$(basename "$path")"
            ;;
        *)
            if [[ "$path" == *".framework/"* ]]; then
                local fw_root="${path%%.framework/*}.framework"
                local fw_name
                local rel_inside
                fw_name="$(basename "$fw_root")"
                rel_inside="${path#"$fw_root"/}"
                echo "@executable_path/../Frameworks/$fw_name/$rel_inside"
            else
                echo "@executable_path/../Frameworks/$(basename "$path")"
            fi
            ;;
    esac
}

set_bundled_id() {
    local path="$1"

    case "$path" in
        "$MACOS_DIR"/screx-desktop)
            return 0
            ;;
        "$FRAMEWORKS"/*.framework/*|"$FRAMEWORKS"/*.dylib)
            chmod u+w "$path" 2>/dev/null || true
            install_name_tool -id "$(bundle_ref_for_path "$path")" "$path" 2>/dev/null || true
            ;;
    esac
}

copy_dependency_to_bundle() {
    local source="$1"

    case "$source" in
        "$APP_DIR"/*)
            echo "$source"
            return 0
            ;;
    esac

    if [[ "$source" == *".framework/"* ]]; then
        local fw_root="${source%%.framework/*}.framework"
        local fw_name
        local dest_fw
        local dest_bin
        fw_name="$(basename "$fw_root")"
        dest_fw="$FRAMEWORKS/$fw_name"
        dest_bin="$dest_fw/${source#"$fw_root"/}"

        if [ ! -e "$dest_fw" ]; then
            log "Copying framework $fw_name"
            ditto "$fw_root" "$dest_fw"
        fi

        set_bundled_id "$dest_bin"
        echo "$dest_bin"
        return 0
    fi

    local name
    local dest
    name="$(basename "$source")"
    dest="$FRAMEWORKS/$name"

    if [ ! -f "$dest" ]; then
        log "Copying dylib $name"
        cp "$source" "$dest"
        chmod 755 "$dest"
    fi

    set_bundled_id "$dest"
    echo "$dest"
}

list_macho_files() {
    find "$APP_DIR" -type f 2>/dev/null | while IFS= read -r path; do
        file "$path" 2>/dev/null | grep -q 'Mach-O' && echo "$path"
    done
}

rewrite_bundle_dependencies() {
    local changed=0
    local macho
    local ref
    local source
    local bundled
    local new_ref

    while IFS= read -r macho; do
        [ -n "$macho" ] || continue
        set_bundled_id "$macho"

        while IFS= read -r ref; do
            [ -n "$ref" ] || continue
            if is_system_ref "$ref"; then
                continue
            fi

            if ! source="$(resolve_reference "$ref" "$macho" 2>/dev/null)"; then
                warn "Could not resolve dependency '$ref' from '$macho'"
                continue
            fi

            bundled="$(copy_dependency_to_bundle "$source")"
            new_ref="$(bundle_ref_for_path "$bundled")"

            if [ "$ref" != "$new_ref" ]; then
                chmod u+w "$macho" 2>/dev/null || true
                install_name_tool -change "$ref" "$new_ref" "$macho" 2>/dev/null || true
                changed=1
            fi
        done < <(list_deps "$macho")
    done < <(list_macho_files)

    echo "$changed"
}

verify_bundle() {
    local issues=0
    local macho
    local ref
    local source

    while IFS= read -r macho; do
        [ -n "$macho" ] || continue
        while IFS= read -r ref; do
            [ -n "$ref" ] || continue
            if is_system_ref "$ref"; then
                continue
            fi

            if ! source="$(resolve_reference "$ref" "$macho" 2>/dev/null)"; then
                echo "$macho -> $ref"
                issues=1
                continue
            fi

            case "$source" in
                "$APP_DIR"/*|/System/*|/usr/lib/*)
                    ;;
                *)
                    echo "$macho -> $ref -> $source"
                    issues=1
                    ;;
            esac
        done < <(list_deps "$macho")
    done < <(list_macho_files)

    return "$issues"
}

sign_nested_code() {
    local path

    while IFS= read -r path; do
        [ -n "$path" ] || continue
        "$CODESIGN_BIN" --force --sign - --timestamp=none "$path"
    done < <(find "$APP_DIR/Contents" -type f \( -name "*.dylib" -o -perm -111 \) | sort)
}

sign_nested_bundles() {
    local path

    while IFS= read -r path; do
        [ -n "$path" ] || continue
        "$CODESIGN_BIN" --force --sign - --timestamp=none "$path"
    done < <(find "$APP_DIR/Contents" -depth -type d \( -name "*.framework" -o -name "*.bundle" \) | sort)
}

build_icon() {
    local icon_src="$SCRIPT_DIR/assets/AppIcon.png"
    [ -f "$icon_src" ] || return 0

    local iconset_dir
    iconset_dir="$(mktemp -d)/AppIcon.iconset"
    mkdir -p "$iconset_dir"

    local size
    for size in 16 32 128 256 512; do
        sips -z "$size" "$size" "$icon_src" --out "$iconset_dir/icon_${size}x${size}.png" >/dev/null 2>&1
        local double
        double=$((size * 2))
        sips -z "$double" "$double" "$icon_src" --out "$iconset_dir/icon_${size}x${size}@2x.png" >/dev/null 2>&1
    done

    iconutil -c icns "$iconset_dir" -o "$RESOURCES/AppIcon.icns"
    rm -rf "$(dirname "$iconset_dir")"
    log "Icon created"
}

copy_qml_modules() {
    local qml_root="$1"
    [ -d "$qml_root" ] || return 0

    mkdir -p "$QML_RESOURCES"

    local module
    for module in QtQuick QtQml; do
        if [ -d "$qml_root/$module" ]; then
            log "Copying QML module tree $module"
            rm -rf "$QML_RESOURCES/$module"
            cp -R -L "$qml_root/$module" "$QML_RESOURCES/$module"
        fi
    done
}

[ -f "$BINARY" ] || die "binary not found at $BINARY"

MACDEPLOYQT_BIN="$(find_macdeployqt)" || die "macdeployqt not found on PATH (and could not find it via qmake)"

QT_INSTALL_LIBS="$(qt_query QT_INSTALL_LIBS)"
QT_INSTALL_QML="$(qt_query QT_INSTALL_QML)"
TARGET_LIB_DIR="$SCRIPT_DIR/target/lib"
QT_PREFIX="$(cd "$(dirname "$MACDEPLOYQT_BIN")/.." && pwd)"
CODESIGN_BIN="$(xcrun -f codesign 2>/dev/null || echo /usr/bin/codesign)"
QT_LIB_ROOT=""
QT_QML_ROOT=""

for candidate in "$QT_INSTALL_LIBS" "$QT_PREFIX/lib"; do
    if [ -n "$candidate" ] && [ -d "$candidate" ]; then
        QT_LIB_ROOT="$candidate"
        break
    fi
done

for candidate in "$QT_INSTALL_QML" "$QT_PREFIX/qml" "$QT_PREFIX/share/qt/qml"; do
    if [ -n "$candidate" ] && [ -d "$candidate" ]; then
        QT_QML_ROOT="$candidate"
        break
    fi
done

add_unique_path "$QT_LIB_ROOT"
add_unique_path "$QT_QML_ROOT"
add_unique_path "$QT_PREFIX/lib"
add_unique_path "$QT_PREFIX/qml"
add_unique_path "$QT_PREFIX/share/qt/qml"
add_unique_path "$TARGET_LIB_DIR"

while IFS= read -r dep; do
    [ -n "$dep" ] || continue
    case "$dep" in
        /*)
            add_unique_path "$(dirname "$dep")"
            ;;
    esac
done < <(list_deps "$BINARY")

log "Assembling $APP_DIR ..."
rm -rf "$APP_DIR"
mkdir -p "$MACOS_DIR" "$RESOURCES" "$FRAMEWORKS"

cp "$BINARY" "$MACOS_DIR/screx-desktop"
cp "$SCRIPT_DIR/Info.plist" "$CONTENTS/Info.plist"

build_icon

log "Running macdeployqt ..."
DEPLOY_CMD=("$MACDEPLOYQT_BIN" "$APP_DIR" "-always-overwrite")

for libpath in "${LIBPATHS[@]:-}"; do
    DEPLOY_CMD+=("-libpath=$libpath")
done

log "macdeployqt search paths: ${LIBPATHS[*]:-(none)}"

"${DEPLOY_CMD[@]}" &
DEPLOY_PID=$!
WAITED=0
while kill -0 "$DEPLOY_PID" 2>/dev/null; do
    sleep 1
    WAITED=$((WAITED + 1))
    if [ "$WAITED" -ge 300 ]; then
        warn "macdeployqt timed out after 300s, killing and continuing with manual fixups"
        kill "$DEPLOY_PID" 2>/dev/null || true
        sleep 1
        kill -9 "$DEPLOY_PID" 2>/dev/null || true
        break
    fi
done
wait "$DEPLOY_PID" 2>/dev/null || true

copy_qml_modules "$QT_QML_ROOT"

log "Rewriting non-system dependencies ..."
pass=1
while [ "$pass" -le 8 ]; do
    changed="$(rewrite_bundle_dependencies)"
    [ "$changed" = "0" ] && break
    pass=$((pass + 1))
done

log "Verifying ..."
if verify_bundle; then
    log "Bundle is self-contained."
else
    warn "Bundle still has unresolved or external dependencies listed above."
fi

log "Code signing ..."
sign_nested_code
sign_nested_bundles
"$CODESIGN_BIN" --force --sign - --timestamp=none "$APP_DIR"

echo
log "Done: $APP_DIR"
echo
echo "  To distribute, zip the .app:"
echo "    cd target/$PROFILE && zip -r Screx.zip Screx.app"
echo

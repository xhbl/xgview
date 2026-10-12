#!/usr/bin/env bash
#
# Builds an XGView.app bundle for macOS.
#
# The desktop build decodes with FFmpeg, so the same problem the Linux script
# solves applies here: the machine that unpacks the package has no FFmpeg. On
# Windows the vcpkg DLLs are copied beside the executable and found by name; on
# Linux the libraries are staged beside it and reached through an `$ORIGIN`
# rpath. macOS has neither - a Mach-O records the path each library is to be
# loaded from - so the libraries are copied into `Contents/Frameworks` and every
# reference to them is rewritten to `@rpath`: the executable looks under
# `@executable_path/../Frameworks`, and each library finds its neighbours through
# `@loader_path`. Only then is the bundle signed.
#
# The signature is ad-hoc (`codesign -s -`), which is enough to run the bundle on
# the machine that built it and on machines that copy it rather than download
# it. Handing it to anyone over the network needs a Developer ID certificate and
# notarisation, which is a separate step and is not done here.
#
# Both architectures are built and joined into one universal binary by default,
# so the same download runs on Intel and on Apple silicon. `--arch` narrows it.
# That holds only if the FFmpeg libraries are universal too - a thin library
# beside a fat executable starts on one machine only - so they are checked
# before the build rather than after it.
#
# Must be run on macOS: `install_name_tool`, `lipo`, `codesign` and `ditto` are
# Apple's own tools.
#
#   scripts/build-macos.sh
#   scripts/build-macos.sh --arch arm64
#
# Prerequisites
# -------------
#   The Xcode Command Line Tools (the four tools above, and `clang` for the
#   OpenH264 sources the workspace compiles), a Rust toolchain with the targets
#   being built (`rustup target add x86_64-apple-darwin aarch64-apple-darwin`),
#   and the FFmpeg development libraries, found through pkg-config - a Homebrew
#   `ffmpeg`, or one built from source with `PKG_CONFIG_PATH` pointed at it.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

usage() {
    cat <<'EOF'
Usage: scripts/build-macos.sh [--arch x86_64|arm64|universal]

  (no options)         one universal .app (x86_64 and arm64)
  --arch ARCH          build for a single architecture, or for both

Environment:
  PKG_CONFIG_PATH      where FFmpeg's .pc files are, when pkg-config does not
                       find them on its own (a self-built FFmpeg, say)
  XGVIEW_OUT_ROOT      output root (default <repo>/target)
EOF
}

# ---------------------------------------------------------------- args
ARCH=universal
while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch)   ARCH="${2:?--arch needs a value}"; shift ;;
        --arch=*) ARCH="${1#*=}" ;;
        -h|--help) usage; exit 0 ;;
        *) echo "error: unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done

case "$ARCH" in
    x86_64)    TARGETS=(x86_64-apple-darwin) ;;
    arm64)     TARGETS=(aarch64-apple-darwin) ;;
    universal) TARGETS=(x86_64-apple-darwin aarch64-apple-darwin) ;;
    *) echo "error: --arch takes x86_64, arm64 or universal, not '$ARCH'" >&2; exit 2 ;;
esac

OUT_ROOT="${XGVIEW_OUT_ROOT:-$REPO_ROOT/target}"
# Cargo writes `<triple>/release/xgview` under here; the .app and the zip are
# built beside it, so everything a run produces is under one root.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$OUT_ROOT}"

# Reads the version from Cargo.toml. It is declared once, in
# `[workspace.package]`, and inherited by every crate, so that is the section
# that carries the literal; a `[package]` that states its own version is
# honoured too. Read rather than repeated, so the bundle cannot drift from the
# binary inside it.
read_version() {
    local manifest="$1" section="" package_version=""
    while IFS= read -r line; do
        if [[ "$line" =~ ^[[:space:]]*\[(.+)\][[:space:]]*$ ]]; then
            section="${BASH_REMATCH[1]}"
            continue
        fi
        if [[ "$line" =~ ^[[:space:]]*version[[:space:]]*=[[:space:]]*\"([^\"]+)\" ]]; then
            if [[ "$section" == "workspace.package" ]]; then
                printf '%s\n' "${BASH_REMATCH[1]}"
                return 0
            fi
            if [[ "$section" == "package" && -z "$package_version" ]]; then
                package_version="${BASH_REMATCH[1]}"
            fi
        fi
    done < "$manifest"
    if [[ -n "$package_version" ]]; then
        printf '%s\n' "$package_version"
        return 0
    fi
    echo "error: no version found in $manifest ([workspace.package] or [package])" >&2
    return 1
}

VERSION="$(read_version "$REPO_ROOT/Cargo.toml")"
APP_NAME="XGView"
APP="$OUT_ROOT/$APP_NAME.app"
PACKAGE_NAME="xgview-$VERSION-macos-$ARCH"
ZIP="$OUT_ROOT/$PACKAGE_NAME.zip"
ICON="$REPO_ROOT/assets/icons/macos/xgview.icns"

# ---------------------------------------------------------------- checks
#
# The four tools the bundle is assembled with, named up front so a machine
# without the Command Line Tools fails here rather than halfway through a build.
for tool in install_name_tool lipo codesign ditto otool; do
    command -v "$tool" >/dev/null 2>&1 \
        || { echo "error: $tool not found - install the Xcode Command Line Tools" >&2; exit 1; }
done

[[ -f "$ICON" ]] \
    || { echo "error: $ICON is missing - run scripts/make-icons.py" >&2; exit 1; }

# Where the FFmpeg libraries live. Both the copy step and the rewrite step need
# it, and a machine where pkg-config cannot name it has no usable FFmpeg at all.
FFMPEG_LIBDIR="$(pkg-config --variable=libdir libavcodec || true)"
if [[ -z "$FFMPEG_LIBDIR" || ! -d "$FFMPEG_LIBDIR" ]]; then
    echo "error: pkg-config cannot find libavcodec's library directory." >&2
    echo "       Install FFmpeg (Homebrew), or build it and point PKG_CONFIG_PATH at it." >&2
    exit 1
fi

# A universal executable joined to thin libraries is a bundle that starts on one
# machine only, and the join happens at the end of the build - so the libraries
# are asked now, when the answer still costs nothing. Both architectures of
# FFmpeg have to have been built into the one prefix for this to hold.
if [[ "$ARCH" == universal ]]; then
    for lib in "$FFMPEG_LIBDIR"/*.dylib; do
        [[ -f "$lib" && ! -L "$lib" ]] || continue
        archs="$(lipo -archs "$lib")"
        for wanted in x86_64 arm64; do
            case " $archs " in
                *" $wanted "*) ;;
                *)
                    echo "error: $(basename "$lib") in $FFMPEG_LIBDIR is '$archs', not universal." >&2
                    echo "       Build FFmpeg for both architectures into this prefix (see the" >&2
                    echo "       macOS setup notes), or pass --arch x86_64 for a one-machine bundle." >&2
                    exit 1
                    ;;
            esac
        done
    done
fi

# ---------------------------------------------------------------- cargo
build_cargo() {
    # pkg-config refuses to answer for a target that is not the host - "has not
    # been configured to support cross-compilation" - and building the second
    # architecture of a universal binary is exactly that. Letting it answer is
    # safe here: the same prefix responds, its libraries carry both
    # architectures, and the linker takes the slice it is building for. The
    # headers do not differ by architecture, so there is nothing for a sysroot to
    # correct.
    export PKG_CONFIG_ALLOW_CROSS=1
    local target
    for target in "${TARGETS[@]}"; do
        echo "==> Building xgview (release, $target)"
        cargo build --release --locked -p xgview --bin xgview --target "$target"
    done
}

# ---------------------------------------------------------------- binary
#
# One executable for the bundle: the binary as built, or the two joined into a
# universal one. `lipo -create` is what makes a single file that runs natively
# on either architecture.
place_executable() {
    local destination="$APP/Contents/MacOS/xgview"
    if [[ "$ARCH" == universal ]]; then
        echo "==> Joining the two architectures"
        lipo -create -output "$destination" \
            "$CARGO_TARGET_DIR/x86_64-apple-darwin/release/xgview" \
            "$CARGO_TARGET_DIR/aarch64-apple-darwin/release/xgview"
    else
        cp "$CARGO_TARGET_DIR/${TARGETS[0]}/release/xgview" "$destination"
    fi
    chmod +x "$destination"
}

# ---------------------------------------------------------------- info.plist
#
# The keys a bundle needs to be one: what it is called, what it starts, its icon,
# and that it is an application rather than a folder. `LSMinimumSystemVersion`
# is the floor the toolkit asks for; `NSHighResolutionCapable` keeps it sharp on
# a Retina display.
write_info_plist() {
    cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>${APP_NAME}</string>
    <key>CFBundleDisplayName</key>
    <string>${APP_NAME}</string>
    <key>CFBundleIdentifier</key>
    <string>com.xhbl.xgview</string>
    <key>CFBundleExecutable</key>
    <string>xgview</string>
    <key>CFBundleIconFile</key>
    <string>xgview.icns</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST
}

# ---------------------------------------------------------------- frameworks
#
# Every FFmpeg library the executable needs is copied in, and every reference to
# one of them is rewritten to load it from beside itself rather than from the
# prefix it was built in.
#
# Both the copy and the rewrite are driven by the references read out of the
# Mach-O headers with `otool -L`, rather than by a listing of the prefix, so that
# a library lands under exactly the name the loader will ask for. That name is
# not necessarily the name of the file behind it - a tree that keeps a versioned
# image answers to `libavutil.60.dylib` through a symlink - so the copy follows
# the link (`cp -L`) and keeps the name that was asked for. The walk is
# transitive, the libraries referring to one another; a reference to a system
# library (`libz`, `libiconv`, ...) is left alone, those being on every Mac.
copy_and_rewrite_frameworks() {
    local frameworks="$APP/Contents/Frameworks"
    local binary="$APP/Contents/MacOS/xgview"
    echo "==> Bundling FFmpeg from $FFMPEG_LIBDIR"

    # `queue` is what still has to be walked - the executable first, then each
    # library copied for it - and `copied` is the names, which the rewrite pass
    # below then goes over.
    local queue=("$binary") file dep name index=0
    local copied=()
    while [[ "$index" -lt "${#queue[@]}" ]]; do
        file="${queue[$index]}"
        index=$((index + 1))
        while read -r dep; do
            [[ "$dep" == "$FFMPEG_LIBDIR"/* ]] || continue
            name="$(basename "$dep")"
            [[ " ${copied[*]-} " == *" $name "* ]] && continue
            copied+=("$name")
            cp -L "$dep" "$frameworks/$name"
            queue+=("$frameworks/$name")
        done < <(otool -L "$file" | tail -n +2 | awk '{print $1}')
    done

    if [[ "${#copied[@]}" -eq 0 ]]; then
        echo "error: the executable references nothing under $FFMPEG_LIBDIR." >&2
        echo "       Is the FFmpeg it was linked against the one pkg-config names?" >&2
        exit 1
    fi

    # Each library: its own id first, so a reference to it can be `@rpath/<name>`,
    # then `@loader_path` on the search path so that `@rpath` means "the folder I
    # am in", then its references to the others.
    local lib
    for name in "${copied[@]}"; do
        lib="$frameworks/$name"
        install_name_tool -id "@rpath/$name" "$lib"
        # Already present on a second run, which is not an error.
        install_name_tool -add_rpath @loader_path "$lib" 2>/dev/null || true
        while read -r dep; do
            [[ "$dep" == "$FFMPEG_LIBDIR"/* ]] || continue
            install_name_tool -change "$dep" "@rpath/$(basename "$dep")" "$lib"
        done < <(otool -L "$lib" | tail -n +2 | awk '{print $1}')
    done

    # The executable looks in the Frameworks folder, one up from it and across -
    # `Contents/MacOS` beside `Contents/Frameworks`.
    install_name_tool -add_rpath @executable_path/../Frameworks "$binary" 2>/dev/null || true
    while read -r dep; do
        [[ "$dep" == "$FFMPEG_LIBDIR"/* ]] || continue
        install_name_tool -change "$dep" "@rpath/$(basename "$dep")" "$binary"
    done < <(otool -L "$binary" | tail -n +2 | awk '{print $1}')
}

# ---------------------------------------------------------------- sign
#
# Ad-hoc, and inside out: the libraries first, then the bundle that contains
# them, since signing a bundle seals what is inside it. `-` is the ad-hoc
# identity - no certificate, no keychain - which is what a locally built app
# needs and all it needs.
sign_app() {
    echo "==> Signing (ad-hoc)"
    local lib
    for lib in "$APP"/Contents/Frameworks/*.dylib; do
        codesign --force --sign - "$lib"
    done
    codesign --force --sign - "$APP"
}

# ---------------------------------------------------------------- bundle
bundle_app() {
    echo "==> Building $APP"
    rm -rf "$APP"
    mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$APP/Contents/Frameworks"

    place_executable
    cp "$ICON" "$APP/Contents/Resources/xgview.icns"
    write_info_plist

    # Language packs go in `Contents/Resources/langs`, not beside the executable:
    # `Contents/MacOS` is where a bundle keeps code, and `codesign` refuses a
    # `.ftl` there - "code object is not signed at all". `monitor_i18n::init`
    # looks in the executable's own folder and then in `Resources/langs` beside
    # it, so a bundle is found either way. English is embedded in the binary;
    # every other language travels here.
    if [[ -d "$REPO_ROOT/langs" ]]; then
        cp -R "$REPO_ROOT/langs" "$APP/Contents/Resources/langs"
    fi

    copy_and_rewrite_frameworks
    sign_app
}

# ---------------------------------------------------------------- pack
pack_package() {
    echo "==> Packing $ZIP"
    rm -f "$ZIP"
    # `ditto` rather than `zip`: it keeps the permissions and the extended
    # attributes a code signature is written into, which `zip` does not.
    ditto -c -k --sequesterRsrc --keepParent "$APP" "$ZIP"
}

# ---------------------------------------------------------------- verify
#
# Read the finished bundle back: the architectures it holds, that every FFmpeg
# library it names resolves inside it rather than to a prefix on this machine,
# and that the signature still verifies after the rewrite.
verify_package() {
    echo "==> Verifying $APP"
    local binary="$APP/Contents/MacOS/xgview"

    echo "--- architectures ---"
    lipo -info "$binary"

    echo "--- FFmpeg libraries ---"
    # Every one has to be an `@rpath` reference *and* the file it names has to be
    # there. A bundle that asks for `@rpath/libavutil.60.dylib` while carrying
    # `libavutil.60.8.103.dylib` reads as rewritten and crashes at launch, which
    # is the failure this is here to catch.
    local failed=0 dep name
    while read -r dep; do
        name="$(basename "$dep")"
        if [[ "$dep" == @rpath/* && -f "$APP/Contents/Frameworks/$name" ]]; then
            echo "    $dep"
        else
            echo "error: $dep is not inside the bundle, or the file it names is missing" >&2
            failed=1
        fi
    done < <(otool -L "$binary" | awk '/libav|libsw/ {print $1}')

    echo "--- language packs ---"
    if [[ -d "$APP/Contents/Resources/langs" ]]; then
        ls "$APP/Contents/Resources/langs" | sed 's/^/    /'
    else
        echo "    (none; only the embedded English is available)"
    fi

    echo "--- signature ---"
    if codesign --verify --deep --strict "$APP" 2>/dev/null; then
        echo "    ok"
        codesign -dv "$APP" 2>&1 | sed 's/^/    /'
    else
        echo "error: the signature does not verify" >&2
        failed=1
    fi

    if [[ "$failed" -ne 0 ]]; then
        echo "==> verification FAILED" >&2
        exit 1
    fi

    local size_bytes
    size_bytes="$(wc -c < "$ZIP" | tr -d ' ')"
    printf '==> %s (%.1f MB)\n' "$ZIP" "$(awk -v b="$size_bytes" 'BEGIN { print b / 1048576 }')"
}

# ---------------------------------------------------------------- run
echo "==> XGView $VERSION, macos-$ARCH"

mkdir -p "$OUT_ROOT"
build_cargo
bundle_app
pack_package
verify_package
echo "==> Done"

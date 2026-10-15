#!/usr/bin/env bash
#
# Builds a portable release package of XGView for Linux.
#
# The desktop build decodes with FFmpeg. A distribution's FFmpeg is linked
# against the whole world - x264, x265, dav1d, rav1e, libplacebo, librsvg, ... -
# so `ldd` on a binary built against it names close to two hundred libraries,
# none of which a machine that never installed them can load. Windows sidesteps
# this by shipping the vcpkg FFmpeg DLLs beside the executable (see
# build-windows.ps1); this script does the same for Linux. It builds a trimmed
# FFmpeg - only the decoders the viewer asks for, and no third party
# dependency - puts its shared libraries next to the executable, and lets the
# loader find them there through an `$ORIGIN` rpath. The result is a tar.gz
# that runs from wherever it is unpacked, on a machine with nothing installed.
#
# Two ways to run it. On a build machine that already has the tool chain (rustc,
# gcc, make, pkg-config, libclang) it builds in place, into `target/`:
#
#   scripts/build-linux.sh
#   FFMPEG_VERSION=8.0.3 scripts/build-linux.sh   # pin another 8.0.x source
#
# Or it builds inside a container, which is how the package's C library floor is
# chosen rather than inherited from whatever host ran the build:
#
#   scripts/build-linux.sh --docker
#   scripts/build-linux.sh --docker --docker-image rockylinux:8
#
# The container image (scripts/Dockerfile.linux-build, tag xgview-linux-build)
# is built on first use and reused afterwards. rockylinux:8 carries glibc 2.28,
# so every GLIBC_* symbol the linker leaves in the executable and the FFmpeg
# libraries is one that RHEL 8, Debian 10, Ubuntu 20.04 and Fedora 32 and newer
# all provide - unlike a build on a current host, which pins the package to that
# host's glibc. The container's whole build tree (FFmpeg sources, cargo's target
# directory, the staging folder) lives in a persistent docker volume, so a
# second run resumes rather than rebuilding; only the finished tarball is copied
# back to `target/`, and the repository is mounted read only.
#
# Prerequisites
# -------------
#   Host build: rustc / cargo, gcc, make, pkg-config, libclang (bindgen), and
#   meson, ninja and the libdrm development files on the build machine - the
#   last three build the bundled libva. `curl` and `tar` fetch and unpack the
#   FFmpeg, libva and nv-codec-headers sources on the first run. `nasm` (or
#   `yasm`) is optional: without it FFmpeg is configured with `--disable-x86asm`
#   and software decoding loses its hand written SIMD paths.
#
#   Container build: a working docker. Under WSL the linux distribution's socket
#   can be unreachable while the Windows client (`docker.exe`) still is; point
#   XGVIEW_DOCKER at it in that case:
#
#     XGVIEW_DOCKER=docker.exe scripts/build-linux.sh --docker
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

usage() {
    cat <<'EOF'
Usage: scripts/build-linux.sh [--docker] [--docker-image IMAGE]

  (no options)             build on this machine, into target/
  --docker                 build inside a container (default rockylinux:8)
  --docker-image IMAGE     use another base image for the container build

Environment:
  FFMPEG_VERSION           FFmpeg source to build   (default 8.0.3)
  LIBVA_VERSION            libva source to build    (default 2.22.0)
  NVCODEC_VERSION          nv-codec-headers tag     (default n13.1.15.0)
  XGVIEW_VA_DRIVER_DIRS    VA driver directories compiled into the bundled
                           libva, colon separated (default: the usual
                           multiarch, lib64 and lib locations)
  XGVIEW_OUT_ROOT          output root              (default <repo>/target)
  XGVIEW_DOCKER            docker client            (default docker)
  XGVIEW_DOCKER_IMAGE_TAG  container image tag      (default derived)
  XGVIEW_DOCKER_VOLUME     docker volume for the build tree (default below)
EOF
}

# ---------------------------------------------------------------- args
#
# `--docker` is answered by rebuilding the same script inside the container with
# XGVIEW_BUILD_IN_CONTAINER set, so the inner run must not try to nest another
# one. The flag is only read on the outside.
DOCKER_MODE=0
DOCKER="${XGVIEW_DOCKER:-docker}"
DOCKER_IMAGE="${XGVIEW_DOCKER_IMAGE:-rockylinux:8}"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --docker)         DOCKER_MODE=1 ;;
        --docker-image)   DOCKER_IMAGE="${2:?--docker-image needs a value}"; shift ;;
        --docker-image=*) DOCKER_IMAGE="${1#*=}" ;;
        -h|--help)        usage; exit 0 ;;
        *) echo "error: unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done

# The FFmpeg release to build. 8.0 is the newest release branch the workspace's
# `ffmpeg-next` crate is written against; its FFmpeg major is the one the crate
# itself is versioned for.
FFMPEG_VERSION="${FFMPEG_VERSION:-8.0.3}"

# The two inputs hardware decoding needs at build time, both built here rather
# than taken from the distribution. libva is what VAAPI (Intel/AMD) asks for;
# nv-codec-headers is the header-only release FFmpeg's CUDA/NVDEC support is
# written against.
LIBVA_VERSION="${LIBVA_VERSION:-2.22.0}"
NVCODEC_VERSION="${NVCODEC_VERSION:-n13.1.15.0}"

# Where the VA-API drivers live on the machines the package is meant for, as
# the default search path of the bundled libva. The package carries its own
# libva (see `build_hwcodecs`), and that copy has to name the directories the
# *running* machine keeps its drivers in: the distribution's libva was compiled
# for its own layout, this one cannot know it. libva splits the path on ':',
# and `LIBVA_DRIVERS_PATH` in the environment still overrides it.
VA_DRIVER_DIRS="${XGVIEW_VA_DRIVER_DIRS:-/usr/lib/x86_64-linux-gnu/dri:/usr/lib64/dri:/usr/lib/dri:/usr/lib/aarch64-linux-gnu/dri}"

# Everything the build writes goes under here: the FFmpeg working tree, cargo's
# target directory, the staging folder and the tarball. On the host that is
# `target/`, unchanged. The container run points it at the mounted volume,
# which is what keeps the two apart - a host `target/` and a container `target/`
# are linked against different C libraries and mixing them would make cargo
# rebuild the whole graph on every switch.
ARCH="$(uname -m)"
OUT_ROOT="${XGVIEW_OUT_ROOT:-$REPO_ROOT/target}"
WORK_DIR="$OUT_ROOT/ffmpeg-linux-$ARCH"
SRC_DIR="$WORK_DIR/build"
PREFIX="$WORK_DIR/prefix"
DL_DIR="$WORK_DIR/downloads"
# libva and the nv-codec-headers each get their own prefix: both are inputs to
# the FFmpeg configure step, and neither belongs in the FFmpeg tree itself.
LIBVA_PREFIX="$WORK_DIR/libva"
NVCODEC_PREFIX="$WORK_DIR/nvcodec"

# Cargo would otherwise default to `target/`; routing it through OUT_ROOT lets
# the container keep its objects in the volume (`/work/out`) while the host
# keeps the previous `target/release` layout.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$OUT_ROOT}"

# Reads the version from Cargo.toml. It is declared once, in
# `[workspace.package]`, and inherited by every crate, so that is the section
# that carries the literal; a `[package]` that states its own version is
# honoured too. Read rather than repeated, so the package name cannot drift
# from the binary inside it.
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
PACKAGE_NAME="xgview-$VERSION-linux-$ARCH"
PACKAGE_DIR="$OUT_ROOT/$PACKAGE_NAME"
TARBALL="$OUT_ROOT/$PACKAGE_NAME.tar.gz"

# ---------------------------------------------------------------- hwcodecs
#
# What hardware decoding needs at build time, beyond the decoders themselves.
# Two inputs, and the two could hardly behave more differently at run time:
#
#   nv-codec-headers - the header-only release FFmpeg's CUDA and NVDEC support
#     is written against. Nothing here is linked: libcuda and libnvcuvid are
#     opened at run time through FFmpeg's dynlink shims, so an NVIDIA-less
#     machine does not fail to start - the CUDA device simply does not open and
#     the next device on the list, or the CPU, takes the stream. The CUDA
#     toolkit is not needed to build this.
#
#   libva - the opposite case. VAAPI is not opened at run time: libavutil and
#     libavcodec link it, so a machine without it cannot start the program at
#     all. A distribution's libva is therefore not reused - one is built here
#     and travels in the package (see `stage_package`). Its driver search path
#     is compiled in as `VA_DRIVER_DIRS`, because the driver is the target's
#     and this libva has to be told where that machine keeps it.
build_hwcodecs() {
    local marker="$WORK_DIR/.xgview-hwcodecs-version" build_id
    build_id="$NVCODEC_VERSION $LIBVA_VERSION"
    if [[ -f "$NVCODEC_PREFIX/lib/pkgconfig/ffnvcodec.pc" \
          && -f "$LIBVA_PREFIX/lib/libva.so" \
          && -f "$marker" && "$(cat "$marker")" == "$build_id" ]]; then
        echo "==> hardware codec inputs already built ($build_id, skipping)"
        return 0
    fi

    local tool
    for tool in meson ninja curl tar; do
        command -v "$tool" >/dev/null 2>&1 \
            || { echo "error: '$tool' is required to build libva" >&2; exit 1; }
    done
    pkg-config --exists libdrm \
        || { echo "error: the libdrm development files are required to build libva" >&2; exit 1; }
    mkdir -p "$DL_DIR" "$SRC_DIR" "$NVCODEC_PREFIX" "$LIBVA_PREFIX"

    # The headers are the whole install, so unpacking to the source tree and
    # running `make install` with a prefix is the entire build.
    local nv_tar="$DL_DIR/nv-codec-headers-$NVCODEC_VERSION.tar.gz"
    if [[ ! -f "$nv_tar" ]]; then
        echo "    downloading nv-codec-headers $NVCODEC_VERSION"
        curl -fL --retry 3 -o "$nv_tar" \
            "https://github.com/FFmpeg/nv-codec-headers/archive/refs/tags/$NVCODEC_VERSION.tar.gz"
    fi
    local nv_src="$SRC_DIR/nv-codec-headers-$NVCODEC_VERSION"
    [[ -d "$nv_src" ]] || tar -xzf "$nv_tar" -C "$SRC_DIR"
    echo "==> nv-codec-headers $NVCODEC_VERSION into $NVCODEC_PREFIX"
    make -C "$nv_src" install PREFIX="$NVCODEC_PREFIX" >/dev/null

    # libva, DRM only. The X11, GLX and Wayland displays are not wanted: the
    # decoder reaches the device through a DRM render node, which is the path
    # that works headless as well as on a desktop, and each of the other three
    # would add a library to carry and a display protocol to depend on.
    local va_tar="$DL_DIR/libva-$LIBVA_VERSION.tar.gz"
    if [[ ! -f "$va_tar" ]]; then
        echo "    downloading libva $LIBVA_VERSION"
        curl -fL --retry 3 -o "$va_tar" \
            "https://github.com/intel/libva/archive/refs/tags/$LIBVA_VERSION.tar.gz"
    fi
    local va_src="$SRC_DIR/libva-$LIBVA_VERSION"
    [[ -d "$va_src" ]] || tar -xzf "$va_tar" -C "$SRC_DIR"
    echo "==> libva $LIBVA_VERSION into $LIBVA_PREFIX"
    rm -rf "$va_src/build"
    (
        cd "$va_src"
        meson setup build --prefix="$LIBVA_PREFIX" --libdir=lib --buildtype=release \
            -Dwith_x11=no -Dwith_glx=no -Dwith_wayland=no -Dwith_win32=no \
            -Ddriverdir="$VA_DRIVER_DIRS" >/dev/null
        ninja -C build >/dev/null
        ninja -C build install >/dev/null
    )

    printf '%s\n' "$build_id" > "$marker"
}

# ---------------------------------------------------------------- ffmpeg
#
# Only the decoders the pipeline can ask for are compiled in. `monitor_codec`
# hands `libavcodec` complete Annex-B access units - the project's own RTSP
# client and RTP depacketiser (monitor_core::rtsp / monitor_core::h264) do the
# demuxing - so no demuxer, protocol, parser, device or filter is needed at
# run time. The codecs it maps an SDP encoding name onto are h264, hevc and
# mjpeg (crates/monitor_codec/src/lib.rs, `Codec::from_encoding`); mpeg4 is
# kept beside them because it costs little and a camera that announces it
# should still decode. `swscale` (the NV12 conversion) and `swresample` stay
# on: the first is used on every frame, the second only has to exist.
#
# libavformat / libavfilter / libavdevice carry no component at all but are
# still built: the `ffmpeg-sys-next` build script links all seven libraries
# whenever the crate's default features are on, so every one of them has to
# exist for the executable to link. The linker then records only the ones
# something actually reaches, but all seven travel in the package - they are
# ours, they are tiny next to the decoding library, and keeping the set whole
# means a future feature that does reach one of them still finds it.
#
# Hardware decoding is compiled in as well: the VAAPI and CUDA hwaccels for
# H.264 and H.265, the two codecs the pipeline asks a device for (`prefer_hardware`
# covers both, crates/monitor_core/src/pipeline.rs). Both `*_vaapi` and `*_nvdec`
# have to be named one by one: `--enable-vaapi` / `--enable-nvdec` bring in the
# hwaccel framework, but the per-codec modules are what libavcodec actually opens,
# and a codec without one decodes on the CPU whatever device was handed over. The
# headers and the libva both need come from `build_hwcodecs`; the CUDA side links
# nothing.
build_ffmpeg() {
    # nasm/yasm assemble the hand written x86 SIMD. Without one of them
    # `--disable-x86asm` is required and software decoding loses those paths;
    # the C paths and inline assembly still work. Which of the two it was is
    # part of what the tree below is keyed on: installing nasm later has to be
    # able to rebuild the libraries, and a marker naming the release alone would
    # keep the slower ones.
    local asm_flags=() asm_state="asm" build_id
    if ! command -v nasm >/dev/null 2>&1 && ! command -v yasm >/dev/null 2>&1; then
        asm_flags+=(--disable-x86asm)
        asm_state="noasm"
    fi
    # The hwaccel set is part of what the tree is keyed on as well: a build that
    # gained a codec's hwaccel has to rebuild, or the marker would keep libraries
    # that cannot decode it on the GPU however `prefer_hardware` reads.
    local hwaccels="vaapi+nvdec:h264,hevc"
    build_id="$FFMPEG_VERSION $asm_state libva-$LIBVA_VERSION $NVCODEC_VERSION $hwaccels"

    if [[ -f "$PREFIX/lib/libavcodec.so" && -f "$PREFIX/include/libavcodec/avcodec.h" \
          && -f "$PREFIX/.xgview-ffmpeg-version" \
          && "$(cat "$PREFIX/.xgview-ffmpeg-version")" == "$build_id" ]]; then
        echo "==> FFmpeg $build_id already built in $PREFIX (skipping)"
        return 0
    fi

    if [[ "$asm_state" == "noasm" ]]; then
        echo "    warning: nasm/yasm not found, configuring with --disable-x86asm" >&2
        echo "    warning: software decoding will be slower than a build with SIMD" >&2
    fi
    echo "==> Building FFmpeg $FFMPEG_VERSION into $PREFIX"
    mkdir -p "$DL_DIR"
    local ffmpeg_tar="$DL_DIR/ffmpeg-$FFMPEG_VERSION.tar.xz"
    if [[ ! -f "$ffmpeg_tar" ]]; then
        echo "    downloading $ffmpeg_tar"
        curl -fL --retry 3 -o "$ffmpeg_tar" \
            "https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz"
    fi
    mkdir -p "$SRC_DIR"
    local ffmpeg_src="$SRC_DIR/ffmpeg-$FFMPEG_VERSION"
    if [[ ! -d "$ffmpeg_src" ]]; then
        tar -xJf "$ffmpeg_tar" -C "$SRC_DIR"
    fi

    cd "$ffmpeg_src"
    # A source tree left half configured by an interrupted run would otherwise
    # keep stale decisions.
    make distclean >/dev/null 2>&1 || true
    # The configure step finds libva and the codec headers through pkg-config,
    # so both prefixes go on the search path (build_cargo inherits it and
    # appends the FFmpeg tree's own).
    export PKG_CONFIG_PATH="$NVCODEC_PREFIX/lib/pkgconfig:$LIBVA_PREFIX/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
    ./configure \
        --prefix="$PREFIX" \
        --enable-shared \
        --disable-static \
        --disable-programs \
        --disable-doc \
        --disable-autodetect \
        --disable-network \
        --disable-everything \
        --disable-debug \
        --enable-decoder=h264 \
        --enable-decoder=hevc \
        --enable-decoder=mjpeg \
        --enable-decoder=mpeg4 \
        --enable-vaapi \
        --enable-ffnvcodec \
        --enable-nvdec \
        --enable-hwaccel=h264_vaapi \
        --enable-hwaccel=h264_nvdec \
        --enable-hwaccel=hevc_vaapi \
        --enable-hwaccel=hevc_nvdec \
        "${asm_flags[@]}"
    make -j"$(nproc 2>/dev/null || echo 4)"
    make install
    printf '%s\n' "$build_id" > "$PREFIX/.xgview-ffmpeg-version"
    cd "$REPO_ROOT"
}

# ---------------------------------------------------------------- cargo
#
# The `ffmpeg-sys-next` build script looks for a prebuilt FFmpeg through
# `FFMPEG_DIR` before it tries vcpkg or pkg-config, taking the headers from
# `$FFMPEG_DIR/include` and the libraries from `$FFMPEG_DIR/lib`. The
# pkg-config path is pointed at the same tree as well, so a transitive lookup
# cannot wander off to the distribution's FFmpeg 6.
build_cargo() {
    echo "==> Building xgview (release)"
    export FFMPEG_DIR="$PREFIX"
    export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
    # `$ORIGIN` is the loader's name for the directory the executable sits in.
    # It has to reach the linker as this literal token, hence the single quotes.
    # `--disable-new-dtags` asks for the old DT_RPATH rather than DT_RUNPATH: only
    # RPATH is inherited by the dependencies, which is what lets libavformat find
    # libavcodec find libavutil, all side by side, without an LD_LIBRARY_PATH.
    RUSTFLAGS='-C link-arg=-Wl,-rpath,$ORIGIN -C link-arg=-Wl,--disable-new-dtags'
    # libavutil and libavcodec are linked against libva, so the linker has to
    # reach that copy while resolving their undefined symbols. It sits in its own
    # prefix, which is not on the default search path - without this the link
    # would succeed only where a distribution libva happens to be installed, and
    # fail in the container, which has none.
    RUSTFLAGS="$RUSTFLAGS -C link-arg=-L$LIBVA_PREFIX/lib"
    export RUSTFLAGS
    cargo build --release --locked -p xgview
}

# ---------------------------------------------------------------- stage
stage_package() {
    echo "==> Staging $PACKAGE_DIR"
    rm -rf "$PACKAGE_DIR"
    mkdir -p "$PACKAGE_DIR"

    cp "$CARGO_TARGET_DIR/release/xgview" "$PACKAGE_DIR/xgview"
    echo "    xgview"

    # Only the libraries of the tree just built, by name prefix, so no system
    # library (libX11, libGL, libvulkan, ...) can ride along. The `.so` and
    # `.so.<major>` entries are symlinks to the real file and are kept as such.
    local library file
    for library in libavcodec libavdevice libavfilter libavformat libavutil \
                   libswresample libswscale; do
        for file in "$PREFIX/lib/$library".so*; do
            [[ -e "$file" ]] || continue
            cp -a "$file" "$PACKAGE_DIR/"
            echo "    $(basename "$file")"
        done
    done

    # libva travels for the same reason the FFmpeg libraries do, only more so:
    # VAAPI is linked rather than opened at run time, so a package without it
    # would not start at all on a machine that never installed libva. This is
    # the copy `build_hwcodecs` built, not the distribution's.
    for library in libva libva-drm; do
        for file in "$LIBVA_PREFIX/lib/$library".so*; do
            [[ -e "$file" ]] || continue
            cp -a "$file" "$PACKAGE_DIR/"
            echo "    $(basename "$file")"
        done
    done

    # Language packs: every .ftl travels beside the executable, the same layout
    # the Windows package uses.
    if [[ -d "$REPO_ROOT/langs" ]]; then
        cp -a "$REPO_ROOT/langs" "$PACKAGE_DIR/langs"
        echo "    langs/"
    fi

    # The icons and the installer travel with the package: the icons are what a
    # desktop entry points at, and the installer is what puts both them and the
    # bundle where a desktop looks for them.
    if [[ -d "$REPO_ROOT/assets/icons/linux/hicolor" ]]; then
        mkdir -p "$PACKAGE_DIR/icons"
        cp -a "$REPO_ROOT/assets/icons/linux/hicolor" "$PACKAGE_DIR/icons/hicolor"
        echo "    icons/hicolor/"
    fi
    if [[ -f "$REPO_ROOT/scripts/install-linux.sh" ]]; then
        cp "$REPO_ROOT/scripts/install-linux.sh" "$PACKAGE_DIR/install.sh"
        chmod +x "$PACKAGE_DIR/install.sh"
        echo "    install.sh"
    fi
}

# ---------------------------------------------------------------- pack
pack_package() {
    echo "==> Packing $TARBALL"
    # The staging directory is the top level of the archive, so unpacking yields
    # one folder to run from rather than scattering files over the current one.
    tar -C "$OUT_ROOT" -czf "$TARBALL" "$PACKAGE_NAME"
}

# Highest GLIBC_x.y symbol version any of the given ELF files asks for. `-V`
# sorts the collected versions numerically, so 2.28 beats 2.2.5 and 2.17, and
# the last one is the floor the package imposes on a machine that loads it.
highest_glibc() {
    readelf --version-info "$@" 2>/dev/null \
        | grep -oE 'GLIBC_[0-9]+(\.[0-9]+)+' \
        | sed 's/^GLIBC_//' \
        | sort -V -u \
        | tail -n 1
}

# ---------------------------------------------------------------- verify
#
# Unpack what was just packed and check it from the outside in: that every
# FFmpeg library resolves to the copy in the unpacked folder rather than to
# anything installed, that the executable runs, that nothing outside the
# handful of system libraries and the graphics stack is left dangling, and
# which glibc floor the package carries.
verify_package() {
    echo "==> Verifying $TARBALL"
    local verify_dir
    verify_dir="$(mktemp -d)"
    # The path is embedded, not referenced: the trap runs after this function has
    # returned, when a `local` would no longer be in scope.
    trap "rm -rf '$verify_dir'" EXIT
    tar -xzf "$TARBALL" -C "$verify_dir"
    local unpacked="$verify_dir/$PACKAGE_NAME"
    local exe="$unpacked/xgview"

    local ldd_output
    ldd_output="$(ldd "$exe")"
    echo
    echo "--- ldd $PACKAGE_NAME/xgview ---"
    printf '%s\n' "$ldd_output"

    local status=0
    # Every library of ours the executable loads has to come from the copy
    # beside it and not from anything installed. Which of the seven FFmpeg
    # libraries actually appear is up to the linker: the ones nothing reaches
    # (libavfilter, libswresample on this build) are dropped, and that is fine -
    # they exist for the link, not for the program. libavcodec is the one that
    # must be there, or nothing decodes; libva is what VAAPI needs and is linked
    # rather than opened, so it has to resolve locally too.
    local foreign
    foreign="$(printf '%s\n' "$ldd_output" \
        | awk '$1 ~ /^lib(av[a-z]*|sw[a-z]*|va|va-drm)\.so/ { print }' \
        | grep -F -v "$unpacked/" || true)"
    if [[ -n "$foreign" ]]; then
        echo "FAIL: libraries that do not resolve into the unpacked folder:" >&2
        printf '%s\n' "$foreign" >&2
        status=1
    fi
    if ! printf '%s\n' "$ldd_output" | grep -q "^[[:space:]]*libavcodec\.so"; then
        echo "FAIL: libavcodec is not linked at all" >&2
        status=1
    fi
    if ! printf '%s\n' "$ldd_output" | grep -q "^[[:space:]]*libva\.so"; then
        echo "FAIL: libva is not linked: VAAPI hardware decoding would be absent" >&2
        status=1
    fi

    # Anything that is neither one of the libraries just packaged nor a library
    # every machine has (glibc, the compiler runtime) or reaches through the
    # graphics driver stack is a dependency that would have to travel with the
    # package too. The list is a glob per name so it reads as a list and not as a
    # regular expression.
    local allowed=(
        'ld-linux*' 'libc.so*' 'libm.so*' 'libdl.so*' 'librt.so*' 'libutil.so*'
        'libpthread.so*' 'libgcc_s.so*' 'libstdc++.so*'
        'libvulkan.so*' 'libwayland-*' 'libX11*' 'libxcb*' 'libxkbcommon*'
        'libGL*' 'libEGL*' 'libGLX*' 'libGLdispatch*' 'libdrm*' 'libgbm*'
        'libz.so*' 'libzstd*' 'libbz2*' 'liblzma*'
    )
    is_allowed() {
        local name="$1" pattern
        for pattern in "${allowed[@]}"; do
            # Unquoted on the right so the glob in the list is what matches.
            [[ "$name" == $pattern ]] && return 0
        done
        return 1
    }
    local unexpected="" line name path
    while read -r line; do
        if [[ "$line" =~ ^[[:space:]]*([^[:space:]]+)[[:space:]]*=\>[[:space:]]*(/[^[:space:]]*) ]]; then
            name="${BASH_REMATCH[1]}"
            path="${BASH_REMATCH[2]}"
            if [[ "$path" != "$unpacked"/* ]] && ! is_allowed "$name"; then
                unexpected+="    $name -> $path"$'\n'
            fi
        fi
    done <<< "$ldd_output"
    if [[ -n "$unexpected" ]]; then
        echo "note: libraries outside the package, and outside the known system set:" >&2
        printf '%s' "$unexpected" >&2
    fi

    # The C library floor: the highest versioned glibc symbol the executable and
    # every shared library in the package reference. Checking the library files
    # too matters, because a decoder can need a newer symbol than the executable
    # that calls it. Printed on every build so a release that quietly gained a
    # newer symbol is visible in the log rather than only on an old machine.
    echo
    echo "--- glibc baseline ---"
    local glibc_all
    glibc_all="$(highest_glibc "$exe" "$unpacked"/*.so*)"
    echo "    highest GLIBC symbol version in the package: GLIBC_${glibc_all}"
    echo

    echo "--- xgview --version ---"
    "$exe" --version

    echo
    local size_bytes
    size_bytes="$(stat -c '%s' "$TARBALL")"
    printf '==> %s (%.1f MB)\n' "$TARBALL" "$(awk -v b="$size_bytes" 'BEGIN { print b / 1048576 }')"

    if [[ "$status" -ne 0 ]]; then
        echo "==> verification FAILED" >&2
        exit 1
    fi
}

# ---------------------------------------------------------------- container
#
# `docker.exe` - Docker Desktop's Windows client, which a WSL shell may reach
# when the distro's own socket is not usable - wants Windows paths; a repository
# living under `/mnt/<drive>/` becomes `<DRIVE>:/...`. Any other client takes
# the path as it is.
docker_host_path() {
    local path="$1"
    if [[ "$DOCKER" == *.exe && "$path" =~ ^/mnt/([a-zA-Z])/(.*)$ ]]; then
        printf '%s:/%s\n' "${BASH_REMATCH[1]^^}" "${BASH_REMATCH[2]}"
    else
        printf '%s\n' "$path"
    fi
}

run_container_build() {
    # Prefer the client the environment names. Otherwise take `docker`, and when
    # that client cannot reach a daemon - the usual state under WSL, where Docker
    # Desktop's linux socket may not be wired into the distribution - fall back
    # to the Windows client, which talks to the same engine and mounts the
    # repository through its drive letters.
    if [[ -z "${XGVIEW_DOCKER:-}" ]] && ! "$DOCKER" info >/dev/null 2>&1 \
       && command -v docker.exe >/dev/null 2>&1 \
       && docker.exe info >/dev/null 2>&1; then
        echo "note: '$DOCKER' cannot reach a daemon; using docker.exe" >&2
        DOCKER=docker.exe
    fi
    if ! command -v "$DOCKER" >/dev/null 2>&1; then
        echo "error: '$DOCKER' not found; set XGVIEW_DOCKER to the docker client" >&2
        exit 1
    fi

    # A tag that names the base image, so switching --docker-image gets its own
    # image instead of reusing one built from somewhere else.
    local image="${XGVIEW_DOCKER_IMAGE_TAG:-}"
    if [[ -z "$image" ]]; then
        local base="${DOCKER_IMAGE##*/}" name tag
        name="${base%%:*}"; tag="${base#*:}"
        [[ "$tag" == "$name" ]] && tag="latest"
        image="xgview-linux-build:${name//linux/}${tag}"
    fi
    local volume="${XGVIEW_DOCKER_VOLUME:-xgview-linux-build}"

    if ! "$DOCKER" image inspect "$image" >/dev/null 2>&1; then
        echo "==> Building container image $image (FROM $DOCKER_IMAGE)"
        "$DOCKER" build \
            -t "$image" \
            --build-arg BASE_IMAGE="$DOCKER_IMAGE" \
            -f "$(docker_host_path "$REPO_ROOT/scripts/Dockerfile.linux-build")" \
            "$(docker_host_path "$REPO_ROOT/scripts")"
    else
        echo "==> Container image $image already exists (skipping build)"
    fi

    local container="xgview-linux-build-$$"
    # shellcheck disable=SC2064  # $DOCKER/$container are wanted at trap time
    trap "'$DOCKER' rm -f '$container' >/dev/null 2>&1 || true" EXIT
    "$DOCKER" rm -f "$container" >/dev/null 2>&1 || true

    # The out-of-tree module of `build.rs` is reached through `priv` at the
    # repository root, and on this machine that is a junction to a tree outside
    # the repository. Only the repository is mounted, so inside the container the
    # junction is a symlink to a path that is not there, the module is not found
    # and the package is built without it - quietly, the same way a public clone
    # builds without it. Mounting the tree it points at over the junction keeps
    # it reachable. A tree that lives within the repository needs nothing, and a
    # public clone has none.
    local priv_mount=()
    if [[ -e "$REPO_ROOT/priv" ]]; then
        local priv_target
        priv_target="$(readlink -f "$REPO_ROOT/priv")"
        if [[ "$priv_target" != "$REPO_ROOT"/* ]]; then
            echo "==> Mounting the out-of-tree module tree at $priv_target"
            priv_mount=(-v "$(docker_host_path "$priv_target"):/repo/priv:ro")
        fi
    fi

    echo "==> Building in container $container ($image), volume $volume"
    # The repository is read only: cargo writes to CARGO_TARGET_DIR under the
    # volume, never into the tree. CARGO_HOME is moved into the volume too, so
    # the crate cache outlives the container and a rerun does not refetch it.
    "$DOCKER" run --name "$container" \
        -v "$(docker_host_path "$REPO_ROOT"):/repo:ro" \
        "${priv_mount[@]}" \
        -v "$volume:/work" \
        -e XGVIEW_BUILD_IN_CONTAINER=1 \
        -e XGVIEW_OUT_ROOT=/work/out \
        -e CARGO_HOME=/work/cargo \
        -e FFMPEG_VERSION="$FFMPEG_VERSION" \
        -e LIBVA_VERSION="$LIBVA_VERSION" \
        -e NVCODEC_VERSION="$NVCODEC_VERSION" \
        -e XGVIEW_VA_DRIVER_DIRS="$VA_DRIVER_DIRS" \
        "$image" \
        bash /repo/scripts/build-linux.sh

    echo "==> Copying $PACKAGE_NAME.tar.gz out of the container"
    mkdir -p "$OUT_ROOT"
    "$DOCKER" cp "$container:/work/out/$PACKAGE_NAME.tar.gz" "$(docker_host_path "$TARBALL")"
    "$DOCKER" rm -f "$container" >/dev/null 2>&1 || true
    trap - EXIT
}

# ---------------------------------------------------------------- run
echo "==> XGView $VERSION, linux-$ARCH"

# Inside the container only the build half runs; the tarball is verified on the
# host afterwards, where a current glibc proves the floor rather than shares it.
if [[ -n "${XGVIEW_BUILD_IN_CONTAINER:-}" ]]; then
    echo "    (container build, output root $OUT_ROOT)"
    mkdir -p "$OUT_ROOT"
    build_hwcodecs
    build_ffmpeg
    # cargo decides whether a build script has to run again by comparing the
    # mtimes of what it watches with the time of the last build, and a tree
    # mounted in from elsewhere carries the mtimes of the filesystem it came
    # from - here older than the build it is compared against. A module that has
    # only just become visible therefore reads as unchanged, and the cached
    # answer of the build script, from a run that could not find it, is kept.
    # Dropping what was built of the crate whose script reads it makes the next
    # answer come from the script rather than from the cache.
    if [[ -e "$REPO_ROOT/priv" ]]; then
        echo "==> Re-reading the out-of-tree module"
        # The profile has to be named: without it the clean looks for the
        # artifacts of a `dev` build, finds none, and reports "Removed 0 files".
        cargo clean -p monitor_gui --release >/dev/null
    fi
    build_cargo
    stage_package
    pack_package
    exit 0
fi

if [[ "$DOCKER_MODE" -eq 1 ]]; then
    run_container_build
else
    mkdir -p "$OUT_ROOT"
    build_hwcodecs
    build_ffmpeg
    build_cargo
    stage_package
    pack_package
fi
verify_package
echo "==> Done"

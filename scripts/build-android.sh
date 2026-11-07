#!/usr/bin/env bash
#
# Cross compiles the XGView Android library with cargo-ndk.
#
# Prerequisites
# -------------
#   rustup target add aarch64-linux-android armv7-linux-androideabi
#   cargo install cargo-ndk
#   ANDROID_NDK_HOME (or ANDROID_NDK_ROOT) must point at an NDK r25+.
#
# This writes the shared libraries only - it is the library half of the build,
# for a machine where Gradle is driven by hand. `build-android.ps1` is the
# release path: it runs this per ABI and packages one APK each.
#
# Both ARM ABIs exist and the release is one package per ABI; this script builds
# the one it is asked for, at API level 27 - the `minSdk` the manifest declares -
# and writes it to android-build/jniLibs/<abi>/. A box whose chip is 64-bit but
# whose Android is 32-bit takes the `armeabi-v7a` one; see the README.
#
# Usage
# -----
#   scripts/build-android.sh                      # release, arm64-v8a, API 27
#   ABIS=armeabi-v7a scripts/build-android.sh     # the other ARM ABI
#   API=30 scripts/build-android.sh               # override the API level
#   PROFILE=debug scripts/build-android.sh        # debug build
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

ABIS="${ABIS:-arm64-v8a}"
API="${API:-27}"
PROFILE="${PROFILE:-release}"
OUT_DIR="${OUT_DIR:-android-build/jniLibs}"

# Map the cargo profile onto the cargo-ndk flag.
case "$PROFILE" in
    release) PROFILE_FLAG="--release" ;;
    debug)   PROFILE_FLAG="" ;;
    *)       PROFILE_FLAG="--profile $PROFILE" ;;
esac

if ! command -v cargo-ndk >/dev/null 2>&1; then
    echo "error: cargo-ndk is not installed (cargo install cargo-ndk)" >&2
    exit 1
fi

if [ -z "${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}" ]; then
    echo "warning: ANDROID_NDK_HOME / ANDROID_NDK_ROOT is not set, relying on cargo-ndk auto detection" >&2
fi

echo "==> building monitor_android (${PROFILE}) for ${ABIS} at API ${API}"
# A library an earlier build left here for another ABI is packaged into the APK
# too - the Android project takes the whole jniLibs tree - so the tree is emptied
# and then holds exactly the ABIs asked for. For a package per ABI, ask for one
# at a time; see the next steps below.
rm -rf "${OUT_DIR}"
# `-P` is cargo-ndk's own platform flag. The lowercase `-p` is passed through to
# cargo as `--package`, so it takes a crate name and not an API level.
# shellcheck disable=SC2086
cargo ndk \
    -t ${ABIS} \
    -P "${API}" \
    -o "${OUT_DIR}" \
    build ${PROFILE_FLAG} -p monitor_android

# OpenH264 is C++, and `cc` links Android's C++ runtime for it. The platform
# hands a program its system libraries but not a C++ runtime to a library of
# ours, so `libc++_shared.so` has to travel in the APK beside the one the
# Android project packages. It comes from the NDK, matched to the ABI.
#
# The static runtime is the other way to do this and the one that looks tidier -
# a single `.so`, nothing to copy - but Android keeps it in two archives, and
# linking `c++_static` without `c++abi` beside it leaves the library loading with
# its RTTI undefined: "cannot locate symbol
# _ZTVN10__cxxabiv117__class_type_infoE", on the device, at launch.
ndk_root="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}"
if [ -n "$ndk_root" ]; then
    ndk_lib_dir=""
    for dir in "$ndk_root"/toolchains/llvm/prebuilt/*/sysroot/usr/lib; do
        ndk_lib_dir="$dir"
        break
    done
    for abi in ${ABIS}; do
        case "$abi" in
            arm64-v8a)   triple=aarch64-linux-android ;;
            armeabi-v7a) triple=arm-linux-androideabi ;;
            x86)         triple=i686-linux-android ;;
            x86_64)      triple=x86_64-linux-android ;;
            *)           echo "error: unknown ABI: $abi" >&2; exit 2 ;;
        esac
        cp "${ndk_lib_dir}/${triple}/libc++_shared.so" "${OUT_DIR}/${abi}/libc++_shared.so"
        echo "    libc++_shared.so (${abi})"
    done
else
    echo "warning: no ANDROID_NDK_HOME, so libc++_shared.so was not packaged" >&2
fi

echo
echo "==> native libraries:"
find "${OUT_DIR}" -name 'libmonitor_android.so' -print

cat <<'EOF'

Next steps
----------
The Gradle project at android/ already reads this tree
(android-build/jniLibs), so:

    gradle -p android assembleRelease

packs whatever is here into one APK, so with one ABI in the tree that is one
package for it. The release is one package per ARM ABI, which is this script and
the Gradle step run once per ABI:

    ABIS=arm64-v8a   scripts/build-android.sh && gradle -p android assembleRelease
    ABIS=armeabi-v7a scripts/build-android.sh && gradle -p android assembleRelease

and copy target/xgview-<version>-android-<abi>.apk between the two. Asking for
both in one run (`ABIS='arm64-v8a armeabi-v7a'`) is a single APK carrying both,
which is not what the release ships.

Then deploy with scripts/deploy-android.sh.

Start on boot (Android 10+)
---------------------------
Android refuses to start an activity from the boot broadcast unless the app is
whitelisted, so a box installed by hand stays dark after a power cut until one
of these is set once on the device:

  * allow it to start over other apps - grant SYSTEM_ALERT_WINDOW, or
  * make XGView the home app (no permission, replaces the box's launcher).

The app's System tab opens the matching screen for each and shows whether it is
in effect. From a computer, the first is:

    adb shell appops set com.xhbl.xgview SYSTEM_ALERT_WINDOW allow
EOF

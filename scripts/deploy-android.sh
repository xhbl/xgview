#!/usr/bin/env bash
#
# One shot deployment of XGView to an Android TV box (or any device reachable
# over ADB), including the wireless (`adb connect <IP>:5555`) workflow.
#
# Prerequisites
# -------------
#   * The box has "USB debugging / ADB debugging" enabled.
#   * scripts/build-android.sh has produced the APK's native libraries.
#   * APK is built (this script installs an existing APK, it does not compile it).
#
# Usage
# -----
#   scripts/deploy-android.sh 192.168.10.42                 # APK=target/xgview.apk
#   ADB_PORT=5555 scripts/deploy-android.sh 192.168.10.42
#   APK=~/build/xgview.apk scripts/deploy-android.sh 192.168.10.42
#   scripts/deploy-android.sh                               # use the already connected device
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

DEVICE="${1:-}"
ADB_PORT="${ADB_PORT:-5555}"
APK="${APK:-target/xgview.apk}"
APP_ID="${APP_ID:-com.xhbl.xgview}"

if ! command -v adb >/dev/null 2>&1; then
    echo "error: adb is not on PATH (install the Android platform tools)" >&2
    exit 1
fi

# ---------------------------------------------------------------- connect
if [ -n "$DEVICE" ]; then
    # Accept either "192.168.10.42" or "192.168.10.42:5555".
    case "$DEVICE" in
        *:*) TARGET="$DEVICE" ;;
        *)   TARGET="$DEVICE:$ADB_PORT" ;;
    esac

    echo "==> adb connect $TARGET"
    adb connect "$TARGET" >/dev/null
    # Give the transport a moment to settle before talking to the device.
    adb -s "$TARGET" wait-for-device
    ADB_SERIAL="$TARGET"
else
    ADB_SERIAL="$(adb get-serialno)"
    if [ -z "$ADB_SERIAL" ] || [ "$ADB_SERIAL" = "unknown" ]; then
        echo "error: no device connected and no IP given (usage: $0 <ip>)" >&2
        exit 1
    fi
    echo "==> using connected device $ADB_SERIAL"
fi

# ---------------------------------------------------------------- install
if [ ! -f "$APK" ]; then
    echo "error: APK not found: $APK" >&2
    echo "       build it first, or pass APK=/path/to/xgview.apk" >&2
    exit 1
fi

echo "==> installing $APK"
# -r: replace an existing install, -g: grant all runtime permissions up front so
# the unattended box never shows a permission dialog.
adb -s "$ADB_SERIAL" install -r -g "$APK"

# ---------------------------------------------------------------- launch
echo "==> launching $APP_ID"
adb -s "$ADB_SERIAL" shell monkey \
    -p "$APP_ID" \
    -c android.intent.category.LAUNCHER \
    1 >/dev/null

echo
echo "==> logcat (Ctrl+C to stop):"
echo "    adb -s $ADB_SERIAL logcat -s XGView.BootReceiver xgview"
exec adb -s "$ADB_SERIAL" logcat -s XGView.BootReceiver xgview

#!/usr/bin/env bash
#
# Installs this unpacked XGView bundle - for the user who runs it, or for the
# whole machine with `--system`. `--uninstall` takes it back out again.
#
# The bundle is self contained: the executable finds the FFmpeg libraries it
# links against beside itself, through an `$ORIGIN` rpath. The folder therefore
# has to stay whole and cannot be split over `bin/` and `lib/` the way a
# distribution's package would, which is what `/opt` is for; `~/.local/opt` is
# the same shape for a single user.
#
# What an install does
# --------------------
#   * copies the bundle to the application directory, replacing an earlier
#     install of it there
#   * copies the icons into the hicolor theme
#   * writes a desktop entry, so XGView appears in the application grid
#   * links the executable onto `PATH`
#   * refreshes the desktop and icon caches, where those tools are installed
#
# An uninstall undoes exactly that - the application directory, the icons, the
# desktop entry and the link - and leaves the configuration in
# `~/.config/xgview` alone: it is the viewer's, not the installer's.
#
# Usage
# -----
#   ./install.sh                  # ~/.local, for the user who ran it
#   sudo ./install.sh --system    # /opt and /usr/share, for every user
#   ./install.sh --uninstall      # or `sudo ./install.sh --system --uninstall`
#
set -euo pipefail

ARGS=("$@")
SYSTEM=0
UNINSTALL=0

usage() {
    cat <<'USAGE'
Usage: install.sh [--system] [--uninstall]

  --system      install for every user: /opt/xgview and /usr/share
                (or remove that install, with --uninstall); needs root
  --uninstall   undo an install instead of performing one
  -h, --help    print this
USAGE
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --system)     SYSTEM=1 ;;
        --uninstall)  UNINSTALL=1 ;;
        -h|--help)    usage; exit 0 ;;
        *)            echo "error: unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ "$SYSTEM" == 1 ]]; then
    if [[ "$(id -u)" -ne 0 ]]; then
        echo "error: --system writes to /opt and /usr/share; run it with sudo" >&2
        exit 1
    fi
    APP_DIR="/opt/xgview"
    DATA_DIR="/usr/share"
    BIN_DIR="/usr/local/bin"
else
    APP_DIR="$HOME/.local/opt/xgview"
    DATA_DIR="$HOME/.local/share"
    BIN_DIR="$HOME/.local/bin"
fi
ENTRY="$DATA_DIR/applications/xgview.desktop"

# Runs after either half, and is only a nudge: a desktop notices both on its own.
refresh_caches() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$DATA_DIR/applications" >/dev/null 2>&1 || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -f -t "$DATA_DIR/icons/hicolor" >/dev/null 2>&1 || true
    fi
}

# ---------------------------------------------------------------- uninstall
if [[ "$UNINSTALL" == 1 ]]; then
    # Removing the application directory deletes this script while the shell is
    # still reading it - an uninstall run from inside the install, which is the
    # natural way to run it, would then die part way through. So that case
    # carries on from a copy of itself in the temporary directory instead.
    if [[ "$HERE" == "$APP_DIR" ]]; then
        self="$(mktemp)"
        cp "${BASH_SOURCE[0]}" "$self"
        chmod +x "$self"
        exec "$self" "${ARGS[@]}"
    fi

    echo "==> Removing $ENTRY"
    rm -f "$ENTRY"

    echo "==> Removing the icons from $DATA_DIR/icons/hicolor"
    # Only the files this program installs, not the theme around them.
    for dir in "$DATA_DIR"/icons/hicolor/*x*/apps; do
        [[ -d "$dir" ]] || continue
        rm -f "$dir/xgview.png"
    done

    # The link only if it is ours: a `xgview` put there by hand is left alone.
    if [[ -L "$BIN_DIR/xgview" && "$(readlink -f "$BIN_DIR/xgview" 2>/dev/null)" == "$APP_DIR/xgview" ]]; then
        echo "==> Removing $BIN_DIR/xgview"
        rm -f "$BIN_DIR/xgview"
    fi

    echo "==> Removing $APP_DIR"
    rm -rf "$APP_DIR"

    refresh_caches

    echo
    echo "==> Done"
    echo "    The configuration in ~/.config/xgview was left where it is."
    exit 0
fi

# ---------------------------------------------------------------- install
if [[ ! -x "$HERE/xgview" ]]; then
    echo "error: $HERE does not look like an XGView bundle (no ./xgview in it)" >&2
    exit 1
fi

# The bundle, copied as it is rather than taken apart file by file: the
# libraries have to travel with the executable, and the icons and this script
# are what the rest of the install asks for.
if [[ "$HERE" != "$APP_DIR" ]]; then
    echo "==> Installing the bundle into $APP_DIR"
    mkdir -p "$(dirname "$APP_DIR")"
    rm -rf "$APP_DIR"
    cp -a "$HERE" "$APP_DIR"
else
    echo "==> $APP_DIR is this bundle already"
fi
chmod +x "$APP_DIR/xgview"

# The desktop entry names the icon `xgview` and leaves the theme to resolve it,
# so the sizes go into hicolor the way any other application's do.
if [[ -d "$HERE/icons/hicolor" ]]; then
    echo "==> Installing the icons into $DATA_DIR/icons/hicolor"
    # The theme is already there on any desktop, and is not ours to replace:
    # `mkdir -p` and then the *contents* of our tree, so a re-install overwrites
    # our own sizes instead of nesting a second `hicolor` inside the first.
    mkdir -p "$DATA_DIR/icons/hicolor"
    cp -a "$HERE/icons/hicolor/." "$DATA_DIR/icons/hicolor/"
fi

echo "==> Writing $ENTRY"
mkdir -p "$DATA_DIR/applications"
cat > "$ENTRY" <<DESKTOP
[Desktop Entry]
Type=Application
Name=XGView
Comment=A grid viewer for surveillance cameras
Exec=$APP_DIR/xgview
Icon=xgview
Terminal=false
Categories=AudioVideo;Video;
StartupWMClass=xgview
DESKTOP
chmod 644 "$ENTRY"

# A symlink, not a wrapper script: the loader takes `$ORIGIN` from the real file
# it is running (`/proc/self/exe`), so the libraries beside the target are found
# through the link as well.
echo "==> Linking $BIN_DIR/xgview"
mkdir -p "$BIN_DIR"
ln -sf "$APP_DIR/xgview" "$BIN_DIR/xgview"

refresh_caches

echo
echo "==> Done"
echo "    application : $APP_DIR/xgview"
echo "    launcher    : $ENTRY"
echo "    on PATH     : $BIN_DIR/xgview"
echo
echo "    To start the wall at login, have XGView register itself:"
echo "        $APP_DIR/xgview --install-autostart"

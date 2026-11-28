#!/usr/bin/env bash
#
# debian-kiosk.sh
# One-shot setup on a "minimal install" of Debian 13: start xgview full screen
# at boot, with no desktop. When the program exits, the tty1 login prompt comes
# back.
#
# Steps:
#   1. From Windows, upload this script and the release package to the target
#      machine (Debian installed, network up, SSH enabled):
#        scp debian-kiosk.sh xgview-*-linux-x86_64.tar.gz user@machine-ip:/home/user/
#   2. Log in over SSH and, as the ordinary user (do not su - yet), unpack the
#      package and run its install.sh (installs into ~/.local/opt/xgview/); see
#      section 5 of docs/DEBIAN-KIOSK.md.
#   3. Switch to root and run this script:
#        su -
#        bash /home/user/debian-kiosk.sh [options]
#   4. reboot and check that it starts by itself.
#    Note: it must run as root (an ordinary user exits at once). If the script
#          was edited on Windows, first run sed -i 's/\r$//' debian-kiosk.sh to
#          strip the Windows line endings.
#
# Usage (as root):
#   bash debian-kiosk.sh [options]
#
# Options:
#   -u user        the user that runs the program (default: detected - the
#                  logged-in user, otherwise the first ordinary user)
#   -p path        program path; default /home/<user>/.local/opt/xgview/xgview
#   --xg package   path to the release tar.gz (named like
#                  xgview-1.4.70-linux-x86_64.tar.gz); the script unpacks it to a
#                  temporary directory and runs its install.sh as the user
#   --no-sudo      do not install sudo (by default it is installed and the user
#                  is added to the sudo group)
#   --no-supervise do not use the program's own supervisor (by default it runs
#                  with --supervise, which restarts the program alone on a crash)
#   --no-en        do not change the system language (by default it is set to
#                  English en_US.UTF-8, so the tty console does not garble CJK)
#   --vnc password write the VNC password (port 5900); x11vnc is always
#                  installed and started with the session, and without this
#                  option the password is left as it is (default "vnc" on the
#                  first install)
#   --start        start the service as soon as the setup is done (by default it
#                  is only enabled, and takes effect after a reboot)
#   --vm / --hw    treat the machine as a virtual machine / physical machine
#                  (detected automatically by default)
#   -h             show this help
#
# Notes:
#   * It is idempotent; steps already done do not fail.
#   * The program itself must be installed separately (the package's install.sh,
#     above); the script neither downloads nor builds it.
#   * On a physical Intel machine the iGPU VAAPI/QSV driver packages are
#     installed as well.

set -euo pipefail

# ---------- arguments ----------
USER_NAME=""
PROG=""
WANT_SUDO=1
SUPERVISE=1
START_NOW=0
ENV_MODE="auto"   # auto | vm | hw

VNC_PWD=""
XG_PKG=""
WANT_EN=1

usage() { awk 'NR>1 && /^#/ {sub(/^# ?/,""); print; next} NR>1 {exit}' "$0"; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
  case "$1" in
    -u) USER_NAME="${2:-}"; shift 2 ;;
    -p) PROG="${2:-}"; shift 2 ;;
    --xg)
      shift
      if [ $# -gt 0 ]; then XG_PKG="$1"; shift; fi
      [ -n "$XG_PKG" ] || { echo "--xg needs a package path"; usage 1; } ;;
    --sudo) WANT_SUDO=1; shift ;;
    --no-sudo) WANT_SUDO=0; shift ;;
    --no-supervise) SUPERVISE=0; shift ;;
    --en) WANT_EN=1; shift ;;
    --no-en) WANT_EN=0; shift ;;
    --vnc)
      shift
      if [ $# -gt 0 ] && [ "${1#-}" = "$1" ]; then VNC_PWD="$1"; shift; fi ;;
    --start) START_NOW=1; shift ;;
    --vm) ENV_MODE="vm"; shift ;;
    --hw) ENV_MODE="hw"; shift ;;
    -h|--help) usage 0 ;;
    *) echo "unknown option: $1"; usage 1 ;;
  esac
done

say()  { printf '\n\033[1;32m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33m[warn] %s\033[0m\n' "$*"; }
die()  { printf '\033[1;31m[error] %s\033[0m\n' "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "run as root (su - first)"
grep -qi '^ID=debian' /etc/os-release || warn "this does not look like Debian; the script is written for Debian 13 - check before continuing"

# ---------- user and paths ----------
# Detect the user: the sudo caller -> the logged-in user (before su -) -> the
# first ordinary user.
if [ -z "$USER_NAME" ]; then
  for cand in "${SUDO_USER:-}" "$(logname 2>/dev/null || true)"; do
    if [ -n "$cand" ] && [ "$cand" != "root" ] && id "$cand" >/dev/null 2>&1; then
      USER_NAME="$cand"; break
    fi
  done
fi
if [ -z "$USER_NAME" ]; then
  USER_NAME="$(awk -F: '$3>=1000 && $3<60000 {print $1; exit}' /etc/passwd)"
fi
[ -n "$USER_NAME" ] || die "no ordinary user found; pass one with -u"
id "$USER_NAME" >/dev/null 2>&1 || die "no such user: $USER_NAME"
HOME_DIR="$(getent passwd "$USER_NAME" | cut -d: -f6)"
[ -n "$PROG" ] || PROG="$HOME_DIR/.local/opt/xgview/xgview"

if [ -n "$XG_PKG" ] && [ ! -f "$XG_PKG" ]; then
  die "package not found: $XG_PKG"
fi

# ---------- environment detection ----------
IS_VM=0
if [ "$ENV_MODE" = "vm" ]; then IS_VM=1
elif [ "$ENV_MODE" = "auto" ] && command -v systemd-detect-virt >/dev/null 2>&1 \
     && systemd-detect-virt --quiet; then IS_VM=1; fi
IS_INTEL=0
grep -q GenuineIntel /proc/cpuinfo && IS_INTEL=1

say "Configuration"
echo "user: $USER_NAME    program: $PROG"
echo "environment: $([ $IS_VM -eq 1 ] && echo 'virtual machine' || echo 'physical machine')    Intel CPU: $([ $IS_INTEL -eq 1 ] && echo yes || echo no)"
echo "sudo: $([ $WANT_SUDO -eq 1 ] && echo install || echo skip)    --supervise: $([ $SUPERVISE -eq 1 ] && echo on || echo off)"
echo "VNC: installing x11vnc (port 5900)$([ -n "$VNC_PWD" ] && echo ', password updated' || echo ', password left as it is')"
echo "system language: $([ $WANT_EN -eq 1 ] && echo 'set to English en_US.UTF-8' || echo 'left as it is')"
[ -n "$XG_PKG" ] && echo "package: $XG_PKG (installed automatically)"

export DEBIAN_FRONTEND=noninteractive

# ---------- 1. apt sources: enable contrib / non-free / non-free-firmware ----------
# The Intel drivers (intel-media-va-driver-non-free) are in non-free, the
# firmware in non-free-firmware.
#
# `non-free` must be matched as a whole component: a naive `grep non-free`
# also hits `non-free-firmware`, which a minimal Debian 13 install already
# carries. On such a machine the naive test concluded non-free was enabled,
# skipped adding it, and the driver below silently failed to install - leaving
# the wall decoding on the CPU. Match a whitespace-delimited token instead, and
# add only the component that is actually missing - appending a fixed string
# would duplicate `non-free-firmware` on sources that already have it.
has_component() { grep -qE "(^|[[:space:]])${1}([[:space:]]|$)" <<<"$2"; }
# Echo a source line back with contrib / non-free / non-free-firmware appended
# only where missing. $1 is everything before the component list, $2 the list.
complete_components() {
  local head="$1" list="$2" component
  for component in contrib non-free non-free-firmware; do
    has_component "$component" "$list" || list="$list $component"
  done
  printf '%s %s\n' "$head" "$list"
}
enable_nonfree() {
  if [ -f /etc/apt/sources.list.d/debian.sources ]; then
    if ! grep -E '^Components:' /etc/apt/sources.list.d/debian.sources \
         | grep -qE '(^|[[:space:]])non-free([[:space:]]|$)'; then
      cp -n /etc/apt/sources.list.d/debian.sources /etc/apt/sources.list.d/debian.sources.bak
      local tmp="$(mktemp)"
      while IFS= read -r line; do
        case "$line" in
          Components:*) line="$(complete_components 'Components:' "${line#Components:}")" ;;
        esac
        printf '%s\n' "$line" >>"$tmp"
      done < /etc/apt/sources.list.d/debian.sources
      cat "$tmp" > /etc/apt/sources.list.d/debian.sources
      rm -f "$tmp"
    fi
  fi
  if [ -f /etc/apt/sources.list ] && grep -qE '^deb ' /etc/apt/sources.list; then
    if ! grep -E '^deb ' /etc/apt/sources.list \
         | grep -qE '(^|[[:space:]])non-free([[:space:]]|$)'; then
      cp -n /etc/apt/sources.list /etc/apt/sources.list.bak
      local tmp="$(mktemp)"
      while IFS= read -r line; do
        case "$line" in
          deb\ *)
            # deb <uri> <suite> <components...>; keep the first three words
            # and pass the rest as the component list.
            local deb uri suite rest
            read -r deb uri suite rest <<<"$line"
            line="$(complete_components "$deb $uri $suite" "$rest")"
            ;;
        esac
        printf '%s\n' "$line" >>"$tmp"
      done < /etc/apt/sources.list
      cat "$tmp" > /etc/apt/sources.list
      rm -f "$tmp"
    fi
  fi
}
if [ $IS_VM -eq 0 ]; then
  say "Enabling the non-free / non-free-firmware sources (needed by the hardware drivers)"
  enable_nonfree
fi

say "Updating the package lists"
apt-get update

# ---------- 2. basics: SSH, essential tools ----------
say "Installing the SSH server and basic tools"
apt-get install -y openssh-server ca-certificates curl

if [ $WANT_SUDO -eq 1 ]; then
  say "Installing sudo and adding the user"
  apt-get install -y sudo
  usermod -aG sudo "$USER_NAME"
fi

# ---------- 3. minimal graphical stack ----------
# xgview does not embed fonts; it reads the system fonts at runtime: DejaVu
# (interface and monospace) and Noto CJK (Chinese), so both must be installed.
say "Installing the minimal graphical stack (X server + openbox + graphics libraries + CJK font)"
apt-get install -y --no-install-recommends \
  xserver-xorg-core xserver-xorg-input-libinput xinit \
  openbox x11-xserver-utils \
  libgl1-mesa-dri libegl1 mesa-vulkan-drivers \
  libxkbcommon-x11-0 libxcursor1 libxi6 libxrandr2 \
  libasound2 \
  fonts-dejavu-core fonts-noto-cjk

if [ $IS_VM -eq 1 ]; then
  say "Virtual machine: installing open-vm-tools"
  apt-get install -y --no-install-recommends open-vm-tools || warn "open-vm-tools failed to install (can be ignored)"
elif [ $IS_INTEL -eq 1 ]; then
  say "Intel hardware: installing iGPU firmware and the VAAPI/QSV drivers"
  apt-get install -y firmware-misc-nonfree || warn "firmware-misc-nonfree failed to install; check that the non-free-firmware source is enabled"
  # The free `intel-media-va-driver` lives in main and covers the common N100 /
  # N5095 (Xe-LP) iGPUs, so it works even where the non-free source is absent.
  # Prefer it; fall back to the non-free build for hardware the free one misses.
  # Either way, fail loudly rather than leaving the wall decoding on the CPU.
  if apt-get install -y intel-media-va-driver vainfo intel-gpu-tools; then
    :
  elif apt-get install -y intel-media-va-driver-non-free vainfo intel-gpu-tools; then
    :
  else
    warn "the Intel VAAPI driver failed to install; hardware decoding will be unavailable - check the non-free source"
  fi
fi

# ---------- 3.5 VNC (x11vnc is always installed; --vnc writes the password) ----------
# x11vnc shares the same X display as the session (:0) - in effect, "share the
# xgview picture running on tty1".
say "Installing x11vnc"
apt-get install -y --no-install-recommends x11vnc
install -d -m 700 -o "$USER_NAME" -g "$USER_NAME" "$HOME_DIR/.vnc"
# With --vnc, write that password; otherwise use "vnc" when no password file
# exists yet (the first install).
if [ -n "$VNC_PWD" ]; then
  VNC_WRITE_PWD="$VNC_PWD"
elif [ ! -f "$HOME_DIR/.vnc/passwd" ]; then
  VNC_WRITE_PWD="vnc"
fi
if [ -n "${VNC_WRITE_PWD:-}" ]; then
  runuser -u "$USER_NAME" -- x11vnc -storepasswd "$VNC_WRITE_PWD" "$HOME_DIR/.vnc/passwd" </dev/null >/dev/null \
    || warn "could not write the VNC password; run x11vnc -storepasswd <password> $HOME_DIR/.vnc/passwd by hand"
  chown "$USER_NAME:$USER_NAME" "$HOME_DIR/.vnc/passwd"
  chmod 600 "$HOME_DIR/.vnc/passwd"
fi

# ---------- 4. program directory and session script ----------
say "Preparing the program directory and the session script"

# --xg: install the release package automatically. install.sh must run as the
# ordinary user (it installs into that user's ~/.local/opt/xgview).
if [ -n "$XG_PKG" ]; then
  # Stop a running service first, so an upgrade does not mix old and new files.
  if systemctl is-active --quiet monitor.service; then
    say "Stopping the running monitor service"
    systemctl stop monitor.service
    sleep 1
  fi
  say "Installing xgview from the package: $XG_PKG"
  XG_TMP="$(mktemp -d)"
  tar -xzf "$XG_PKG" -C "$XG_TMP"
  XG_INSTALL="$(find "$XG_TMP" -maxdepth 3 -name install.sh -print -quit)"
  [ -n "$XG_INSTALL" ] || die "install.sh not found in the package: $XG_PKG"
  XG_DIR="$(dirname "$XG_INSTALL")"
  chown -R "$USER_NAME:$USER_NAME" "$XG_TMP"
  runuser -u "$USER_NAME" -- bash -c "cd '$XG_DIR' && bash install.sh" </dev/null \
    || die "install.sh failed"
  rm -rf "$XG_TMP"
fi

if [ -f "$PROG" ]; then
  chmod +x "$PROG"
else
  warn "the program is not installed yet: $PROG"
  warn "unpack the package as the ordinary user ($USER_NAME, not root) and run its install.sh, or pass --xg with the package"
fi

# xset: keep the screensaver and the automatic blank timers off - a monitor
# wall must stay lit on its own. `+dpms dpms 0 0 0` keeps the DPMS extension
# enabled with every timeout at zero: the server will not blank by itself, but
# the state stays readable (`xset q` shows `Monitor is`) and the viewer can use
# DPMS for a blackout that deliberately switches the display off. `-dpms` would
# disable the extension entirely, which makes that impossible.
# To force every attached display on at its preferred resolution, add
# `xrandr --auto` before openbox.
PROG_ARGS=""
[ $SUPERVISE -eq 1 ] && PROG_ARGS=" --supervise"

# x11vnc: started in the background inside the session, sharing the current
# display; it ends with the session. The password file is checked at runtime:
# with it, x11vnc authenticates with -rfbauth; without it, it serves -nopw.
# \$DISPLAY and \$VNC_AUTH are expanded at runtime; $HOME_DIR is expanded here.
cat > /usr/local/bin/xgview-session.sh <<EOF
#!/bin/sh
xset s off s noblank +dpms dpms 0 0 0
# A blackout may switch the display off; if the viewer is killed or crashes,
# the DPMS state is left dark with nobody to restore it. The supervisor starts
# this script again on every restart, so light the screen here first. Harmless
# when the server has no DPMS extension (the xset error is swallowed).
xset dpms force on 2>/dev/null || true
openbox &
VNC_AUTH="-nopw"
[ -f "$HOME_DIR/.vnc/passwd" ] && VNC_AUTH="-rfbauth $HOME_DIR/.vnc/passwd"
x11vnc -display "\$DISPLAY" \$VNC_AUTH -rfbport 5900 -forever -shared -bg -o /tmp/x11vnc.log
exec $PROG$PROG_ARGS
EOF
chmod +x /usr/local/bin/xgview-session.sh

# The short command for running it by hand (on tty1, logged in as the user).
cat > /usr/local/bin/xg <<'EOF'
#!/bin/sh
if systemctl is-active --quiet monitor; then
  echo "monitor is running; stop it first with systemctl stop monitor (as root)"
  exit 1
fi
exec xinit /usr/local/bin/xgview-session.sh -- :0 vt1
EOF
chmod +x /usr/local/bin/xg

# The short command that restarts the kiosk. A plain `systemctl restart
# monitor` cancels itself: the stop's `ExecStopPost` starts `getty@tty1`, and
# the restart's start job then conflicts with it. Doing the stop and the start
# separately lets systemd settle that conflict for us - `monitor.service`
# declares `Conflicts=getty@tty1.service`, so starting it parks the getty.
# Never stop the getty by hand here: the command is usually typed on tty1
# itself, and stopping that getty hangs up the very shell running this script,
# cutting it off before the start. `systemctl stop` is also only a signal:
# xinit needs another beat to take its X server down and free vt1 / :0. If the
# new X starts in that gap the two fight for the console and both die, which
# on a kiosk looks like a blank screen with a blinking cursor and no service.
# Wait for the old server to be gone, bounded so a wedged one cannot hang the
# command, then start - systemd moves the getty aside on its own.
cat > /usr/local/bin/xgr <<'EOF'
#!/bin/sh
exec sudo sh -c '
  systemctl stop monitor
  i=0
  while [ "$i" -lt 50 ]; do
    if ! pgrep -x Xorg >/dev/null 2>&1 && [ ! -e /tmp/.X0-lock ]; then
      break
    fi
    i=$((i + 1))
    sleep 0.1
  done
  systemctl reset-failed monitor
  systemctl start monitor
'
EOF
chmod +x /usr/local/bin/xgr

# The short command for starting the service (needs sudo membership on the
# ordinary user; the installer writes it alongside xg and xgr).
cat > /usr/local/bin/xgs <<'EOF'
#!/bin/sh
exec sudo systemctl start monitor
EOF
chmod +x /usr/local/bin/xgs

# ---------- 5. systemd unit ----------
say "Writing the systemd unit monitor.service"
cat > /etc/systemd/system/monitor.service <<EOF
[Unit]
Description=XGView monitor
After=systemd-user-sessions.service plymouth-quit-wait.service getty@tty1.service
Conflicts=getty@tty1.service

[Service]
User=$USER_NAME
PAMName=login
TTYPath=/dev/tty1
TTYReset=yes
TTYVHangup=yes
StandardInput=tty
StandardOutput=journal
Environment=RUST_LOG=info
ExecStart=/usr/bin/xinit /usr/local/bin/xgview-session.sh -- :0 vt1 -keeptty -nolisten tcp
ExecStopPost=+/bin/systemctl --no-block start getty@tty1.service
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
EOF

for g in video input render; do
  getent group "$g" >/dev/null && usermod -aG "$g" "$USER_NAME"
done

systemctl daemon-reload
systemctl enable monitor.service
systemctl set-default multi-user.target
systemd-analyze verify /etc/systemd/system/monitor.service \
  && echo "unit file syntax check passed"

# ---------- 6. system language (English by default, so the tty console is readable) ----------
# The console uses a bitmap font without CJK glyphs, so Chinese system messages
# show as diamonds. This only affects system and shell messages; the XGView
# interface language comes from the program's own configuration.
if [ $WANT_EN -eq 1 ]; then
  say "Setting the system language to English (en_US.UTF-8)"
  if ! locale -a 2>/dev/null | grep -qiE '^en_US\.utf-?8$'; then
    localedef -i en_US -f UTF-8 en_US.UTF-8 || warn "could not generate en_US.UTF-8; check that the locales package is installed"
    if [ -f /etc/locale.gen ] && ! grep -q '^en_US.UTF-8 UTF-8' /etc/locale.gen; then
      sed -i 's/^# *en_US\.UTF-8 UTF-8/en_US.UTF-8 UTF-8/' /etc/locale.gen
    fi
  fi
  update-locale LANG=en_US.UTF-8 LANGUAGE=en \
    || warn "update-locale failed; run update-locale LANG=en_US.UTF-8 LANGUAGE=en by hand"
fi

# ---------- done ----------
# With --start, or when --xg has just installed the program, start the service.
if [ $START_NOW -eq 1 ] || [ -n "$XG_PKG" ]; then
  if [ -f "$PROG" ]; then
    say "Starting the service"
    systemctl start monitor.service
  else
    warn "the program is not there; the service was not started"
  fi
fi

say "All done"
cat <<EOF

Next:
  1. Install the program to: $PROG (run the package's install.sh as the ordinary
     user if it is not installed yet; --xg does this automatically)
  2. Reboot to check start-up: reboot
  3. Common commands:
       systemctl start|stop|status monitor
       xgs                          # start; needs sudo
       xgr                          # restart; handles the tty1 getty
       journalctl -u monitor -f
     On tty1, logged in as $USER_NAME, run it by hand: xg
EOF
[ $IS_VM -eq 0 ] && [ $IS_INTEL -eq 1 ] && echo "  4. Check hardware decoding: vainfo (should list H264/HEVC profiles)"
echo "  4. VNC: enabled on port 5900. Available only while the graphical session runs; it disconnects when the session ends."
[ $WANT_EN -eq 1 ] && echo "  5. Language: system messages are now English; log in again or reboot for it to take effect (the XGView interface language is set in the program, unaffected)"
[ $WANT_SUDO -eq 1 ] && echo "  Note: the sudo group takes effect after $USER_NAME logs in again"
true

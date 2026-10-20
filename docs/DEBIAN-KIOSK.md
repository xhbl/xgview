# Minimal GUI monitor deployment on Debian 13

**Goal**: Debian 13 with no desktop environment, where systemd starts the Rust
egui program `xgview` full screen at boot. When the program exits, the tty1
login prompt comes back. Frigate and Home Assistant then run in Docker on the
same machine (an Intel N100).

**Test environment**: the "runtime environment for a single GUI program" was
first verified in VMware Workstation Player, without testing hardware decoding;
the setup was then moved to the physical N100.

**Conventions used here**

- User: `admin` (the ordinary user created at install time)
- Program path: `/home/admin/.local/opt/xgview/xgview`
- A `#` before a command means it runs as root, a `$` means as the ordinary user
- Substitute your own user name or paths where they differ

## Quick start: the setup script

The sections below explain each step. To deploy quickly, the script
[`scripts/debian-kiosk.sh`](../scripts/debian-kiosk.sh) in this repository does
everything at once (upload it to the target machine first, and run it as
**root**):

> Appendix B carries the core steps inline if you would rather set it up by hand;
> the options table below shows what the script does on top.

**1. Upload the script and the release package** (from PowerShell on Windows;
the target machine needs Debian installed, a network, and SSH enabled)

```powershell
scp scripts/debian-kiosk.sh xgview-1.4.70-linux-x86_64.tar.gz admin@machine-ip:/home/admin/
```

(Use the version of the package you actually downloaded.)

**2. Install the program as the ordinary user** (after logging in over SSH,
**not as root** - `install.sh` installs into the current user's directory)

```bash
ssh admin@machine-ip
T=$(mktemp -d)
tar -xzf ~/xgview-*-linux-x86_64.tar.gz -C "$T"
cd "$T"/xgview-*-linux-x86_64 && bash install.sh
cd ~ && rm -rf "$T" ~/xgview-*-linux-x86_64.tar.gz
```

**Do not** run the `xgview --install-autostart` the installer suggests at the
end: start-up here is the job of the systemd unit (see 5.3 and 7.3).

**3. Switch to root and run the script**

```bash
su -
bash /home/admin/debian-kiosk.sh
```

The script prints the user, program path and environment (virtual machine or
physical) it detected; check them before letting it continue.

> VNC is built in: the script always installs x11vnc and starts it with the
> session (port 5900). The default password is `vnc`; override it with
> `--vnc your-password`, for example
> `bash /home/admin/debian-kiosk.sh --vnc 123456` (see section 13).

**4. Reboot and check**

`reboot`; the machine should come up straight into XGView.

**Options**

| Option | Effect |
|---|---|
| `--no-docker` | Do not install Docker (Docker CE is installed by default, for Frigate / Home Assistant; turn it off for a test VM) |
| `--no-sudo` | Do not install sudo (it is installed by default, and the user is added to the sudo group) |
| `--no-supervise` | Do not use the program's own supervisor (it starts with `--supervise` by default) |
| `--no-en` | Do not change the system language (by default it is set to `en_US.UTF-8`, so the tty console does not garble Chinese; the XGView interface language is unaffected) |
| `--vnc password` | Write the VNC password (port 5900); x11vnc is always installed and started with the session, and without this option the password is left as it is (default `vnc` on first install) (see section 13) |
| `--start` | Start the service as soon as the setup is done |
| `-u user`, `-p program-path` | Override the detected user or program path |
| `--xg package` | Point at an `xgview-*.tar.gz`; the script unpacks it and runs its `install.sh` as the ordinary user, stopping the running service first and starting it again afterwards (it checks that the file exists) |
| `--vm` / `--hw` | Treat the machine as a virtual machine / physical machine (detected automatically by default) |

**Notes**

- It must run as **root**; it is idempotent, and steps already done do not fail.
- The program itself is not installed by default: without options, install it
  from the release package by hand (step 2 and section 5); with
  `--xg /path/xgview-*.tar.gz` the script unpacks it and runs its `install.sh`.
- After being added to the sudo group, the user must **log in again** before
  sudo works.
- If the script was edited on Windows, run
  `sed -i 's/\r$//' debian-kiosk.sh` first, or the Windows line endings
  produce `\r` errors.
- With no network or no SSH: log in on the local console, `su -`, and
  `apt install openssh-server` (needs a network). A USB stick can carry the
  script, but its `apt` steps still need the network.

---

## Table of contents

0. [Quick start: the setup script](#quick-start-the-setup-script)
1. [Overall approach](#1-overall-approach)
2. [Installing Debian 13](#2-installing-debian-13)
3. [First login: SSH and privilege escalation](#3-first-login-ssh-and-privilege-escalation)
4. [The minimal graphical stack](#4-the-minimal-graphical-stack)
5. [Getting and installing the program (release package)](#5-getting-and-installing-the-program-release-package)
6. [Verifying the graphical stack by hand](#6-verifying-the-graphical-stack-by-hand)
7. [The session script and the systemd unit](#7-the-session-script-and-the-systemd-unit)
8. [Enabling and testing](#8-enabling-and-testing)
9. [Day-to-day operations and short commands](#9-day-to-day-operations-and-short-commands)
10. [Notes for testing in VMware](#10-notes-for-testing-in-vmware)
11. [Moving to the Intel N100](#11-moving-to-the-intel-n100)
12. [Docker: Frigate and Home Assistant](#12-docker-frigate-and-home-assistant)
13. [Remote viewing over VNC](#13-remote-viewing-over-vnc)
14. [Troubleshooting quick reference](#14-troubleshooting-quick-reference)
15. [Appendix: file list](#15-appendix-file-list)

---

## 1. Overall approach

With no desktop environment, the graphical stack has to be built by hand, just
enough to run one program:

```
boot
 └─ systemd starts monitor.service
     └─ xinit (starts the X server)
         └─ xgview-session.sh
             ├─ openbox &     (window manager, only for keyboard and mouse focus)
             └─ exec xgview   (the program; when it exits, the session ends)
```

Program exits → script ends → xinit ends → X closes → `ExecStopPost` starts
`getty@tty1` again, returning to the tty login prompt.

**Lessons learned (the traps met in testing)**

| Symptom | Cause | Fix |
|---|---|---|
| The interface appears but keyboard and mouse do nothing | Bare X has no window manager, so the window never takes input focus | Install and start `openbox` first |
| Chinese characters are all boxes | The minimal system has no Chinese font | Install `fonts-noto-cjk`, or embed a font into the program |
| Started over SSH, the picture appears but input misbehaves | An SSH session does not belong to the local seat0 | Start it from the tty1 console; use SSH only for logs and for killing processes |
| `systemctl start` reports `Access denied`, `pkttyagent` fails | The ordinary user has no permission | `su -` to root first |

---

## 2. Installing Debian 13

At the installer's software-selection step:

| Option | Choice |
|---|---|
| Debian desktop environment | ☐ off |
| GNOME / Xfce / KDE Plasma / Cinnamon / MATE / LXDE / LXQt / GNOME Flashback | ☐ all off |
| web server | ☐ off |
| **SSH server** | ☑ **on** (for remote maintenance, and for copy-pasting commands) |
| standard system utilities | ☑ keep |
| Choose a Debian Blend | ☐ off |

**Notes**

- If the installer sets a root password, `sudo` is not installed; use `su -` to
  escalate (see section 3).
- If the installer asks about enabling non-free firmware, **say yes** (the N100
  needs its iGPU firmware).
- Docker is installed later; nothing Docker-related has to be selected at
  install time.
- When planning disks, put the Frigate recordings on a separate large disk or
  partition if you will record.

---

## 3. First login: SSH and privilege escalation

### 3.1 Why SSH

VMware's text console does **not** support pasting, which makes long commands
tedious. Connecting over SSH from the host (Windows) gives you copy and paste.

Find the address in the VM console:

```bash
ip -4 a
```

Look for something like `192.168.x.x`, then, in PowerShell or Windows Terminal:

```powershell
ssh admin@vm-ip
```

> root is **not** allowed to log in over SSH by default. Log in as the ordinary
> user and escalate.

### 3.2 Escalating

When the installer set a root password (the case here):

```bash
su -
```

Enter **root's** password (not `admin`'s). The `-` matters: without it, `/usr/sbin`
is not on `PATH`. A prompt ending in `#` means you are root.

To use `sudo` later:

```bash
# apt install sudo
# usermod -aG sudo admin
```

Log out and back in for it to take effect.

### 3.3 Resetting a forgotten root password

1. Reboot and press `e` in the GRUB menu.
2. Append ` init=/bin/bash` to the end of the line starting with `linux`.
3. Press Ctrl+X to boot, then run:

```bash
mount -o remount,rw /
passwd root
exec /sbin/init
```

---

## 4. The minimal graphical stack

The commands below run as root (`su -` first, then paste).

```bash
apt install --no-install-recommends \
  xserver-xorg-core xserver-xorg-input-libinput xinit \
  openbox \
  libgl1-mesa-dri libegl1 mesa-vulkan-drivers \
  libxkbcommon-x11-0 libxcursor1 libxi6 libxrandr2 \
  fonts-dejavu-core fonts-noto-cjk x11-xserver-utils \
  open-vm-tools
```

| Package | Purpose |
|---|---|
| `xserver-xorg-core`, `xinit` | The X server and its launcher |
| `xserver-xorg-input-libinput` | Keyboard and mouse driver (without it there is no input) |
| `openbox` | A tiny window manager that **provides input focus** |
| `libgl1-mesa-dri`, `libegl1`, `mesa-vulkan-drivers` | OpenGL / Vulkan libraries (used by eframe/wgpu) |
| `libxkbcommon-x11-0`, `libxcursor1`, `libxi6`, `libxrandr2` | Libraries egui needs under X11 |
| `fonts-dejavu-core` | Interface and monospace fonts (DejaVu Sans / Sans Mono), read by XGView at runtime |
| `fonts-noto-cjk` | Chinese font (Noto Sans CJK); without it Chinese renders as boxes |
| `x11-xserver-utils` | Provides `xset`, used to disable blanking and power saving (a monitor wall must stay lit) |
| `open-vm-tools` | VMware guest tools (**virtual machines only**; skip on real hardware) |

Do not install `open-vm-tools-desktop`: it pulls in graphical dependencies.

> The release package carries FFmpeg and libva (see section 5), so the system
> ffmpeg is **not** needed; X11, OpenGL/Vulkan libraries and the graphics driver
> still come from the system, so the packages above cannot be skipped.

> **About fonts**: XGView **does not embed fonts**; it reads the system fonts at
> runtime, so they must be installed. From the program's log, it loads
> `/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf` (interface),
> `DejaVuSansMono.ttf` (monospace) and
> `/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc` (Chinese). Without
> the CJK font, Chinese shows as boxes. Scripts outside CJK (Arabic, Thai and so
> on) need their own font packages (for example `fonts-noto-core`).

---

## 5. Getting and installing the program (release package)

XGView ships as a Linux package named like
`xgview-1.4.70-linux-x86_64.tar.gz` (the version changes). Inside is a directory
of the same name holding:

| Content | Purpose |
|---|---|
| `xgview` | The program |
| `install.sh` | Installer: by default into the **current user's** `~/.local/opt/xgview/`; `--system` installs to `/opt/xgview` (needs root); `--uninstall` removes it |
| `icons/`, `langs/` | Icons and language packs |
| `libavcodec`, `libavformat`, `libavutil`, `libavfilter`, `libavdevice`, `libswscale`, `libswresample`, `libva`, `libva-drm`, … | **Bundled runtime libraries** (FFmpeg and the VA-API interface), independent of the system ffmpeg |

The package needs glibc 2.28 or newer; Debian 13 is far above that.

### 5.1 Uploading the package

From PowerShell on Windows (no need to enter the VM):

```powershell
scp xgview-1.4.70-linux-x86_64.tar.gz admin@machine-ip:/home/admin/
```

WinSCP or MobaXterm's file panel works too.

### 5.2 Unpacking and installing (as the ordinary user, not root)

`install.sh` installs into the **current user's** directory, so run it as the
user that will run the program (`admin`); as root it would install under
`/root`, where the service cannot find it.

After logging in over SSH (do not `su -` yet):

```bash
T=$(mktemp -d)                                   # temporary unpack directory
tar -xzf ~/xgview-*-linux-x86_64.tar.gz -C "$T"
cd "$T"/xgview-*-linux-x86_64 && bash install.sh
cd ~ && rm -rf "$T" ~/xgview-*-linux-x86_64.tar.gz   # remove temp dir and package
```

Both can be deleted when the install finishes. If several versions are in the
directory, the wildcard matches more than one; use the exact file name.

### 5.3 What install.sh does

Without `--system`, it:

- Copies the **whole directory** to `~/.local/opt/xgview/` (deleting an older
  installation of the same name first). The program finds its libraries beside
  itself through an `$ORIGIN` rpath, so the directory must stay intact.
- Copies the icons to `~/.local/share/icons/hicolor/`
- Writes a desktop entry at `~/.local/share/applications/xgview.desktop`
- Creates a symlink at `~/.local/bin/xgview`
- Refreshes caches when `update-desktop-database` or `gtk-update-icon-cache` are
  installed (their absence does no harm)

The desktop entry, icons and `PATH` link are unused on a machine without a
desktop, but they do no harm; the systemd unit starts the program by full path.

**Optional, and what not to do**

- `./install.sh --uninstall` removes all of the above and keeps
  `~/.config/xgview`.
- `sudo ./install.sh --system` installs to `/opt/xgview` for all users; the
  program path is then `/opt/xgview/xgview`, and the setup script needs
  `-p /opt/xgview/xgview`. A single-user kiosk is fine with the default.
- At the end it suggests `xgview --install-autostart` (start at desktop login).
  **Do not run it for a kiosk**: start-up is `monitor.service`'s job; a desktop
  autostart entry is not read in a minimal environment, and would start a second
  copy if a desktop is installed later. Keep the "start with the system" option
  in the program's **System** tab off for the same reason.

### 5.4 Verifying the installation

```bash
ls -l ~/.local/opt/xgview/xgview
```

It exists and is executable - that is enough. The configuration lives in
`~/.config/xgview/xgvconfig.json`; installing, upgrading and uninstalling leave
it alone.

Optionally, check for missing system libraries (the package's own libraries are
not reported; output means missing **system** libraries such as X11/GL):

```bash
ldd ~/.local/opt/xgview/xgview | grep "not found"
```

### 5.5 Upgrading

Install the new version the same way. `install.sh` deletes the old directory and
copies the new one; a running old process keeps running the old version without
an error. To avoid mixing old and new files, stop the service first and start it
afterwards:

```bash
sudo systemctl stop monitor
# then install the new version as in 5.2
sudo systemctl start monitor
```

### 5.6 Notes

- **Bundled libraries**: FFmpeg and friends need no installation; X11,
  OpenGL/Vulkan libraries and the graphics driver still come from the system, so
  the packages in section 4 cannot be skipped.
- **Hardware decoding**: the bundled `libva` is only the interface; the actual
  VA-API driver (on the N100, `intel-media-va-driver`) comes from the system.
  Check with `vainfo` first, then see whether the program's **About** page
  reports hardware decoding. If the system driver is fine but the program says
  otherwise, the bundled `libva` may not find the driver directory; try adding
  `Environment=LIBVA_DRIVERS_PATH=/usr/lib/x86_64-linux-gnu/dri` to the unit's
  `[Service]` section. That is inferred from how VA-API usually works and was
  not verified against this package.
- **Building from source as an alternative**: without the package, build on
  Debian itself (the environment matches the N100):
  ```bash
  # apt install build-essential pkg-config libxkbcommon-dev libwayland-dev \
      libx11-dev libxcursor-dev libxi-dev libxrandr-dev curl
  $ curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  $ cargo build --release
  ```
  A `.exe` built on Windows does not run on Debian.

---

## 6. Verifying the graphical stack by hand

**Verify by hand before writing the unit.** Log in as `admin` on the VMware
**console's tty1** (not over SSH):

```bash
$ xinit /home/admin/.local/opt/xgview/xgview -- :0 vt1
```

A picture means the display stack is fine. If keyboard and mouse do nothing, the
window manager is missing; use:

```bash
$ xinit /bin/sh -c 'openbox & exec /home/admin/.local/opt/xgview/xgview' -- :0 vt1
```

If input works now, that confirms the cause.

### 6.1 Getting out when the program does not respond

Connect over SSH:

```bash
$ pkill xinit
```

If that does not work, try `pkill -f xgview`, then `pkill Xorg`. Ctrl+Alt+F2 in
VMware switches to another tty (the host may intercept it).

### 6.2 The full check when input does not work

1. Was X started on the **tty1 console**, not over SSH?
2. Was `openbox` started? (the most common cause)
3. Has the VMware window captured input (click it, or press Ctrl+G)?
4. Is the libinput driver installed - `dpkg -l | grep xserver-xorg-input`?
5. Look at the Xorg log:
   ```bash
   $ grep -iE "libinput|\(EE\)|input device" ~/.local/share/xorg/Xorg.0.log | tail -30
   ```
6. Cross-check: `apt install x11-apps`, run
   `xinit /usr/bin/xeyes -- :0 vt1` on tty1; eyes that follow the mouse mean X
   input is fine.
7. If permission errors remain:
   ```bash
   # apt install xserver-xorg-legacy libpam-systemd dbus-user-session
   # usermod -aG input,video admin
   ```
   Put `allowed_users=anybody` and `needs_root_rights=yes` in
   `/etc/X11/Xwrapper.config`, then log in again.

---

## 7. The session script and the systemd unit

### 7.1 The session script

xinit runs one command only, so a script wraps openbox and the program. `exec`
makes the program the script's main process: when it exits, the script ends and
X closes with it.

```bash
# cat > /usr/local/bin/xgview-session.sh <<'EOF'
#!/bin/sh
xset s off -dpms s noblank
openbox &
exec /home/admin/.local/opt/xgview/xgview --supervise
EOF
# chmod +x /usr/local/bin/xgview-session.sh
# chmod +x /home/admin/.local/opt/xgview/xgview
```

`xset s off -dpms s noblank` disables the screen saver and display power saving.
Bare X blanks after about ten minutes by default; a monitor wall must not.

### 7.2 About full screen

The program supports full screen itself (**Full screen (F11)** and **Start in
full screen** in the settings), so openbox's `rc.xml` needs no changes; openbox
is here only to provide input focus.

### 7.3 The systemd unit

```bash
# cat > /etc/systemd/system/monitor.service <<'EOF'
[Unit]
Description=XGView monitor
After=systemd-user-sessions.service plymouth-quit-wait.service getty@tty1.service
Conflicts=getty@tty1.service

[Service]
User=admin
PAMName=login
TTYPath=/dev/tty1
TTYReset=yes
TTYVHangup=yes
StandardInput=tty
StandardOutput=journal
Environment=RUST_LOG=info
# To force the OpenGL backend (sometimes steadier in a virtual machine), uncomment:
# Environment=WGPU_BACKEND=gl
ExecStart=/usr/bin/xinit /usr/local/bin/xgview-session.sh -- :0 vt1 -keeptty -nolisten tcp
ExecStopPost=+/bin/systemctl --no-block start getty@tty1.service
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
EOF
```

**Each setting**

| Setting | Effect |
|---|---|
| `User=admin`, `PAMName=login` | Runs as admin through a login session, which is what grants local input and display access |
| `TTYPath=/dev/tty1`, `StandardInput=tty` | Binds to tty1 |
| `Conflicts=getty@tty1.service` | Frees tty1's login prompt when the service starts |
| `ExecStart` | Runs the session script under xinit, so the program starts at boot |
| `ExecStopPost` | Starts the tty1 login prompt again when the service ends, so "exit" returns to the tty |
| `Restart=on-failure` | Restarts three seconds after a crash or non-zero exit; a clean exit (0) is not restarted |

### 7.4 File dialogs (import/export): the built-in one, not the system's

On a desktop, import and export can raise the system file dialog; under systemd
on a minimal system the system dialog **does not come up, and can freeze the
interface**.

**Why** (from the logs and from testing):

1. The dialog library (rfd) first asks xdg-desktop-portal over D-Bus to raise a
   system dialog.
2. A process started by systemd has no user session bus
   (`DBUS_SESSION_BUS_ADDRESS`, `XDG_RUNTIME_DIR/bus`), so the portal call fails
   (`Portal operation not allowed` in the log).
3. It then falls back to `zenity`; without it that fails, and with it the dialog
   may still fail to start or be hidden under the full-screen window.
4. Called **synchronously** on the UI thread, the wait never ends and the whole
   wall freezes. That is why `pkill zenity` unfreezes it.

**The solution used here**: the program uses **`egui-file-dialog`**, which draws
the file browser inside the egui interface:

- No zenity, GTK, portal or D-Bus, and no window-stacking problems, so the
  deployment stays minimal;
- It behaves the same on Android and the other platforms;
- Verified in the VM (import and export dialogs work under systemd, Chinese
  labels included).

**So none of these are needed**: `zenity`, `xdg-desktop-portal`,
`xdg-desktop-portal-gtk`, `dbus-user-session`. If they were installed while
investigating, remove them:

```bash
# apt remove --purge zenity xdg-desktop-portal xdg-desktop-portal-gtk
# apt autoremove
```

The `/home/admin/.config/xdg-desktop-portal/portals.conf` created while
investigating can go too.

**Lessons for writing a program**:

- On a wall that runs for months, **never put a possibly blocking external call
  on the UI thread**. Even with threads or async, add timeouts so a dialog
  problem cannot stop the picture.
- On a minimal system, a USB stick used for import/export must be mounted by
  hand (`mount /dev/sdb1 /mnt`) or `udisks2` installed; to move a configuration
  between a NAS and the machine, `scp` is simpler.

### 7.5 The program's own supervisor, `--supervise`

XGView accepts `--supervise`: it runs as a supervisor, starts a copy of itself
without that switch, and starts it again if it dies. The session script already
uses it.

**How it shares the job with systemd** (two layers, both kept):

| Layer | Covers | Recovery |
|---|---|---|
| `--supervise` (in the program) | Restarts the program itself | Fast; X and openbox stay up, so the picture does not flicker |
| systemd `Restart=on-failure` | X dying, the supervisor dying, and so on - the whole session restarts | About 3 seconds; X starts again |

**Behaviour** (confirmed in the VM): a deliberate exit (code 0) is **not**
restarted; the session ends and the tty1 login prompt returns. Only an abnormal
exit is restarted.

**How to test**: two xgview processes run; `pgrep -a xgview` shows them.

- Kill only the one **without** `--supervise` (`kill -9 pid`): the supervisor
  should bring it back within seconds, without restarting X.
- `pkill -9 xgview` kills the supervisor too, which tests systemd's whole-session
  restart rather than the supervisor.
- After `systemctl stop monitor`, `pgrep -a xgview` should print nothing and
  tty1 should show its login prompt again.

To do without the supervisor, remove `--supervise` from the script (the setup
script's `--no-supervise` does the same).

---

## 8. Enabling and testing

### 8.1 Enabling (as root)

```bash
# usermod -aG video,input,render admin
# systemctl daemon-reload
# systemctl enable monitor.service
# systemctl set-default multi-user.target
# systemd-analyze verify /etc/systemd/system/monitor.service
```

- `enable` only sets it to start at boot; it does not start it now.
- `set-default multi-user.target` keeps the system from starting a graphical
  login manager.
- `verify` printing nothing means the unit is syntactically fine.

### 8.2 Starting

Stop the manual xinit first (`pkill xinit`), then, as **root**:

```bash
# systemctl start monitor.service
```

> As the ordinary user this reports `Failed to execute /usr/bin/pkttyagent` and
> `Access denied`; that is only a permission problem - `su -` and retry.

### 8.3 Checklist

| Case | Action | Expected |
|---|---|---|
| Start at boot | `reboot` | Straight into the program, no login prompt |
| Clean exit | Choose exit in the program | Back to the tty1 login prompt |
| Program crash, supervisor restarts | Over SSH, `kill -9` the process without `--supervise` | The program returns within seconds; X does not restart |
| Whole session crash, systemd restarts | Over SSH, `pkill -9 xgview` | The session returns after about 3 seconds |
| Manual stop | Over SSH (root), `systemctl stop monitor` | Back to the tty1 login prompt |
| Logs | `journalctl -u monitor -f` | Program output |

**Note**: `Restart=on-failure` restarts only on a non-zero exit or a crash. If a
"clean" exit comes straight back, the exit code is not 0; check
`systemctl status monitor`, then decide whether to set `Restart=no` or make the
program return 0.

---

## 9. Day-to-day operations and short commands

### 9.1 Common commands

```bash
systemctl start monitor      # start (as root)
systemctl stop monitor       # stop, back to the tty login (as root)
xgs                          # start; needs sudo
xgr                          # restart the program; see 9.5 (needs sudo)
systemctl status monitor     # status and exit code (as root)
journalctl -u monitor -n 50 --no-pager   # recent log (as root)
journalctl -u monitor -f                  # live log (as root)
```

A plain `systemctl restart monitor` is **not** reliable with this unit; see
9.5 for why and for `xgr`.

### 9.2 Running it again after an exit

On tty1, logged in as `admin`, either:

**Through the service (as at boot)**

```bash
$ su -
# systemctl start monitor
```

**By hand (for debugging; no root needed)**

```bash
$ xinit /usr/local/bin/xgview-session.sh -- :0 vt1
```

Do not do this while the service runs: two X servers would fight over vt1.

### 9.3 The short command `xg`

Create it as root:

```bash
# cat > /usr/local/bin/xg <<'EOF'
#!/bin/sh
if systemctl is-active --quiet monitor; then
  echo "monitor is running; stop it first with systemctl stop monitor (as root)"
  exit 1
fi
exec xinit /usr/local/bin/xgview-session.sh -- :0 vt1
EOF
# chmod +x /usr/local/bin/xg
```

Then, on tty1 as `admin`, `xg` runs it by hand with the log in the terminal. For
debugging: `RUST_LOG=debug xg`.

### 9.4 The short command `xgs` to start the service (needs sudo)

The installer writes `/usr/local/bin/xgs`; a hand-installed kiosk can add it
with the block below. It needs sudo membership (`apt install sudo`;
`usermod -aG sudo admin`, then log in again):

```bash
# cat > /usr/local/bin/xgs <<'EOF'
#!/bin/sh
exec sudo systemctl start monitor
EOF
# chmod +x /usr/local/bin/xgs
```

Then `xgs` starts the service.

### 9.5 The short command `xgr` to restart the kiosk (needs sudo)

`systemctl restart monitor` is not reliable with this unit. The stop half runs
the unit's `ExecStopPost`, which starts `getty@tty1`; the start half is then
queued against a tty1 that the getty already owns, and the `Conflicts`/`After`
relationship cancels it - systemd reports **"Job for monitor.service canceled"**
and the service is left failed, with tty1 at the login prompt.

`xgr` does the stop and start in the order that works:

```bash
# cat > /usr/local/bin/xgr <<'EOF'
#!/bin/sh
exec sudo sh -c 'systemctl stop monitor; systemctl stop getty@tty1; systemctl reset-failed monitor; systemctl start monitor'
EOF
# chmod +x /usr/local/bin/xgr
```

Then `xgr` restarts the wall. It needs the same sudo membership as `xgs`
(9.4). The `debian-kiosk.sh` installer writes it; a hand-installed kiosk can
add it with the block above.

---

## 10. Notes for testing in VMware

- Tick "Accelerate 3D graphics" and give it a larger video memory (256 MB or
  more); it helps GL rendering but is not required.
- VMware has no Intel iGPU: Vulkan can only run on software (lavapipe), while
  OpenGL can go through vmwgfx. If rendering misbehaves in the VM, try
  `WGPU_BACKEND=gl`.
- **Cannot be verified** in the VM: VAAPI/QSV hardware decoding, `vainfo`,
  `intel_gpu_top`, OpenVINO on the GPU, and real multi-stream performance.
- A black border at the bottom in full screen comes from the VM window and the
  guest resolution disagreeing; with `open-vm-tools` installed, resizing the VM
  window resizes the guest. A machine with a real display does not have this.
- Use a **bridged** network adapter, or the camera subnet is unreachable.
- **Take a snapshot** after the base system is installed and before the service
  is configured.
- Not using the cage (Wayland) approach: Xorg + openbox was the steadiest in the
  VM.

---

## 11. Moving to the Intel N100

The N100 (Alder Lake-N) iGPU supports VAAPI/QSV hardware decoding and OpenVINO
detection, and Vulkan through the anv driver.

### 11.1 Installing the system

As in section 2: desktop off, SSH server and standard utilities on, non-free
firmware enabled.

### 11.2 Host packages

```bash
# apt install --no-install-recommends \
  xserver-xorg-core xserver-xorg-input-libinput xinit openbox \
  libgl1-mesa-dri libegl1 mesa-vulkan-drivers \
  libxkbcommon-x11-0 libxcursor1 libxi6 libxrandr2 fonts-dejavu-core fonts-noto-cjk x11-xserver-utils

# apt install firmware-misc-nonfree intel-media-va-driver-non-free vainfo intel-gpu-tools
```

On real hardware, do **not** install `open-vm-tools`.

| Package | Purpose |
|---|---|
| `firmware-misc-nonfree` | GuC/HuC firmware for i915 (needs the non-free-firmware source) |
| `intel-media-va-driver-non-free` | The iHD driver QSV/VAAPI depends on |
| `vainfo` | Verifies hardware decoding (should list H.264/HEVC profiles) |
| `intel_gpu_top` (intel-gpu-tools) | Shows iGPU utilisation |

### 11.3 Copying the files and enabling

Copy these to the N100:

- The program: install the `xgview-*-linux-x86_64.tar.gz` package with
  `install.sh` as in section 5 (do not copy the binary alone)
- `/usr/local/bin/xgview-session.sh`
- `/etc/systemd/system/monitor.service`
- `/usr/local/bin/xg` (optional)
- `/usr/local/bin/xgs` (optional; the start helper, see 9.4)
- `/usr/local/bin/xgr` (optional; the restart helper, see 9.5)
- The configuration `~/.config/xgview/xgvconfig.json` (so the cameras need no
  setting up again)

Then run the enable commands from 8.1, adjusting the unit and script if the user
name differs.

### 11.4 Resource notes

The N100 has four cores, shared between the GUI and Frigate:

- Prefer **sub streams** in the wall, or use VAAPI hardware decoding
- Frigate records by stream copy (no transcoding) and detects on the sub stream
- With many cameras, watch `intel_gpu_top` for iGPU saturation
- The GUI and Frigate can share the same iGPU without conflict

---

## 12. Docker: Frigate and Home Assistant

### 12.1 Installing Docker (from the official repository)

Do not use Debian's own `docker.io`. Add the official apt repository as
described at docs.docker.com/engine/install/debian, then:

```bash
# apt install docker-ce docker-ce-cli containerd.io docker-compose-plugin
```

Docker needs no graphical stack and is entirely independent of the
Xorg/openbox setup above.

### 12.2 Frigate hardware acceleration (N100)

The key part of the compose file:

```yaml
services:
  frigate:
    image: ghcr.io/blakeblackshear/frigate:stable
    restart: unless-stopped
    shm_size: "256mb"
    devices:
      - /dev/dri/renderD128:/dev/dri/renderD128
    environment:
      - LIBVA_DRIVER_NAME=iHD
    volumes:
      - ./config:/config
      - /path/to/media:/media/frigate
```

In Frigate's `config.yml`:

```yaml
ffmpeg:
  hwaccel_args: preset-vaapi        # or preset-intel-qsv-h264 / preset-intel-qsv-h265

detectors:
  ov:
    type: openvino
    device: GPU
```

OpenVINO on the iGPU means the N100 needs no Coral. The OpenVINO detector also
needs its `model` section, as in Frigate's own documentation - follow the version
you run.

### 12.3 Home Assistant (container)

```yaml
services:
  homeassistant:
    image: ghcr.io/home-assistant/home-assistant:stable
    restart: unless-stopped
    network_mode: host
    volumes:
      - ./ha-config:/config
      - /etc/localtime:/etc/localtime:ro
```

- `network_mode: host` is what device discovery and mDNS rely on.
- The container edition has **no add-on store**; MQTT, Zigbee2MQTT and the rest
  run as their own compose services.

### 12.4 Storage planning

Frigate records a lot: put its recordings on a separate large disk or partition;
the system disk (`/var/lib/docker`) should only hold images and configuration.

---

## 13. Remote viewing over VNC

Use it when no monitor is attached, or to look in remotely: connect with VNC to
**the X session running on tty1 (`:0`)** and you see the real wall, not a new
virtual desktop. Useful for debugging and for a quick look.

It uses `x11vnc`: it attaches to the existing X display and shares it instead of
starting a desktop of its own. **The script always installs x11vnc and starts it
with the session**; `--vnc` only controls the password written.

### 13.1 Installed by the setup script (recommended)

The script always installs x11vnc and starts it with the session; `--vnc` only
controls the password:

```bash
su -
bash /home/admin/debian-kiosk.sh --vnc your-password
```

- The script installs `x11vnc`, writes the password to `~/.vnc/passwd` (owned by
  the user, mode 600), and starts x11vnc after `openbox &` in the session script
  (port 5900).
- **First install without `--vnc`**: the default password is `vnc`.
- **Reinstall without `--vnc`**: the existing password is kept.
- **With `--vnc new-password`**: it replaces the old one.

### 13.2 Installing by hand (to see how it works)

```bash
# apt install --no-install-recommends x11vnc
$ install -d -m 700 ~/.vnc
$ x11vnc -storepasswd your-password ~/.vnc/passwd
$ chmod 600 ~/.vnc/passwd
```

Then edit `/usr/local/bin/xgview-session.sh` and add, after `openbox &` and
before `exec xgview`:

```sh
x11vnc -display "$DISPLAY" -rfbauth /home/admin/.vnc/passwd -rfbport 5900 \
  -forever -shared -bg -o /tmp/x11vnc.log
```

- `-display "$DISPLAY"`: shares the current X session (`DISPLAY=:0` there).
- `-bg`: runs in the background, so `exec xgview` still happens.
- `-forever -shared`: allows reconnections and several viewers at once.

Apply it with `xgr` (or the stop/start sequence in 9.5; a plain `systemctl
restart monitor` can be canceled by the tty1 getty).

### 13.3 Connecting

Use any VNC client (RealVNC Viewer, TightVNC, Remmina and so on) against
`machine-ip:5900` with the password given to `--vnc`.

### 13.4 Notes and troubleshooting

- VNC exists only **while the graphical session runs**: the program exits → X
  ends → VNC disconnects. **Disconnecting a viewer does not affect the wall**
  (`-forever -shared`), so you can reconnect at any time.
- Port `5900` is open to the LAN; VNC authentication is weak, so **do not expose
  it to the internet** - keep it inside the network.
- If you cannot connect, check in order:
  - Is the session running (no X, nothing to share)? `systemctl status monitor`
  - Is x11vnc up? `pgrep -a x11vnc`; failures are in `/tmp/x11vnc.log`
  - Connects but the screen is black: make sure `:0` is shared (`pgrep -a Xorg`
    should show `:0 vt1`)
- When debugging with `xg`, it runs the same
  `/usr/local/bin/xgview-session.sh`, so a x11vnc line there starts VNC as well.

---

## 14. Troubleshooting quick reference

| Symptom | Likely cause | Fix |
|---|---|---|
| `systemctl start` reports `Access denied` / `pkttyagent` | Not root | `su -` and retry |
| `sudo: command not found` | Installer set a root password, so sudo was not installed | Use `su -`, or install sudo |
| Writing to `/etc/systemd/system/` says Permission denied | Not root | `su -` first |
| Picture appears but input does nothing | No window manager / X started over SSH | Start openbox; start from the tty1 console |
| Chinese shows as boxes | Missing Chinese font (XGView reads system fonts) | Install `fonts-noto-cjk` (the log should show NotoSansCJK loading) |
| Black screen after boot, no program | The service failed | `systemctl status monitor`, `journalctl -u monitor -n 50` |
| The program comes back by itself after exiting | Non-zero exit code | Check the code with `systemctl status monitor`; set `Restart=no` if needed |
| Manual `xg` conflicts with the service | Two X servers on vt1 | `systemctl stop monitor` first |
| Xorg cannot open tty/DRM - permission errors | Missing privileges or logind | Install `xserver-xorg-legacy libpam-systemd dbus-user-session`, configure `Xwrapper.config`, join the input/video groups |
| `ldd` shows `not found` | Missing system libraries (X11/GL, outside the package) | Install the matching packages |
| The import/export file dialog does not appear, or freezes the interface | No session bus under systemd, so the system dialog (portal/zenity) fails, and the UI thread waits synchronously | The program uses `egui-file-dialog` (see 7.4); as a stopgap, `pkill zenity` |
| The screen blanks after a while | X's default screen saver / power saving | Add `xset s off -dpms s noblank` to the session script, and install `x11-xserver-utils` |
| Chinese in the tty console shows as diamonds | The console's bitmap font has no Chinese (not related to `fonts-noto-cjk`) | The setup script sets the system language to English by default (`--no-en` turns that off); or `update-locale LANG=en_US.UTF-8 LANGUAGE=en` and log in again |
| Rendering misbehaves (especially in a VM) | Vulkan backend problem | Set `WGPU_BACKEND=gl` |
| VNC will not connect, or connects black | The graphical session is not running, or the wrong display is shared | `systemctl status monitor`, `pgrep -a x11vnc`, `/tmp/x11vnc.log`; make sure `:0` is shared (see 13.4) |

---

## 15. Appendix: file list

| Path | Purpose |
|---|---|
| `/home/admin/.local/opt/xgview/` | Program directory (installed by the package's `install.sh`; holds `xgview`, the bundled libraries, `icons/`, `langs/`) |
| `/home/admin/.local/bin/xgview` | Symlink created by `install.sh` (unused by the kiosk) |
| `/home/admin/.local/share/applications/xgview.desktop` | Desktop entry written by `install.sh` (unused by the kiosk) |
| `/home/admin/.config/xgview/xgvconfig.json` | Program configuration (cameras and so on) |
| `/usr/local/bin/xgview-session.sh` | Session script: disable blanking + openbox + xgview --supervise |
| `/etc/systemd/system/monitor.service` | The systemd unit |
| `/usr/local/bin/xg` | Short command to run it by hand (optional) |
| `/usr/local/bin/xgs` | Short command to start the service (needs sudo; written by the installer) |
| `/usr/local/bin/xgr` | Short command to restart the kiosk (needs sudo; see 9.5) |
| `/home/admin/.vnc/passwd` | VNC password file (written by the script, mode 600; default `vnc`, `--vnc` overrides) |
| `/tmp/x11vnc.log` | x11vnc's log |

### Appendix A (optional): embedding a CJK font in an egui program

```rust
let mut fonts = egui::FontDefinitions::default();
fonts.font_data.insert(
    "cjk".into(),
    egui::FontData::from_static(include_bytes!("../assets/NotoSansSC-Regular.otf")).into(),
);
fonts.families.get_mut(&egui::FontFamily::Proportional).unwrap().insert(0, "cjk".into());
fonts.families.get_mut(&egui::FontFamily::Monospace).unwrap().push("cjk".into());
cc.egui_ctx.set_fonts(fonts);
```

`FontData::from_static(...).into()` depends on the egui version; older ones do
not need `.into()`. XGView currently uses system fonts, so the font package must
be installed; embedding would remove that dependency and make
`fonts-noto-cjk` unnecessary on the N100.

### Appendix B: the whole setup inline (run as root)

> The full version is [`scripts/debian-kiosk.sh`](../scripts/debian-kiosk.sh)
> (automatic user detection, environment detection, Intel drivers, Docker, VNC
> and so on); see "Quick start" at the top. Below is the minimal inline version
> without those extras.

This folds sections 4, 7 and 8 into one, assuming the program was installed to
`/home/admin/.local/opt/xgview/xgview` as in section 5, and that this is a
**virtual machine** (on real hardware, drop `open-vm-tools` and add the Intel
driver packages from section 11):

```bash
apt install -y --no-install-recommends \
  xserver-xorg-core xserver-xorg-input-libinput xinit openbox \
  libgl1-mesa-dri libegl1 mesa-vulkan-drivers \
  libxkbcommon-x11-0 libxcursor1 libxi6 libxrandr2 \
  fonts-dejavu-core fonts-noto-cjk x11-xserver-utils open-vm-tools

cat > /usr/local/bin/xgview-session.sh <<'EOF'
#!/bin/sh
xset s off -dpms s noblank
openbox &
exec /home/admin/.local/opt/xgview/xgview --supervise
EOF
chmod +x /usr/local/bin/xgview-session.sh
chmod +x /home/admin/.local/opt/xgview/xgview

cat > /etc/systemd/system/monitor.service <<'EOF'
[Unit]
Description=XGView monitor
After=systemd-user-sessions.service plymouth-quit-wait.service getty@tty1.service
Conflicts=getty@tty1.service

[Service]
User=admin
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

cat > /usr/local/bin/xg <<'EOF'
#!/bin/sh
if systemctl is-active --quiet monitor; then
  echo "monitor is running; stop it first with systemctl stop monitor (as root)"
  exit 1
fi
exec xinit /usr/local/bin/xgview-session.sh -- :0 vt1
EOF
chmod +x /usr/local/bin/xg

usermod -aG video,input,render admin
systemctl daemon-reload
systemctl enable monitor.service
systemctl set-default multi-user.target
systemd-analyze verify /etc/systemd/system/monitor.service
```

# About xgview

**A grid viewer for surveillance cameras**

XGView is a production grade, cross platform surveillance wall written in Rust.
It targets TV boxes / living room displays (Android TV, e.g. Amlogic S905X5M or
Nvidia Shield TV) and Windows Mini PCs that run unattended around the clock; it
also runs on desktop Windows and Linux.

---

## Introduction

### Features

| Area | What it does |
| --- | --- |
| Grid layouts | 1x1, 2x2, 3x3 and 4x4, switchable at runtime |
| Pagination | Channels beyond the grid capacity are paged; swipe / PageUp / PageDown slides between pages |
| Focus navigation | Arrow keys or an Android TV DPAD move a 2 px cyan focus ring; the sideways arrows turn the page at the left and right edges, and the vertical ones stop at the top and bottom, where the wall hands the keys to the bars |
| Temporary zoom | Double click, `DPAD_CENTER` or `Enter` magnifies a viewport to 1x1; `BACK` / `Esc` restores the grid |
| Stream switching | Every layout pulls the cheap sub stream (360P / 480P), the 1x1 grid included; only a magnified viewport switches to the main stream (1080P / 4K) |
| Seamless transition | The last sub stream frame is held until the main stream delivers its first keyframe – no black or green frames |
| Suspend | Channels on a non visible page are suspended to save bandwidth, CPU and decoder handles |
| Reconnection | Exponential backoff self healing with a non blocking "reconnecting…" overlay; the backoff shape and the handshake timeout are configurable and reach channels already running |
| Decoding | Hardware where the machine offers a decoder for the stream (Direct3D 11 / CUDA on Windows, VAAPI / CUDA on Linux, `AMediaCodec` on Android), software elsewhere; each tile shows `HW` or `SW` |
| On-screen display | Each tile corner shows one of: none, name, number + name, stream, transport + rate, frame rate, size + shape, or all statistics. Chosen per corner in the settings panel |
| Power management | Keeps the machine awake and/or the screen on while the wall is up (`SetThreadExecutionState` on Windows, `caffeinate` on macOS, `systemd-inhibit` on Linux, `WakeLock` + `FLAG_KEEP_SCREEN_ON` on Android); both switches default on |
| Blackout | Scheduled periods, per weekday, in which the wall blanks itself and parks every channel - sockets closed, decoders dropped; input brings it back for a chosen number of minutes |
| Language | English, Chinese, or any language added as an external `.ftl` pack; a partial pack falls back to English key by key |
| Toolbar | On-screen bar for the layout, paging, full screen, settings and adding cameras; usable with a mouse, touch or a remote |
| Discovery | ONVIF WS-Discovery multicast probe, cross subnet unicast scan, TCP 554/80/8000 fallback probe, GetProfiles / GetStreamUri |
| NVR | Synology Surveillance Station import via `SYNO.API.Auth` + `SYNO.SurveillanceStation.Camera` |
| Cameras | Add, edit, reorder and remove cameras; per-camera display aspect and RTSP transport (UDP when a relay damages the TCP interleaving) |
| Start on boot | Windows `HKCU\...\Run` registry entry (or a scheduled task) and an Android `BOOT_COMPLETED` receiver |
| Configuration | Everything persisted to a cross platform `config.json`, with export / import |

---

## Usage

Setting up a machine, building and deploying are in
[docs/CONFIG.md](docs/CONFIG.md); what follows is what the viewer does, and how
it is driven.

### Command line

```
xgview [OPTIONS]

  --config <PATH>      Use an explicit configuration file
  --fullscreen         Start in full screen (TV / kiosk deployment)
  --windowed           Start in a window even if the configuration asks for full screen
  --autostart          Marker used by the start-on-boot registration
  --install-autostart  Register XGView for start-on-boot and exit
  --remove-autostart   Remove the start-on-boot registration and exit
  --print-schedule     Print the resolved grid / channel schedule and exit
  --console            Open a console window for log output (Windows only)
  -h, --help           Print this help
  -V, --version        Print the version
```

`--print-schedule` is useful to validate a configuration on a headless box: it
lists every page of every layout and shows which camera is decoded on which
stream.

### Keyboard and remote control

| Key | Action |
| --- | --- |
| Arrow keys / DPAD | Move the focus between viewports (turns the page at the edges) |
| `Enter` / `Space` / `DPAD_CENTER` | Magnify the focused viewport to 1x1, press again to return |
| `Esc` / `Backspace` / `BACK` | Leave the magnified view |
| `PageUp` / `PageDown` | Previous / next page |
| `1` `2` `3` `4` | Select the 1x1 / 2x2 / 3x3 / 4x4 layout |
| `F1` | Settings panel |
| `F2` | Camera discovery / management window |
| `F11` | Toggle full screen |

Mouse and touch dragging horizontally over the grid also turns the page. The
toolbar across the top offers the same layout, paging, full screen, settings and
add-camera actions as buttons, for a device without a keyboard.

### Settings panel

`F1` opens a tabbed panel, navigable with the arrows and `Enter` on a remote:

| Tab | What it holds |
| --- | --- |
| Display | Interface **language**, grid, the four OSD corners, full screen at start-up, the Android navigation bar |
| Cameras | The camera list: add, edit, reorder, remove; display aspect and RTSP transport per camera (the "Confirm order" step) |
| Streams | Reconnect timings (first retry, first-frame retry, max delay, handshake timeout, backoff factor / jitter / spread, attempts) and the hardware-decoding preference |
| Blackout | The blank schedule: on/off, the periods (weekday, start, end), and how long the wall stays back after input |
| System | Start on boot, **keep awake** (prevent sleep / keep the screen on) with the mechanism in use, the key list, and the Android boot helpers |
| About | Version, author, decoder in use, config path, export / import |

### Language

The interface is English by default; Chinese ships as `langs/zh-CN.ftl`. Any
other language is a plain `.ftl` file: put it beside the executable (`langs/`) or
under the configuration directory (`langs/`) and it appears in the **Language**
selector in the Display tab. A pack may translate only part of the interface -
anything it leaves out falls back to English. `Automatic` follows the system
locale.

### Keep awake

The **System** tab can hold the machine awake and keep the screen on while the
wall is up, so a display that would blank or a system that would sleep does not
cut the streams. Both switches are on by default; keeping the screen on also
keeps the system awake. The mechanism per platform is `SetThreadExecutionState`
(Windows), `caffeinate` (macOS), `systemd-inhibit` (Linux) and `WakeLock` +
`FLAG_KEEP_SCREEN_ON` (Android), and is shown in the panel. The request is
released when the viewer exits; see
[docs/power-management-plan.md](docs/power-management-plan.md).

### Adding cameras

Press `F2` to open the discovery window:

1. **ONVIF** – broadcasts a WS-Discovery probe, optionally scans one or more IP
   ranges with unicast probes, and falls back to a TCP 554/80/8000 port scan for
   cameras that stay silent. `GetProfiles` / `GetStreamUri` extract the main and
   sub stream RTSP URLs.
2. **Manual** – paste a main stream RTSP URL and let the built in regular
   expressions derive the sub stream, or fill both in by hand.
3. **Synology** – sign in to a Surveillance Station host and import every camera
   bound to the NAS in one go.

### Start on boot

**Windows.** Toggle "Start with the system" in the settings panel (`F1`), or run

```powershell
xgview.exe --install-autostart
```

which writes `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`. Remove it with
`xgview.exe --remove-autostart`. For a machine that must start without an
interactive logon, install a scheduled task instead (see
[docs/CONFIG.md](docs/CONFIG.md)) or register the binary as a Windows service.

**Android.** The manifest registers a `BootReceiver` for `BOOT_COMPLETED`, which
relaunches the activity after the box booted. Android 10+ refuses to start an
activity from the background, and that broadcast is one, so the relaunch is
aborted - silently, from the viewer's side - unless the app is whitelisted.
Either of these, a one-time step on each device, is enough:

* **Start over other apps** - grant the *Display over other apps* special
  permission (`SYSTEM_ALERT_WINDOW`, declared in the manifest). The settings
  panel's **System** tab has a button that opens the screen, and shows whether
  it is granted.
* **Home app** - the manifest also offers XGView as a home app; the **System**
  tab has a button that opens the chooser. No permission is involved, at the
  cost of replacing the box's own launcher.

From a computer, the first is one command:

```bash
adb shell appops set com.xhbl.xgview SYSTEM_ALERT_WINDOW allow
```

### Deployment

Packaging the Windows Mini PC install, building and signing the Android APK,
and what to carry by hand to a fresh clone are all in
[docs/CONFIG.md](docs/CONFIG.md).

---

## License
This project is licensed under [MIT License](../LICENSE). All code in this repository are free to use, modify, and distribute under the terms of this license.

---

## Contact
**E-mail**: [Send Email](mailto:newxhbl@hotmail.com?subject=[RustApps]%20Inquiry)  
**Issues**: [Open Issue](../../../issues)  

# About xgview

**A grid viewer for surveillance cameras**

XGView is a production grade, cross platform surveillance wall written in Rust.
It targets TV boxes / living room displays (Android TV, e.g. Amlogic S905X5M or
Nvidia Shield TV) and Windows Mini PCs that run unattended around the clock, and
also builds natively on desktop Windows / Linux for development.

---

## Introduction

### Features

| Area | What it does |
| --- | --- |
| Grid layouts | 1x1, 2x2, 3x3 and 4x4, switchable at runtime |
| Pagination | Channels beyond the grid capacity are paged; swipe / PageUp / PageDown slides between pages |
| Focus navigation | Arrow keys or an Android TV DPAD move a 2 px cyan focus ring; leaving an edge automatically turns the page |
| Temporary zoom | Double click, `DPAD_CENTER` or `Enter` magnifies a viewport to 1x1; `BACK` / `Esc` restores the grid |
| Stream switching | Multi grid pulls the cheap sub stream (360P / 480P), a magnified viewport switches to the main stream (1080P / 4K) |
| Seamless transition | The last sub stream frame is held until the main stream delivers its first keyframe – no black or green frames |
| Suspend | Channels on a non visible page are suspended to save bandwidth, CPU and decoder handles |
| Reconnection | Exponential backoff self healing with a non blocking "reconnecting…" overlay |
| Discovery | ONVIF WS-Discovery multicast probe, cross subnet unicast scan, TCP 554/80/8000 fallback probe, GetProfiles / GetStreamUri |
| NVR | Synology Surveillance Station import via `SYNO.API.Auth` + `SYNO.SurveillanceStation.Camera` |
| Start on boot | Windows `HKCU\...\Run` registry entry (or a scheduled task) and an Android `BOOT_COMPLETED` receiver |
| Configuration | Everything persisted to a cross platform `config.json` |

### Workspace layout

```
xgview/
├── Cargo.toml                  # workspace root + the `xgview` binary
├── crates/
│   ├── monitor_core/           # platform independent logic
│   │   ├── model.rs            #   camera / stream data model
│   │   ├── layout.rs           #   1x1 … 4x4 grids and pagination math
│   │   ├── scheduler.rs        #   which channel decodes which stream on which page
│   │   ├── config.rs           #   JSON persistence
│   │   ├── discovery/          #   ONVIF (WS-Discovery / SOAP), TCP scan, Synology
│   │   ├── rtsp.rs             #   async RTSP client (TCP interleaved)
│   │   ├── pipeline.rs         #   per channel supervisor + backoff reconnection
│   │   └── autostart.rs        #   start on boot helpers
│   ├── monitor_codec/          # platform decoder: Android AMediaCodec, Windows FFmpeg, null fallback
│   ├── monitor_gui/            # egui + wgpu UI, grid, slide animation, dialogs
│   └── monitor_android/        # NativeActivity entry point + Android manifest sample
└── scripts/                    # Windows install and cargo-ndk / adb deployment helpers
```

### Architecture principles

* **Non blocking UI** – the egui thread never touches a socket or a decoder. All
  network I/O and decoding run on a tokio runtime; results reach the UI through
  lock free channels (`crossbeam-channel` upward, `tokio::sync::mpsc` downward).
* **Hardware decoding on every target** – Android decodes with `AMediaCodec` and
  reads the pictures back through an `AImageReader`, so they reach the renderer
  as the same NV12 planes the Windows/Linux backends produce.
* **Conditional compilation** – `#[cfg(target_os = "android")]` /
  `#[cfg(target_os = "windows")]` select the decoder and the start on boot
  mechanism. A plain `cargo run` on Windows is enough to get going.

---

## Usage

### Requirements

* Rust 1.80 or newer (Edition 2021). Install it with [rustup](https://rustup.rs).
* Windows / Linux: no extra system dependency for the default build.
* Android: the Android NDK (r25+), the `aarch64-linux-android` target and
  `cargo-ndk`.

### Build and run (Windows / Linux)

```bash
cargo run                 # windowed
cargo run -- --fullscreen # kiosk / TV deployment
```

The first launch creates a configuration file next to the user profile:

| Platform | Path |
| --- | --- |
| Windows | `%APPDATA%\xgview\config.json` |
| Linux | `$XDG_CONFIG_HOME/xgview/config.json` (or `~/.config/xgview/config.json`) |
| Android | the app private files directory (set through `XGVIEW_HOME`) |

Set `XGVIEW_CONFIG_DIR` to override the location entirely (portable / USB stick
deployments).

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
| `F1` | Settings panel (layout, streams, start on boot) |
| `F2` | Camera discovery / management window |
| `F11` | Toggle full screen |

Mouse and touch dragging horizontally over the grid also turns the page.

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
interactive logon, install a scheduled task instead (see the install script
below) or register the binary as a Windows service.

**Android.** The manifest registers a `BootReceiver` for `BOOT_COMPLETED`, which
relaunches the activity after the box booted. On Android 10+ a background
activity start is restricted, so whitelist XGView in the OEM auto-start settings
or make it the default launcher.

### Windows Mini PC deployment

```powershell
powershell -ExecutionPolicy Bypass -File scripts\install-windows.ps1
```

The script builds the release binary, copies it to `%LOCALAPPDATA%\XGView`, and
registers the start on boot entry. Useful switches:

```powershell
# Register a scheduled task with restart-on-failure instead of the Run key.
powershell -ExecutionPolicy Bypass -File scripts\install-windows.ps1 -TaskScheduler

# Package an existing build, do not touch the start on boot entry.
powershell -ExecutionPolicy Bypass -File scripts\install-windows.ps1 -NoBuild -NoAutostart
```

### Android build and deployment

```bash
rustup target add aarch64-linux-android
cargo install cargo-ndk

# Build the native library (arm64-v8a, API 28 by default).
scripts/build-android.sh

# Or override the targets / API level.
ABIS="arm64-v8a armeabi-v7a" API=30 scripts/build-android.sh
```

The generated `android-build/jniLibs/<abi>/libmonitor_android.so` goes into the
`app/src/main/jniLibs/` folder of the Android Gradle project, together with
[crates/monitor_android/android/AndroidManifest.xml](crates/monitor_android/android/AndroidManifest.xml),
its `java/` sources and its `res/` folder. The manifest is a ready to use sample:
it declares `INTERNET`, `RECEIVE_BOOT_COMPLETED` and the NativeActivity whose
`android.app.lib_name` matches the `monitor_android` crate.

Once the APK exists, deploy it over the network (wireless debugging):

```bash
scripts/deploy-android.sh 192.168.10.42
```

The script runs `adb connect <ip>:5555`, installs the APK with all runtime
permissions granted (`adb install -r -g`) so the unattended box never shows a
dialog, launches the app and follows its logcat output.

---

## License
This project is licensed under [MIT License](../LICENSE). All code in this repository are free to use, modify, and distribute under the terms of this license.

---

## Contact
**E-mail**: [Send Email](mailto:newxhbl@hotmail.com?subject=[RustApps]%20Inquiry)  
**Issues**: [Open Issue](../../../issues)  

# XGView build and configuration

How to set a machine up to build the viewer, build and deploy it for each
target, where its configuration lives, and what has to be carried by hand when
the repository is cloned somewhere new. It closes with the repository layout and
the principles the code is built around.

For what the viewer does, see [../README.md](../README.md). For the findings
behind the decoder and the cameras, see [DevMemo.md](DevMemo.md).

---

## 1. Requirements

| Target | Needs |
|---|---|
| Windows / Linux desktop (default build) | Rust 1.80+ (Edition 2021, via [rustup](https://rustup.rs)); the FFmpeg **development** libraries, found through `VCPKG_ROOT` (`vcpkg install ffmpeg:x64-windows`); **libclang** for `bindgen` (`winget install LLVM.LLVM`, then set `LIBCLANG_PATH`) |
| Windows / Linux desktop (runtime) | the matching FFmpeg libraries beside the executable or on `PATH` |
| Android | the Android NDK (r25+), the `aarch64-linux-android` target and `cargo-ndk`; a JDK, Gradle and the Android SDK for the APK |

Neither FFmpeg nor OpenH264 is used on Android - decoding is `AMediaCodec`.

The FFmpeg build details (why `VCPKG_ROOT` is enough, why `libclang` is needed,
which DLLs to copy) are in [DevMemo.md](DevMemo.md) §4.

## 2. Configuration file

The viewer persists everything to one JSON file, created on first launch:

| Platform | Path |
| --- | --- |
| Windows | `%APPDATA%\xgview\config.json` |
| Linux | `$XDG_CONFIG_HOME/xgview/config.json` (or `~/.config/xgview/config.json`) |
| Android | the app private files directory (the entry point sets `XGVIEW_HOME` to it) |

- `XGVIEW_CONFIG_DIR` overrides the directory entirely (portable / USB stick
  deployments).
- `--config <PATH>` points the viewer at a different file for one run.
- The About tab can **export** and **import** the configuration; "Only import
  the cameras" keeps the viewer's own settings and takes the camera list from
  the file.
- Adding a field is backwards compatible: `AppConfig` is `#[serde(default)]`, so
  an older `config.json` simply takes the default for anything it lacks. The
  same file is shared by every platform.

## 3. Build and run (Windows / Linux)

```bash
cargo run                     # windowed
cargo run -- --fullscreen     # kiosk / TV deployment
cargo build --release         # optimized build in target/release/
```

The desktop decoder links FFmpeg dynamically, so at run time the `av*` / `sw*`
DLLs (Windows) or `.so` files (Linux) have to be resolvable - beside the
executable, or on `PATH` as vcpkg leaves them on the build machine. On Windows
`scripts\build-windows.ps1` gathers them from `VCPKG_ROOT` / `FFMPEG_DIR` into
the staged package, which is what makes a build copyable to another machine.

## 4. Windows Mini PC deployment

```powershell
powershell -ExecutionPolicy Bypass -File scripts\install-windows.ps1
```

The script builds the release binary, copies it to `%LOCALAPPDATA%\XGView`
together with the FFmpeg runtime DLLs and the `langs\` language packs, and
registers the start on boot entry. Useful switches:

```powershell
# Register a scheduled task with restart-on-failure instead of the Run key.
powershell -ExecutionPolicy Bypass -File scripts\install-windows.ps1 -TaskScheduler

# Package an existing build, do not touch the start on boot entry.
powershell -ExecutionPolicy Bypass -File scripts\install-windows.ps1 -NoBuild -NoAutostart
```

`scripts\build-windows.ps1` produces the same layout as a zip under `target/`
without installing it.

## 5. Android build and deployment

### Windows: one script, library and APK

```powershell
powershell -ExecutionPolicy Bypass -File scripts\build-android.ps1
# -> target\xgview-<version>-arm64-v8a.apk

# The debug variant, another ABI and API level.
powershell -ExecutionPolicy Bypass -File scripts\build-android.ps1 -DebugBuild -Abis armeabi-v7a -Api 30
```

The toolchain (NDK, JDK, Gradle, SDK) is looked up under `XGVIEW_ANDROID_ROOT`,
or `C:\Android` by default.

**The device** needs Android 8.1 (API 27) or newer, and a GPU the renderer can
use. The viewer draws through wgpu, which wants **Vulkan 1.1** or
**OpenGL ES 3.2 / `GL_KHR_debug`**; a box with only OpenGL ES 2.0 - an Amlogic
S905 / Mali-450 on its stock Android 9, say - has no adapter, and the app closes
a moment after launch with `No suitable graphics adapter found` in its logcat
(`adb logcat -s xgview`). A device whose Vulkan driver is older than 1.1
(Adreno on Android 8.x, Vulkan 1.0) is sent to the GL backend before the renderer
is built - the activity reads the version and leaves a marker the native side
acts on - so it comes up on GL from the first launch. If a launch does fail
before the renderer comes up, the panic is caught and the app brings itself back
on GL a few seconds later, so nothing has to be relaunched by hand. A panic later
in a run restarts the viewer on the backend that was plainly working, without
touching the markers. At most three restarts are allowed within ten minutes,
after which the viewer is left down rather than restarted in a loop.

On Android 10 and newer the framework can refuse that automatic restart as a
background start, and the wall then simply stays closed; granting *Display over
other apps* is what lifts the refusal, the same permission start-on-boot already
wants (see the manifest's `SYSTEM_ALERT_WINDOW`).

To force the Vulkan path on a device the gate would keep off it - to exercise the
fallback by hand - drop an empty file at
`/sdcard/Android/data/com.xhbl.xgview/files/force-vulkan`; the launch then says
so in its logcat. That file overrides the *version gate* only: a launch that
panicked still leaves its own marker behind and stays on GL, which is what stops
the restart from forcing its way back into the same panic. Removing the file and
launching once clears that marker and returns to the gate, so the sequence
`touch` - launch - `rm` - launch repeats the fallback without wiping anything.

### Linux / macOS: library, then Gradle

```bash
rustup target add aarch64-linux-android
cargo install cargo-ndk

scripts/build-android.sh                       # -> android-build/jniLibs/<abi>/libmonitor_android.so
ABIS="arm64-v8a armeabi-v7a" API=30 scripts/build-android.sh
gradle -p android assembleRelease
```

### How the APK is assembled

The Gradle project lives at `android/` and holds no sources of its own:
`sourceSets` take the manifest, the `java/` sources and the `res/` folder from
`crates/monitor_android/android/`, the native library from
`android-build/jniLibs/`, and the language packs from `langs/` as APK assets -
nothing is copied by hand. `minSdk` is 27, and the manifest declares
`INTERNET`, `RECEIVE_BOOT_COMPLETED` and the NativeActivity whose
`android.app.lib_name` matches the `monitor_android` crate.

### Deploy

```bash
scripts/deploy-android.sh 192.168.10.42
# Windows
powershell -ExecutionPolicy Bypass -File scripts\deploy-android.ps1 -Device 192.168.10.42
```

The script runs `adb connect <ip>:5555`, installs the APK with all runtime
permissions granted (`adb install -r -g`) so the unattended box never shows a
dialog, grants the special `SYSTEM_ALERT_WINDOW` permission the boot start
needs, launches the app and follows its logcat output.

## 6. Android release signing: what to carry to a new clone

The release APK is signed with a private key that is deliberately **not** in the
repository (see `.gitignore`). Cloning elsewhere therefore needs two files
copied by hand, and nothing else.

| File | Destination in the new clone | What it is |
|---|---|---|
| `xgview-release.jks` | `android/xgview-release.jks` | the PKCS12 keystore: the **private key** (alias `xgview`, RSA 2048) |
| `keystore.properties` | `android/keystore.properties` | `storeFile` / `storePassword` / `keyAlias` / `keyPassword`, passwords in plain text |

- The two must travel **together**. `scripts/build-android.ps1` refuses a tree
  that has only one of them ("release signing is half set up"), and Gradle falls
  back to the debug key with a warning if the properties file is missing.
- `storeFile=xgview-release.jks` is resolved relative to `android/`, so keep the
  pair in that directory rather than moving them elsewhere.
- Both are created automatically by the **first release build**
  (`keytool -genkeypair`, with a random 32-character password), which is why
  they are easy to lose: the password exists in no other place.
- A build with `-DebugBuild` (or `assembleDebug`) does not need either file - it
  is signed with the local debug key.

```powershell
# On the old machine: take a copy out of the repository, somewhere safe.
Copy-Item android\xgview-release.jks, android\keystore.properties D:\secure\xgview-signing\

# After cloning somewhere new: put them back into place.
Copy-Item D:\secure\xgview-signing\* .\android\
```

**Why it matters.** A release APK signed with a different key cannot upgrade an
installed one in place: Android refuses the install, and the only route is to
uninstall (losing the app's stored `config.json`) and install afresh. Keep an
offline backup of both files, out of the repository, for as long as a signed
release is in use.

### Do not carry these

They are generated, and are ignored by `.gitignore`:

| Path | What it is |
|---|---|
| `target/` | cargo output, including the built APK and the Windows zip |
| `android-build/` | `cargo-ndk` output (`jniLibs/`) |
| `android/build/`, `android/app/build/`, `android/.gradle/` | Gradle output |
| `android/local.properties` | not used: `build-android.ps1` finds the SDK/JDK/NDK through environment variables |
| `config.json` (at the repository root) | a local configuration, if one was put there |

The Android toolchain itself is an installation, not a file to copy: see §7 for
where its paths are read from.

## 7. Environment variables

| Variable | Used by | Purpose |
|---|---|---|
| `VCPKG_ROOT` | desktop build | where the FFmpeg development libraries are found (`ffmpeg-sys-next`) |
| `FFMPEG_DIR` | Windows packaging | alternative root for the FFmpeg runtime DLLs, beside `VCPKG_ROOT` |
| `LIBCLANG_PATH` | desktop build | `libclang.dll` / `libclang.so` for `bindgen` |
| `XGVIEW_ANDROID_ROOT` | `build-android.ps1` | toolchain root (NDK, JDK, Gradle, SDK); `C:\Android` by default |
| `ANDROID_NDK_HOME`, `ANDROID_HOME`, `JAVA_HOME` | cargo-ndk / Gradle | set by the build script from `XGVIEW_ANDROID_ROOT`; set them yourself when building with the `.sh` scripts |
| `XGVIEW_CONFIG_DIR` | the viewer | overrides the configuration directory (portable deployments) |
| `XGVIEW_HOME` | the viewer | the base directory `config_dir()` falls back to; the Android entry point sets it to the app's private storage |
| `RUST_LOG` | the viewer | log filter, e.g. `RUST_LOG=xgview=debug` |

## 8. Repository layout

```
xgview/
├── Cargo.toml                  # workspace root + the `xgview` binary
├── crates/
│   ├── monitor_core/           # platform independent logic
│   │   ├── model.rs            #   camera / stream data model, enum labels as i18n keys
│   │   ├── layout.rs           #   1x1 … 4x4 grids and pagination math
│   │   ├── scheduler.rs        #   which channel decodes which stream on which page
│   │   ├── config.rs           #   JSON persistence
│   │   ├── blackout.rs         #   the scheduled blank periods
│   │   ├── power.rs            #   keep the machine awake / the screen on
│   │   ├── rtsp.rs             #   async RTSP client (TCP interleaved / UDP)
│   │   ├── mjpeg.rs            #   MJPEG (`multipart/x-mixed-replace`) client
│   │   ├── h264.rs             #   RTP depacketizer (the decoder is in monitor_codec)
│   │   ├── pipeline.rs         #   per channel supervisor + backoff reconnection
│   │   ├── discovery/          #   ONVIF (WS-Discovery / SOAP), TCP scan, Synology
│   │   └── autostart.rs        #   start on boot helpers
│   ├── monitor_codec/          # platform decoder: FFmpeg / OpenH264, Android AMediaCodec, null fallback
│   ├── monitor_gui/            # egui + wgpu UI: grid, video upload, dialogs, toolbar, fonts
│   ├── monitor_i18n/           # Fluent language packs, English embedded
│   └── monitor_android/        # NativeActivity entry point + Android manifest sample
├── langs/                      # language packs shipped with a release (`zh-CN.ftl`, …)
├── docs/                       # this file, design notes, and `DevMemo.md`
├── android/                    # the Gradle project that turns the library into an APK
└── scripts/                    # Windows install and cargo-ndk / adb deployment helpers
```

## 9. Architecture principles

* **Non blocking UI** – the egui thread never touches a socket or a decoder. All
  network I/O and decoding run on a tokio runtime; results reach the UI through
  lock free channels (`crossbeam-channel` upward, `tokio::sync::mpsc` downward).
* **One picture format everywhere** – every backend hands the UI NV12 planes
  (luma plus one interleaved chroma plane), which are uploaded to two wgpu
  textures and converted by a fragment shader. On Android `AMediaCodec` decodes
  into byte buffers rather than a surface, and that buffer is walked into the
  same two planes; the surface path was given up because it aborts the process
  on some drivers (see [DevMemo.md](DevMemo.md) §9).
* **Hardware decoding where it is available** – Windows and Linux prefer a GPU
  decoder (`Direct3D 11` / `CUDA`, `VAAPI` / `CUDA`) and fall back to software
  for a device or a stream the hardware will not take; Android decodes through
  `AMediaCodec`. Each tile shows which one it got (`HW` / `SW`).
* **Text is keyed, not hard-coded** – `monitor_core` returns i18n keys and
  `monitor_gui` translates them through `monitor_i18n`; English is embedded and
  every other language is a `.ftl` file.
* **Conditional compilation** – `#[cfg(target_os = "android")]` /
  `#[cfg(target_os = "windows")]` select the decoder, the power mechanism and
  the start on boot mechanism. With the prerequisites of §1 in place, a plain
  `cargo run` on Windows is enough to get going.

# Changelog

What each release changed, newest first. Releases before 1.2.98 are in the git
history.

## 1.3.7

Changes since 1.2.107. Almost all of it the *add devices* window: two more
sources to import cameras from, and the details of the restream read from the
server rather than assumed.

### Import

- **Frigate and go2rtc, as two tabs of their own.** A wall that already has an
  aggregator in front of its cameras - Frigate (which embeds go2rtc) or a
  standalone go2rtc - can point XGView at it and import every camera it
  publishes, instead of re-adding each one. Frigate is read through its own API
  alone (`POST /api/login` for a JWT, then `/api/config`): it groups each camera
  with the go2rtc streams behind it, and pairs main and sub by **measured**
  resolution - never by Frigate's `detect` / `record` roles, which name a
  purpose and let a viewer record the sub feed to save space. go2rtc carries no
  grouping, so its tab is a flat list, one camera per stream. Neither tab
  depends on the other.
- **The "Synology NAS" tab is now "Synology SS".** It signs in to Surveillance
  Station, which is the program and not the NAS it runs on, so the wording that
  called it "the NAS" now says Surveillance Station.
- **The restream port and account are read from the server, not assumed.** Both
  were hard-coded to 8554. The port now comes from go2rtc's `rtsp.listen`
  (through `/api`) or from a Frigate input path, with 8554 only the last resort;
  the account is read where the server names one - Frigate's `/api/config`
  carries `go2rtc.rtsp` in clear, and go2rtc serves its own configuration file
  at `/api/config`, whose `rtsp:` block names the account its `/api` hides by
  design. Either can still be typed on the tab.
- **A missing restream account is said out loud.** When none can be read and the
  field is left empty, the go2rtc tab says so in place of its "leave empty" hint:
  a protected restream would otherwise fail with a 401 once the camera was added,
  with nothing to say why.
- Every tab now lays its accounts out the same way, and a status belongs to the
  tab that reported it. A failed request carries the `reqwest` source chain, so
  "error sending request for url (…)" is followed by the cause under it.

## 1.2.107

Changes since 1.2.98. 1.2.105 was built without an entry of its own, so its
changes are folded in here.

### Android

- **The viewer opens full screen in one step.** Full screen is asked for on the
  viewport builder, so the window is the screen before it is ever seen, rather
  than being shown at its window size and opening out a moment later.
- **Either landscape direction is allowed.** The activity is pinned to
  `userLandscape`, so a box mounted the other way round - or turned through 180
  degrees - no longer keeps an upside-down wall.
- **The wall keeps clear of the navigation bar wherever it is.** The strip is
  read on all four edges rather than the two horizontal ones, so a phone (strip
  along a short edge) and a tablet (along the bottom) both leave it free.

### Windows

- **The command line lives in `xgview.com`, installed beside `xgview.exe`.** A
  shell finds the `.com` first and, being a console application, waits for it -
  so `xgview --print-schedule` prints, `--install-autostart` reports a non-zero
  exit code, and a bare `xgview` shows the log in the console it was started
  from. `xgview.exe` takes the same switches and is what a shortcut and the
  start-on-boot entry run.

### Playback

- **The video is framed from the interleaved channel the server confirms, not
  the one requested.** A `SETUP` answer may name its own `interleaved=RTP-RTCP`
  pair; go2rtc numbers channels by track index, so a stream whose SDP lists
  audio first puts the video on `2-3` whatever was asked for. The client read
  channel 2 as RTCP and discarded the picture, and such a tile stayed on
  `connecting` for good. The requested pair is now only a suggestion.

### Discovery

- **A camera that names no sub stream still has one.** When no ONVIF profile
  names itself like a sub stream, the smallest genuinely *smaller* profile is
  used for the sub stream rather than the URL being guessed - so a FOSCAM's
  `prof0` / `prof1` gets a real `…/videoSub` from `GetStreamUri`.
- The sub-stream badge now says where an address came from, not what it looks
  like: a sub stream an ONVIF profile produced is no longer reported as
  "derived" merely because it reads like the `/videoMain` -> `/videoSub` rule's
  answer.

### Build

- **Linux is packaged in a container** (rockylinux:8), which lowers the glibc
  floor from the build host's 2.39 to **2.28**, and every platform's package is
  named one way: `xgview-<version>-<os>-<arch>.<ext>`. The Linux package ships
  its own `install.sh`.
- The docs note the APK is arm64-v8a only, and the 32-bit build examples are
  gone.

## 1.2.98

Changes since 1.2.95. Almost all of it Android, and most of that one class of
device: a box whose Vulkan driver is older than 1.1 (Adreno on Android 8.x,
Vulkan 1.0), which enumerates an adapter, hands out a device, and then loses it
while the first frame is being built.

### Android

- **Such a device now comes up on its first launch.** The Vulkan version is read
  from `PackageManager` before the renderer is built, and a device below 1.1 is
  sent to the OpenGL ES backend instead of being allowed to fail. Before this,
  the first launch died of `SIGABRT` and only the second one drew anything.
- **A launch that fails anyway is caught and recovered.** The renderer's panic is
  caught, written to logcat, and the app brings itself back on GL a few seconds
  later - no remote needed. A panic later in a run restarts the viewer on the
  backend that had plainly been working, and leaves the fallback markers alone.
- **Restarts are bounded**: three within ten minutes, after which the viewer is
  left closed. A device that can draw on neither backend no longer restarts
  itself for ever.
- **The fallback markers expire** when the app version or the device's
  `Build.FINGERPRINT` changes, so a driver fixed by an OTA is tried again.
- For support: a `force-vulkan` file in the app's external data directory forces
  the Vulkan path on a device the version gate would keep off it, and says so in
  its log.

### Build

- **Fixed: an Android build could package a stale ABI.** A library left in
  `android-build/jniLibs` by an earlier build for another ABI was merged into the
  APK even though the file name mentioned only the ABIs of that run.
  `xgview-1.2.98-arm64-v8a.apk` had grown to 21.8 MB that way - 8.9 MB of it a
  32-bit library older than every fix above. Both build scripts now empty the
  output tree first, and the APK is 12.9 MB again.

### Internals

- The backend decision is a pure function in `monitor_gui::backend`, with
  table-driven tests that run on the host rather than only on a device.
- No `std::env::set_var` is left in the process: the GL backend is requested
  through egui-wgpu's setup, and the Android configuration directory is handed
  to `monitor_core` as an argument.

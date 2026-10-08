# Changelog

What each release changed, newest first. Releases before 1.2.98 are in the git
history.

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

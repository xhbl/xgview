# XGView power management (prevent sleep / keep the screen on / Android keep-alive)

> Status: **implemented** on Windows, macOS, Linux and Android (screen + CPU).
> Android foreground-service keep-alive is the one part still outstanding; see
> §2.2 and §10.
> Decision: `prevent_sleep` and `keep_screen_on` are two independent switches,
> **both on by default** (the monitoring-wall case).

## 1. What this is, and where it lives

A monitoring wall is meant to stay up 7×24. A machine that sleeps or blanks its
display stops showing the cameras, and the streams are then cut and reconnected
for nothing. `crates/monitor_core/src/power.rs` holds the request for each
platform, and the settings panel offers it.

| Platform | No sleep | Screen on |
|---|---|---|
| Windows | `SetThreadExecutionState(ES_CONTINUOUS \| ES_SYSTEM_REQUIRED)` | `+ ES_DISPLAY_REQUIRED` |
| macOS | `caffeinate -i` | `+ -d`, with `-w <pid>` |
| Linux | `systemd-inhibit --what=sleep … sleep infinity` | `+ :idle` |
| Android | `PARTIAL_WAKE_LOCK` | `FLAG_KEEP_SCREEN_ON` |

Two facts shape the whole design:

- What is held is a **request**, not a setting. Every mechanism is released when
  the process ends, which is why the quit path matters (§7).
- The project had no power-management code before this. The Android manifest
  already declared `android.permission.WAKE_LOCK`, with a comment that named
  exactly this feature - the permission existed, nothing used it.

## 2. Goals and requirements

Three capabilities; two are delivered:

| Capability | Meaning | Platforms | Delivered |
|---|---|---|---|
| **Prevent system sleep** | The system does not suspend | all | yes |
| **Keep the screen on** | The display does not blank or screensaver | all | yes |
| **Keep-alive** | Stop the system reclaiming the process while it is on screen | Android | **no**, see §2.2 |

Two independent configuration switches:

- `prevent_sleep`: do not let the system suspend.
- `keep_screen_on`: keep the display on.
- Both default to **on**.

### 2.1 "Screen on" implies "system awake"

This has to be handled in the implementation, or the two switches can be
combined into a state that means nothing.

On all three desktop platforms the "screen on" mechanism acts on the **display
only** and does not stop the system suspending:

- Windows: `ES_DISPLAY_REQUIRED` only resets the display idle timer.
- macOS: `caffeinate -d` is display-only.
- Linux: `systemd-inhibit --what=idle` blocks idle (blank/screensaver) only.

So "screen on, sleep allowed" is a strange state: the screen is lit, but the
system may still suspend underneath it - and a suspension takes the screen with
it, so the combination is pointless.

**How it is handled: `plan()` normalises - asking for the screen also asks for
the system.** The UI says so (`settings-power-hint`), rather than presenting
them as two orthogonal switches.

### 2.2 Keep-alive is not delivered

The Android implementation today is `FLAG_KEEP_SCREEN_ON` plus
`PARTIAL_WAKE_LOCK`. The first is an activity window flag cleared with the
window; the second only keeps the CPU out of low-power states. **Neither stops
the system reclaiming the process** under memory pressure or background
restrictions.

Real keep-alive needs a foreground service: a `<service>` in the manifest, the
`FOREGROUND_SERVICE` permission, a persistent notification, and on Android 14+
a `foregroundServiceType` with its corresponding `FOREGROUND_SERVICE_*`
permission. That is a different order of cost from the two halves above, so it
is left as a separate phase; until it lands the UI must not claim "keep-alive".

## 3. Platform API matrix

| Capability | Windows | macOS | Linux | Android |
|---|---|---|---|---|
| No sleep | `SetThreadExecutionState(ES_CONTINUOUS \| ES_SYSTEM_REQUIRED)` | `caffeinate -i` | `systemd-inhibit --what=sleep` | `PARTIAL_WAKE_LOCK` |
| Screen on | the same `+ ES_DISPLAY_REQUIRED` | `+ -d` | `+ :idle` | `FLAG_KEEP_SCREEN_ON` |
| Keep-alive | n/a | n/a | n/a | foreground service (not built) |

Release differs on every one of them, and that is the easiest thing to get
wrong:

| Platform | Release | Note |
|---|---|---|
| Windows | `SetThreadExecutionState(ES_CONTINUOUS)` | **per-thread** state; must be set and cleared on a long-lived thread |
| macOS | kill the child | `-w <pid>` makes it exit with us, so no orphan |
| Linux | kill the child | `systemd-inhibit` has no `-w` equivalent, so a crash leaves the lock behind |
| Android | clear the flag / `release()` the wake lock | the wake lock must also be released in Java's `onDestroy` |

## 4. Architecture

`crates/monitor_core/src/power.rs`, modelled on `autostart.rs`: a
platform-independent API on top, one `imp` module per platform, errors reported
through `CoreError`.

```
power.rs
├── struct Applied { system: bool, display: bool }        // what is being held
├── fn plan(applied, prevent_sleep, keep_screen) -> Option<Applied>
├── pub struct PowerStatus { supported, applied, mechanism, error: Option<String> }
├── pub fn is_supported() -> bool
├── pub fn mechanism() -> &'static str
├── pub fn apply(prevent_sleep: bool, keep_screen: bool) -> Result<()>   // idempotent
├── pub fn release() -> Result<()>
├── pub fn status() -> PowerStatus
├── #[cfg(target_os = "android")] pub type AndroidRequest = fn(bool, bool) -> Result<()>
├── #[cfg(target_os = "android")] pub fn install_android(request: AndroidRequest)
├── #[cfg(windows)]                    mod imp — SetThreadExecutionState FFI
├── #[cfg(target_os = "macos")]        mod imp — caffeinate child process
├── #[cfg(all(unix, not(any(macos, android))))] mod imp — systemd-inhibit child process
├── #[cfg(all(unix, not(android)))]    mod holder — spawn a child and check it did not exit at once
├── #[cfg(target_os = "android")]      mod imp — forward to the AndroidRequest the GUI installed
└── #[cfg(not(any(windows, unix)))]    mod imp — CoreError::unsupported
```

**State**: `static Mutex<State>` holds the current `Applied` and the last error.
`apply` is idempotent - repeating the same request does nothing, and moving one
switch leaves the other where it was. The mutexes guard a few booleans and a
child handle, so a poisoned lock is recovered rather than propagated: refusing
to lock would turn a panic elsewhere into a process that can never take back
the wakefulness it asked for.

**Error reporting**: `CoreError::config` / `CoreError::unsupported`, as in
`autostart.rs`. A failure must be visible to the UI (§6), never silently
treated as success.

### 4.1 The testable half

`imp` calls the operating system directly, and no unit test can change the
machine's real power state. Following the `StallDeadline` / `ReconnectPolicy`
precedent in the repository, "what to do" is a pure function:

```rust
/// The normalised target against what is already held; `None` means no change.
fn plan(applied: Applied, prevent_sleep: bool, keep_screen: bool) -> Option<Applied>;
```

`plan` does two things: normalise (`keep_screen ⇒ system`, §2.1) and detect a
no-op (equal to `applied` → `None`). **This is what the unit tests cover**;
`apply` only carries out what `plan` returns.

### 4.2 Why Android is a hook

Android's two halves - `PARTIAL_WAKE_LOCK` and `FLAG_KEEP_SCREEN_ON` - both
reach the activity through JNI, and **the only place in the process that talks
to Java is `monitor_gui`**: `keyboard` holds the VM and the activity's class
reference, and `android` is its call layer. `monitor_core` cannot reach it,
because the dependency direction is gui → core and not the other way.

Options considered, third taken:

| Option | Problem |
|---|---|
| A second JNI bootstrap inside `monitor_core` | Two places in the process "know how to talk to Java", and they must stay in step forever |
| Move the whole Android branch into `monitor_gui` | `apply` / `release` / `status` split in half, `cfg` scattered through the UI, `PowerStatus` duplicated |
| **`imp(android)` forwards to a function pointer the GUI installs** | One layer of indirection, and the dependency direction and single responsibility stay clean |

So `power.rs` exposes one type and one installer:

```rust
#[cfg(target_os = "android")]
pub type AndroidRequest = fn(system: bool, display: bool) -> Result<()>;

#[cfg(target_os = "android")]
pub fn install_android(request: AndroidRequest);
```

`monitor_gui::app` installs it in `App::new`, **before anything asks whether the
platform can hold a wake request**:

```rust
#[cfg(target_os = "android")]
power::install_android(crate::android::set_keep_awake);
```

`is_supported()` on Android is simply "is the hook installed":

- installed → `true`, the panel shows both switches normally;
- not installed yet → `false`, the panel greys them out and says why (§6).

That gives "not implemented here" and "implemented but unavailable here" a
single exit, with **no separate gate flag**: a platform with no mechanism greys
out automatically, and Android becomes available automatically once the hook is
installed.

The other end is `monitor_gui::android::set_keep_awake`, which calls the two
Java methods through a JNI helper that takes a parameter:

```rust
/// Calls a Java static method taking one `boolean`, signature `"(Z)V"`.
fn call_void_bool(method: &str, value: bool) -> bool;
```

The parameter is necessary: the file's existing `call_void` / `call_bool` hard
code the signature as `"()V"` / `"()Z"` and take no argument. The helper also
clears a pending Java exception the way the rest of the file does - leaving one
pending aborts the process on the next JNI call. `set_keep_awake` returns an
error unless both calls reached the activity, so a wake request that silently
did nothing cannot pass.

## 5. Platform implementation

### 5.1 Windows

Hand-written FFI, no new crate (`kernel32` is linked by the standard library):

```rust
extern "system" {
    fn SetThreadExecutionState(flags: u32) -> u32;
}
const ES_CONTINUOUS: u32 = 0x8000_0000;
const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;
```

- Combine: `prevent_sleep` → `ES_CONTINUOUS | ES_SYSTEM_REQUIRED`;
  `keep_screen_on` → add `ES_DISPLAY_REQUIRED`.
- Release: `SetThreadExecutionState(ES_CONTINUOUS)`.
- **Check the return value**: it is `0` on failure, which is turned into a
  `CoreError` rather than being taken for success. (On success the first call
  returns the previous state, `0x80000000` by default, so "zero means failure"
  holds.)
- **The key trap**: `ES_CONTINUOUS` is **per-thread** state. It has to be set
  and cleared on a long-lived thread (eframe's `update` main thread), not on a
  temporary one, or the lock dies with the thread. `apply` / `release` are
  therefore called from the main thread.

The `extern` block is written **without** `unsafe`: this crate is edition 2021,
and the `unsafe extern` form would raise the effective minimum compiler past
the 1.80 the workspace declares.

### 5.2 macOS

An external command, to avoid IOKit FFI, in keeping with the project's existing
use of external commands (the `xdg-open` in `monitor_gui`):

- `prevent_sleep` → `caffeinate -i` (block idle sleep).
- `keep_screen_on` → add `-d` (block display sleep).
- **Add `-w <our_pid>`**: `caffeinate -w <pid>` exits when that process does.
  Without it, a crash leaves `caffeinate` orphaned and the assertion held until
  the next log out.
- The child's stdin and stdout are detached (`Stdio::null()`) and its stderr is
  piped, via `holder::watch`, so a refusal that comes back on stderr can be
  reported (see §5.3).
- The `std::process::Child` is kept in a static; `release` kills and reaps it.
  Because `caffeinate`'s switches are one assertion set, changing the request
  means starting a new process rather than re-arguing a running one.

### 5.3 Linux

`systemd-inhibit` holds the lock: **the inhibition lasts as long as the child
lives and is released the moment it exits.**

- `prevent_sleep` → `--what=sleep`; `keep_screen_on` → add `:idle`.
- Full command:

```text
systemd-inhibit --what=sleep:idle --mode=block --why="XGView is showing a surveillance wall" sleep infinity
```

- **Do not use `cat` as the COMMAND.** `systemd-inhibit` releases the lock when
  its COMMAND exits, and `cat` exits on stdin EOF - a GUI process's stdin is
  either `/dev/null` (immediate EOF) or a terminal `cat` would consume. Either
  way the lock is gone before it can be used. `sleep infinity` is used instead,
  with the child's stdin set to `null`.
- **Crash residue**: `systemd-inhibit` has no macOS `-w` equivalent, so if the
  main process is killed the holder survives and holds the lock until log out.
  This is a known residue, documented rather than fixed.
- **No `xdg-screensaver` fallback**: it is an X11 tool that does nothing in a
  Wayland session, and a switch that silently does nothing is worse than one the
  panel greys out.
- **`is_supported()` must actually ask, not just look on `PATH`.** Where there
  is no seat, logind refuses outright: `systemd-inhibit` exits 1 with `Failed to
  inhibit: Access denied` (containers and WSL sessions do this). So support is
  probed once with a command that ends at once (`systemd-inhibit … true`) and
  cached in a `OnceLock`, because `is_supported()` is reached every frame by the
  settings panel.
- **A child that dies at once must be noticed.** Both child-process mechanisms
  hold their lock only while the child lives, so "started and already dead"
  means the request was refused - and the reason is only on its stderr. After
  spawning, the child is watched for 150 ms (`holder::watch`); if it has exited
  the call fails with its stderr, e.g. `systemd-inhibit refused the request
  (exit status: 1): Failed to inhibit: Access denied`. **This is not cosmetic**:
  without it a switch reads "on" while nothing is held - a defect found by
  running the Linux side for real, where `apply` returned ok, `status` reported
  `applied = { system: true, display: true }`, and `systemd-inhibit --list`
  showed no lock of ours. macOS's `caffeinate` goes through the same `holder`
  helper.

### 5.4 Android (this phase: screen on + CPU awake)

Java's `MainActivity` gains two **static, single-argument** methods, both in the
existing shape (`instance` + `runOnUiThread`):

```java
private static PowerManager.WakeLock wakeLock;

/** Keeps the CPU running (the screen may still go off). WAKE_LOCK is declared. */
public static void setPreventSleep(final boolean wanted) {
    final MainActivity self = instance;
    if (self == null) {
        return;
    }
    self.runOnUiThread(() -> {
        if (!wanted) {
            releaseWakeLock();
            return;
        }
        if (wakeLock == null) {
            final PowerManager manager =
                    (PowerManager) self.getSystemService(Context.POWER_SERVICE);
            if (manager == null) {
                return;
            }
            wakeLock = manager.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "xgview:wall");
        }
        if (!wakeLock.isHeld()) {
            wakeLock.acquire();
        }
    });
}

/** Keeps the screen on. An activity window flag; no permission needed. */
public static void setKeepScreenOn(final boolean wanted) {
    final MainActivity self = instance;
    if (self == null) {
        return;
    }
    self.runOnUiThread(() -> {
        if (wanted) {
            self.getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        } else {
            self.getWindow().clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        }
    });
}
```

- **Never call `getWindow()` from a static method** - it is an instance method;
  reading `instance`, null-checking it, and running on the UI thread is the
  shape used everywhere in this file.
- Two imports are needed: `android.os.PowerManager` and
  `android.view.WindowManager`.
- **How it is reached**: the hook of §4.2 wires `monitor_core::power` to
  `monitor_gui::android::set_keep_awake`, which calls these two methods with
  `call_void_bool`. If either call does not reach the activity, `set_keep_awake`
  returns an error and the panel shows it under the switches - a wake request
  that did not take effect must not pass silently.
- The real code adds two guards over the shape above: `getSystemService` may
  return null (return without creating a lock), and release is folded into a
  `releaseWakeLock()` helper so the switch and `onDestroy` use one line.
- **`onDestroy` releases once more**: `WakeLock` is not released by losing the
  object that holds it, so an activity the system destroys (memory pressure, a
  rebuild) would leak the lock. `FLAG_KEEP_SCREEN_ON` clears with the window and
  needs no handling.
- `FLAG_KEEP_SCREEN_ON` does not conflict with `applySystemBars()`, which uses
  `setSystemUiVisibility` / insets and does not touch window flags.
- **Semantics**: with `prevent_sleep` on and `keep_screen_on` off, the screen
  goes dark, the activity becomes stopped, and the CPU keeps running on the wake
  lock - RTSP and decoding continue. That is the "screen off, streams still on"
  monitoring-wall case, and the UI text must make clear that the panel is not
  visible while the screen is dark.

## 6. Configuration and UI

`AppConfig` (`crates/monitor_core/src/config.rs`) carries two fields:

```rust
pub prevent_sleep: bool,
pub keep_screen_on: bool,
```

No `default_true()` helper is needed: `AppConfig` is annotated
`#[serde(default)]` at the struct level, so a missing field falls back to
`AppConfig::default()`, where both are `true`. Old configurations upgrade with
no version bump.

> Both defaulting to **on** means a normal desktop user starts blocking sleep
> and blanking immediately after the first run, and after an upgrade. That is
> right for a monitoring wall, but the switch must be easy to find and turn off.

**Start-up logs it.** `tracing::info!` records which switches were on, the
mechanism, and the resulting `Applied` - the only line pointing at XGView when a
user notices the machine no longer sleeps.

**UI location**: the **System** tab (`settings_system`), in the same section
pattern as autostart, under a "Keep awake" heading:

- two checkboxes (`settings-prevent-sleep`, `settings-keep-screen-on`), applied
  immediately on change;
- a hint line stating that keeping the screen on also keeps the machine awake;
- **unsupported platforms are disabled, not silent**: the checkboxes are drawn
  inside `add_enabled_ui(power_supported)` and an explanation
  (`settings-power-unsupported`) is shown below them, following the existing
  `settings-autostart-unsupported` precedent;
- **a failed `apply()` is visible**: the error is shown under the switches (and
  a toast is raised via `settings-power-failed`), never swallowed;
- the mechanism name is shown through the generic `settings-mechanism` wrapper,
  with the value keyed per platform.

**i18n keys** (`en.ftl` and `langs/zh-CN.ftl`):

- `settings-power`, `settings-prevent-sleep`, `settings-keep-screen-on`,
  `settings-power-hint`, `settings-power-unsupported`, `settings-power-failed`;
- `settings-mechanism` is reused as the wrapper
  (`mechanism: { $name }`), with the values in `power-mechanism-windows`,
  `power-mechanism-caffeinate`, `power-mechanism-systemd`,
  `power-mechanism-android`, `power-mechanism-unsupported`.

## 7. Lifecycle integration

| Moment | Action | Where |
|---|---|---|
| Start-up | `apply()` from the config, then `info!` the mechanism and result | `App::new` |
| Setting changed | re-`apply()` | System tab checkbox |
| Desktop exit | **explicit** `release()` | the `close_requested` branch of `update` |
| Android exit | **explicit** `release()` | `quit()`, before `std::process::exit(0)` |
| Android activity destroyed | Java releases the wake lock | `MainActivity.onDestroy` |

⚠️ **Do not rely on `Drop` to release.** Android's `quit()` goes through
`std::process::exit(0)`, which runs **no destructors**; the desktop exit path is
equally undependable for RAII. `release()` must be an explicit call before the
process ends - a hard constraint, not a style choice.

## 8. The blackout interaction

The blackout feature (a wall that blanks itself on a schedule) arrived after
this design and borrows the power request while it is active. In
`apply_power_for_blackout`:

- the two switches are the viewer's setting **or** `blackout_on`, so a blank
  wall asks for both;
- the blank borrows the request rather than changing the setting - when it ends,
  `apply_power_for_blackout` re-derives from the config and the power request
  returns to what the viewer chose;
- a failure to hold the machine awake for the blank is logged at `warn` and does
  not disturb the setting.

The reason it outranks the setting: a screen that goes black and is then powered
down is precisely the state the blackout exists to avoid.

## 9. Risks

- **Power draw**: screen on plus no sleep increases consumption, most of all on
  battery devices.
- **OLED burn-in**: a static wall lit for a long time risks burn-in.
- **A changed default behaviour**: old configurations start blocking sleep
  automatically; the switch must be visibly available and the start-up log must
  record it.
- **Windows thread constraint**: `SetThreadExecutionState` must be called from
  the main thread or the lock is lost.
- **Linux fragmentation / refused inhibition**: without systemd, or without a
  seat (containers, WSL), logind refuses. `is_supported()` reflects that
  truthfully via a real probe, and `apply()` shows the refusal in the panel;
  neither path passes silently.
- **Linux orphan lock**: `systemd-inhibit` has no `-w` equivalent, so a crash
  leaves a holder behind (macOS avoids it with `caffeinate -w`).
- **Android wake-lock leak**: Java must pair acquire/release, and `onDestroy`
  must release once more.
- **Switch combination semantics**: without the §2.1 normalisation, "screen on
  but sleep allowed" is a contradictory state.
- **Dead switches on unsupported platforms**: if `is_supported()` is false and
  the panel does not grey out, the user believes it took effect.

## 10. Implementation status and remaining work

- **Phase 1 - desktop (Windows / macOS / Linux): done.** `power.rs` with
  `Applied` + `plan()` (unit-tested), the platform-independent API, and the
  three `imp` modules; `AppConfig` fields and defaults; `App::new` apply +
  `info!` and the close path `release()`; the System-tab switches with the
  disabled-when-unsupported handling; the i18n keys.
- **Phase 2 - Android: done.** `MainActivity.setPreventSleep` /
  `setKeepScreenOn` (and `releaseWakeLock` in `onDestroy`); `call_void_bool` and
  `set_keep_awake` in `android.rs`; `install_android` wired in `App::new`;
  `release()` in `quit()` before `std::process::exit(0)`. With the hook
  installed, `is_supported()` is true and the phase-1 grey-out disappears with
  no separate gate to open.
- **Phase 3 - keep-alive (optional, separate evaluation): not started.** Add
  `<service>` + `FOREGROUND_SERVICE` (and `foregroundServiceType` plus its
  permission on Android 14+), a persistent notification, and a user-facing way
  to turn it off. **Only at that point may the UI claim "keep-alive".**

## 11. Verification

- **Unit tests** (`power.rs`): `plan()`'s normalisation (`keep_screen ⇒ system`)
  and idempotence (the same request returns `None`), release covering all four
  switch combinations, the mechanism name being a translatable key, and
  `status()` agreeing with `is_supported()`. None of these touch the real
  system.
- **Manual checks**:
  - **Windows**: `powercfg /requests` shows the SYSTEM / DISPLAY request - but
    it **needs administrator rights** and a non-elevated shell is refused
    outright. Without elevation, read the application's own log:
    `xgview::power`'s `execution state requested` and `power management at
    start-up` with `applied` true. (This is enough because
    `SetThreadExecutionState` does not return 0 on success; no failure warning
    in the log means the request was accepted.) To observe it independently,
    set the power-plan timeout to one minute and watch whether the screen blanks
    or the machine sleeps.
  - **macOS**: `pmset -g assertions` shows the caffeinate assertion; also
    confirm `caffeinate` disappears when XGView is killed (`-w` working).
  - **Linux**: `systemd-inhibit --list` shows the lock, `loginctl` shows idle;
    then kill the process and confirm the lock is gone (otherwise it is the §9
    orphan). **In a container or WSL this usually cannot be held** -
    `systemd-inhibit` answers `Failed to inhibit: Access denied` and exits 1, so
    `is_supported()` is false and the panel greys out, which is the expected
    behaviour rather than a defect; verifying a real inhibition needs a real
    Linux desktop session with a seat.
  - **Android**: read three independent pieces of evidence together -
    `adb shell dumpsys power` has `PARTIAL_WAKE_LOCK 'xgview:wall' (uid=…)`;
    `adb shell dumpsys window windows` shows our window's `fl=` containing
    `KEEP_SCREEN_ON` with `mHoldScreenWindow` pointing at it; and
    `adb logcat -s xgview` shows `power management at start-up` with
    `supported=true` and `applied=Applied { system: true, display: true }`. The
    first two prove the effect; the last proves the hook was installed and both
    JNI calls succeeded. Also destroy the activity deliberately and confirm no
    wake lock is left behind.

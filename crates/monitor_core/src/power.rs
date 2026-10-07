//! Cross platform power management: keep the machine awake, and the screen on.
//!
//! | Platform | Screen on | System awake |
//! |----------|-----------|--------------|
//! | Windows  | `SetThreadExecutionState(ES_CONTINUOUS \| ES_SYSTEM_REQUIRED \| ES_DISPLAY_REQUIRED)` | `ES_SYSTEM_REQUIRED` alone |
//! | macOS    | `caffeinate -i -d` child process | `-i` alone |
//! | Linux    | `systemd-inhibit --what=sleep:idle` child process | `--what=sleep` alone |
//! | Android  | `FLAG_KEEP_SCREEN_ON` + `PARTIAL_WAKE_LOCK` | the same |
//!
//! A monitoring wall is meant to stay on: a machine that sleeps, or blanks its
//! display, stops showing the cameras - and the streams are then cut and
//! reconnected for nothing. What is held here is a *request*, not a setting:
//! every mechanism is released when the process ends, which is why quitting
//! matters and why two of them are child processes that have to be told to die
//! with us.
//!
//! **Keeping the screen on does not imply keeping the system awake** on any of
//! these platforms. Each mechanism only resets the *display*'s idle timer, so
//! the two switches could otherwise be combined into a state that means
//! nothing: a screen that stays lit until the system suspends underneath it,
//! taking the screen with it. [`plan`] therefore normalises - asking for the
//! screen also asks for the system.
//!
//! Android's two halves belong to the GUI crate, which is where the bridge to
//! the activity lives: it installs them through [`install_android`] before the
//! settings panel is drawn, and until it does [`is_supported`] answers `false`
//! there, so the panel greys the switches out instead of offering two that do
//! nothing.

use std::sync::{Mutex, MutexGuard};

use crate::error::Result;

/// What a platform mechanism has been asked for.
///
/// Both are requests to a system that may refuse them, which is why
/// [`PowerStatus::error`] exists: a switch that is on while nothing is held is
/// a lie the panel should not tell.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Applied {
    /// The system has been asked not to sleep.
    pub system: bool,
    /// The display has been asked to stay on.
    pub display: bool,
}

/// Everything the settings panel needs to draw the switches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerStatus {
    /// `false` when this platform has no mechanism, or none that is usable here.
    pub supported: bool,
    /// What is held right now.
    pub applied: Applied,
    /// i18n key of the mechanism, shown through `settings-mechanism`.
    pub mechanism: &'static str,
    /// Why the last attempt failed, when one did.
    pub error: Option<String>,
}

/// The switches as a target state, or `None` when nothing has to change.
///
/// This is the whole decision, kept apart from carrying it out so that it can
/// be tested without a machine to change: it normalises (a screen request
/// implies a system request, see the module note) and it reports a no-op, which
/// is what makes [`apply`] idempotent rather than a stream of repeated calls
/// into the operating system.
fn plan(applied: Applied, prevent_sleep: bool, keep_screen: bool) -> Option<Applied> {
    let wanted = Applied { system: prevent_sleep || keep_screen, display: keep_screen };
    (wanted != applied).then_some(wanted)
}

/// What this process is holding, and why the last attempt failed.
struct State {
    applied: Applied,
    error: Option<String>,
}

static STATE: Mutex<State> =
    Mutex::new(State { applied: Applied { system: false, display: false }, error: None });

/// Locks one of the statics, recovering from a panic that happened while it was
/// held.
///
/// The locks here guard a few booleans and a child handle, so a poisoned one is
/// not hiding state that has been half written - and refusing to lock it would
/// turn a panic somewhere else into a process that can never take back the
/// request it made, which is exactly the wakefulness nobody asked for.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `true` when this platform has a mechanism, and it can be used here.
///
/// Linux is the one that has to be asked rather than assumed: the mechanism is
/// systemd's, and a distribution without it - or a container that hides it -
/// has nothing to hold. Answering `false` beats pretending, because the panel
/// greys the switches out on `false` instead of letting the viewer tick
/// something that does nothing.
pub fn is_supported() -> bool {
    imp::supported()
}

/// i18n key of the mechanism used here, e.g. `power-mechanism-systemd`.
///
/// A key rather than the text itself, the way [`crate::autostart`] does it: the
/// name is translated wherever it is shown.
pub fn mechanism() -> &'static str {
    #[cfg(windows)]
    {
        "power-mechanism-windows"
    }
    #[cfg(target_os = "macos")]
    {
        "power-mechanism-caffeinate"
    }
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
    {
        "power-mechanism-systemd"
    }
    #[cfg(target_os = "android")]
    {
        "power-mechanism-android"
    }
    #[cfg(not(any(windows, unix)))]
    {
        "power-mechanism-unsupported"
    }
}

/// Asks for the machine to stay awake and/or the screen to stay on.
///
/// Idempotent: a call that asks for what is already held does nothing, and a
/// call that moves one switch leaves the other where it was.
///
/// **Must be called from the thread that stays alive.** On Windows the request
/// is per-thread state, so setting it on a worker that then ends releases it
/// again; the two child process mechanisms have no such constraint, but the
/// callers are the same ones either way (start-up, the settings panel, exit).
pub fn apply(prevent_sleep: bool, keep_screen: bool) -> Result<()> {
    let mut state = lock(&STATE);
    let Some(wanted) = plan(state.applied, prevent_sleep, keep_screen) else {
        return Ok(());
    };
    match imp::apply(wanted) {
        Ok(()) => {
            state.applied = wanted;
            state.error = None;
            Ok(())
        }
        Err(err) => {
            // The state is left alone rather than assumed: a mechanism that
            // failed has not moved anything, and reporting the switches as on
            // would be the panel lying about the machine.
            state.error = Some(err.to_string());
            Err(err)
        }
    }
}

/// Takes the request back.
///
/// **Called explicitly, never from a destructor.** Android's quit path ends in
/// `std::process::exit`, which runs no destructors at all, so a `Drop`
/// implementation would silently never release the wake lock - see
/// `docs/power-management-plan.md` §7.
pub fn release() -> Result<()> {
    imp::release()?;
    lock(&STATE).applied = Applied::default();
    Ok(())
}

/// [`is_supported`], the mechanism, what is held, and the last failure.
pub fn status() -> PowerStatus {
    let state = lock(&STATE);
    PowerStatus {
        supported: is_supported(),
        applied: state.applied,
        mechanism: mechanism(),
        error: state.error.clone(),
    }
}

/// Starting a lock holder, and refusing to believe one that died at once.
///
/// Both child process mechanisms hold their lock only while the child lives, so
/// a child that exits immediately is a request that was **refused** - and it
/// says why on its standard error, which is the only place the reason appears.
/// `systemd-inhibit` answers `Failed to inhibit: Access denied` and exits 1
/// where logind will not grant it, which is what a container or a WSL session
/// holding no seat looks like.
///
/// Believing the spawn is how a switch comes to read "on" while nothing at all
/// is held, which is the one outcome this module exists to prevent. Caught by
/// running the Linux side for real: the request was accepted, `status()` said
/// `applied = { system: true, display: true }`, and `systemd-inhibit --list`
/// showed no lock of ours.
#[cfg(all(unix, not(target_os = "android")))]
mod holder {
    use std::io::Read;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use crate::error::{CoreError, Result};

    /// How long a holder is watched before it is believed.
    ///
    /// Paid only when the switches move, not per frame: a refusal comes back
    /// from the bus in single digit milliseconds, so this is generous, and the
    /// cost is a bounded stall on a settings toggle.
    const SETTLE: Duration = Duration::from_millis(150);

    /// Spawns `command` with its output detached, and returns it only once it
    /// has stayed alive through [`SETTLE`].
    pub(super) fn watch(command: &mut Command, what: &str) -> Result<Child> {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| CoreError::config(format!("cannot run {what}: {err}")))?;
        let deadline = Instant::now() + SETTLE;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(None) => std::thread::sleep(Duration::from_millis(15)),
                Ok(Some(status)) => {
                    return Err(CoreError::config(format!(
                        "{what} refused the request ({status}): {}",
                        reason(&mut child)
                    )))
                }
                Err(err) => return Err(CoreError::config(format!("cannot watch {what}: {err}"))),
            }
        }
        Ok(child)
    }

    /// What the holder said on its way out.
    fn reason(child: &mut Child) -> String {
        let mut text = String::new();
        if let Some(mut errors) = child.stderr.take() {
            let _ = errors.read_to_string(&mut text);
        }
        let text = text.trim();
        if text.is_empty() {
            "no reason given".to_string()
        } else {
            text.to_string()
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use crate::error::CoreError;

    // `kernel32` is linked by the standard library on Windows, so declaring the
    // one function needed here is cheaper than a dependency for it. Written
    // without `unsafe` on the block: this crate is edition 2021, and the
    // `unsafe extern` form would raise the effective minimum compiler past the
    // 1.80 the workspace declares.
    extern "system" {
        fn SetThreadExecutionState(flags: u32) -> u32;
    }

    const ES_CONTINUOUS: u32 = 0x8000_0000;
    const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
    const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;

    pub(super) fn supported() -> bool {
        true
    }

    pub(super) fn apply(wanted: Applied) -> Result<()> {
        let mut flags = ES_CONTINUOUS;
        if wanted.system {
            flags |= ES_SYSTEM_REQUIRED;
        }
        if wanted.display {
            flags |= ES_DISPLAY_REQUIRED;
        }
        // Safety: the call takes a bitmask of the four constants above and
        // changes only the calling thread's execution state. It touches no
        // memory of ours, and its result is checked below.
        let previous = unsafe { SetThreadExecutionState(flags) };
        if previous == 0 {
            // Documented to return zero on failure, and to have changed
            // nothing when it does.
            return Err(CoreError::config(format!(
                "SetThreadExecutionState(0x{flags:08x}) was refused"
            )));
        }
        tracing::info!(
            target: "xgview::power",
            system = wanted.system,
            display = wanted.display,
            "execution state requested"
        );
        Ok(())
    }

    pub(super) fn release() -> Result<()> {
        // `ES_CONTINUOUS` on its own, with neither request, is how the thread's
        // state is cleared back to the default.
        // Safety: as above.
        let previous = unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
        if previous == 0 {
            return Err(CoreError::config("SetThreadExecutionState could not be cleared"));
        }
        tracing::info!(target: "xgview::power", "execution state released");
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::process::{Child, Command};

    use super::*;

    /// The assertion holder, `None` while nothing is held.
    ///
    /// A child rather than IOKit: `caffeinate` is the tool the platform itself
    /// ships for this, and the project already runs external commands where
    /// that is the smaller change (the `xdg-open` in `monitor_gui`).
    static CHILD: Mutex<Option<Child>> = Mutex::new(None);

    pub(super) fn supported() -> bool {
        true
    }

    pub(super) fn apply(wanted: Applied) -> Result<()> {
        // `caffeinate`'s switches are one assertion set, so a change means a new
        // process rather than an argument changed under a running one.
        stop();
        let mut command = Command::new("caffeinate");
        if wanted.system {
            command.arg("-i");
        }
        if wanted.display {
            command.arg("-d");
        }
        // `-w` makes it exit when this process does. Without it a crash leaves
        // the assertion behind - keeping a machine awake with nothing running
        // on it - until the next log out.
        command.arg("-w").arg(std::process::id().to_string());
        let child = holder::watch(&mut command, "caffeinate")?;
        *lock(&CHILD) = Some(child);
        tracing::info!(
            target: "xgview::power",
            system = wanted.system,
            display = wanted.display,
            "caffeinate started"
        );
        Ok(())
    }

    pub(super) fn release() -> Result<()> {
        stop();
        Ok(())
    }

    /// Kills the holder, if there is one, and reaps it.
    fn stop() {
        if let Some(mut child) = lock(&CHILD).take() {
            let _ = child.kill();
            // Reaped rather than left a zombie: this process may go on running
            // for weeks after the switch is turned off.
            let _ = child.wait();
        }
    }
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
mod imp {
    use std::process::{Child, Command, Stdio};
    use std::sync::OnceLock;

    use super::*;

    /// The lock holder, `None` while nothing is held.
    static CHILD: Mutex<Option<Child>> = Mutex::new(None);

    /// Whether an inhibitor can actually be held here, worked out once.
    ///
    /// Being on the `PATH` is not the same as being allowed: logind refuses the
    /// request outright where the process holds no seat - a container, or a WSL
    /// session, which is where this was found - answering `Failed to inhibit:
    /// Access denied`, and the switch would then appear only to do nothing. So
    /// the question is asked once by asking: `true` is a command that exits at
    /// once, taking the lock and giving it back in the same breath.
    ///
    /// Asked once per process because `is_supported` is reached from the
    /// settings panel, which draws every frame.
    ///
    /// There is deliberately no `xdg-screensaver` fallback, which an earlier
    /// draft of the plan had. It is an X11 tool that does nothing at all on a
    /// Wayland session, and a switch that silently does nothing is worse than
    /// one the panel greys out.
    pub(super) fn supported() -> bool {
        static ALLOWED: OnceLock<bool> = OnceLock::new();
        *ALLOWED.get_or_init(|| {
            Command::new("systemd-inhibit")
                .arg("--what")
                .arg("sleep")
                .arg("--mode")
                .arg("block")
                .arg("--why")
                .arg("XGView is checking whether it may")
                .arg("true")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
        })
    }

    pub(super) fn apply(wanted: Applied) -> Result<()> {
        stop();
        let what = match (wanted.system, wanted.display) {
            (true, true) => "sleep:idle",
            (true, false) => "sleep",
            (false, true) => "idle",
            // Normalised away by `plan`; nothing can ask for neither.
            (false, false) => return Ok(()),
        };
        let mut command = Command::new("systemd-inhibit");
        command
            .arg("--what")
            .arg(what)
            .arg("--mode")
            .arg("block")
            .arg("--why")
            .arg("XGView is showing a surveillance wall")
            // The lock lives exactly as long as this command does, so the
            // command has to be one that does not finish. `cat` is the
            // conventional choice and it is wrong: it reads standard input, and
            // a GUI process's standard input is either a terminal it would
            // consume or a closed one that hands it end of file at once -
            // either way the lock is gone before anyone can use it.
            .arg("sleep")
            .arg("infinity");
        let child = holder::watch(&mut command, "systemd-inhibit")?;
        *lock(&CHILD) = Some(child);
        tracing::info!(target: "xgview::power", what, "inhibitor started");
        Ok(())
    }

    pub(super) fn release() -> Result<()> {
        stop();
        Ok(())
    }

    /// Kills the holder, if there is one, and reaps it.
    fn stop() {
        if let Some(mut child) = lock(&CHILD).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The Android implementation, supplied by the GUI crate.
///
/// Android's two halves are a `PARTIAL_WAKE_LOCK` from `PowerManager` and
/// `FLAG_KEEP_SCREEN_ON` on the activity's window, and reaching either means
/// the JNI bridge to the activity - which lives in `monitor_gui` (`keyboard`
/// and `android`), the only place in the process that knows how to talk to
/// Java. Copying the bridge here to avoid the indirection would leave two of
/// them to keep in step.
#[cfg(target_os = "android")]
pub type AndroidRequest = fn(system: bool, display: bool) -> Result<()>;

#[cfg(target_os = "android")]
static ANDROID_REQUEST: Mutex<Option<AndroidRequest>> = Mutex::new(None);

/// Installs the Android implementation; see [`AndroidRequest`].
///
/// Called once, before anything asks what the platform can hold.
#[cfg(target_os = "android")]
pub fn install_android(request: AndroidRequest) {
    *lock(&ANDROID_REQUEST) = Some(request);
}

#[cfg(target_os = "android")]
mod imp {
    use super::*;
    use crate::error::CoreError;

    pub(super) fn supported() -> bool {
        lock(&ANDROID_REQUEST).is_some()
    }

    pub(super) fn apply(wanted: Applied) -> Result<()> {
        // Copied out before the call, so the lock is not held across it.
        let Some(request) = *lock(&ANDROID_REQUEST) else {
            return Err(CoreError::unsupported("power management is not wired up on Android"));
        };
        request(wanted.system, wanted.display)
    }

    pub(super) fn release() -> Result<()> {
        let Some(request) = *lock(&ANDROID_REQUEST) else {
            return Ok(());
        };
        request(false, false)
    }
}

#[cfg(not(any(windows, unix)))]
mod imp {
    use super::*;
    use crate::error::CoreError;

    pub(super) fn supported() -> bool {
        false
    }

    pub(super) fn apply(_wanted: Applied) -> Result<()> {
        Err(CoreError::unsupported("power management is not available on this platform"))
    }

    pub(super) fn release() -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four combinations, as a table, so the tests read as statements about
    /// the switches rather than about `bool` arithmetic.
    const ALL: [(bool, bool); 4] = [(false, false), (false, true), (true, false), (true, true)];

    #[test]
    fn asking_for_the_screen_asks_for_the_system_too() {
        // A lit screen over a suspended system is not a state worth being able
        // to ask for: the suspension takes the screen with it.
        assert_eq!(plan(Applied::default(), false, true), Some(Applied { system: true, display: true }));
    }

    #[test]
    fn the_system_can_be_held_with_the_screen_dark() {
        // The other direction is honoured as asked: the streams are the point,
        // and a wall that keeps pulling them with its display off is a real
        // thing to want.
        assert_eq!(plan(Applied::default(), true, false), Some(Applied { system: true, display: false }));
    }

    #[test]
    fn nothing_is_asked_for_twice() {
        assert_eq!(plan(Applied::default(), false, false), None, "nothing from nothing");
        assert_eq!(
            plan(Applied { system: true, display: true }, true, true),
            None,
            "the same again"
        );
        // The normalised form of this pair is already held, so the second call
        // is not a change either.
        assert_eq!(plan(Applied { system: true, display: true }, false, true), None);
    }

    #[test]
    fn every_switch_can_be_taken_back() {
        for (system, display) in ALL {
            let held = Applied { system, display };
            let released = plan(held, false, false);
            if held == Applied::default() {
                // Nothing was held, so there is nothing to take back - and
                // saying so is what keeps `apply` from calling into the
                // operating system to undo work that was never done.
                assert_eq!(released, None);
            } else {
                assert_eq!(
                    released,
                    Some(Applied::default()),
                    "holding {system}/{display} was not released by asking for nothing"
                );
            }
        }
    }

    #[test]
    fn the_mechanism_is_a_key_the_panel_can_translate() {
        // A key, not the text: `monitor_gui` puts it through `monitor_i18n`.
        assert!(mechanism().starts_with("power-mechanism-"), "{}", mechanism());
    }

    #[test]
    fn status_reports_what_is_supported() {
        let status = status();
        assert_eq!(status.supported, is_supported());
        // Nothing is held in a fresh test process, and a platform that cannot
        // hold anything must not claim it can.
        if !status.supported {
            assert_eq!(status.applied, Applied::default());
        }
    }
}

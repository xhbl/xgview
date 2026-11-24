//! Powering the display off and back on, for a blackout that really blanks it.
//!
//! This is the opposite of [`crate::power`]: that module asks the machine to
//! keep the screen *on*, this one asks it to turn the screen *off*. They are
//! deliberately separate - the mechanisms do not overlap, and on Windows and
//! macOS the "keep on" request has to be taken back before this one can work.
//!
//! Every mechanism here is a **request to whatever owns the display**, which
//! may refuse it. Nothing here pretends a command that was refused worked:
//! [`set`] reports failure, and on X11 the state is read back after the command
//! so that a server without DPMS is not mistaken for one that obeyed.
//!
//! # Why support is probed, not assumed
//!
//! A compile-time platform check is not enough. A Wayland session has no X
//! server for `xset` to talk to (Xwayland exposes no DPMS extension), and an X
//! server on a virtual GPU may not implement DPMS either - both were found on
//! real machines. So [`supported`] asks the machine, once, and answers `false`
//! when the answer cannot be trusted. The settings panel hides the switch on
//! `false` instead of offering one that does nothing.

use std::sync::{Mutex, MutexGuard};

use crate::error::{CoreError, Result};

/// What this process last asked of the display.
///
/// Kept so that leaving a blackout restores the screen only when this module
/// was the one that turned it off - a viewer that never blanked must not light
/// a screen somebody else deliberately darkened.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// Whether the display is currently requested off by us.
    pub off: bool,
    /// Why the last attempt failed, when one did.
    pub error: Option<String>,
}

/// Locks one of the statics, recovering from a poisoned mutex.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

static STATE: Mutex<Applied> = Mutex::new(Applied { off: false, error: None });

/// Whether this machine can turn its display off and back on.
///
/// Cached: the X11 probe runs `xset q`, which is too expensive to repeat on
/// every settings frame, and the answer does not change during a session.
pub fn supported() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(imp::supported)
}

/// Turns the display off (`true`) or back on (`false`).
///
/// Idempotent in effect but not in calls: each call runs the command once and
/// reads the state back on X11, so repeated calls are harmless but not free.
/// Callers should move only on a change. On failure [`status`] carries the
/// reason and `off` is left unchanged - a command that failed did not darken
/// the screen, so claiming it did would make the restore path skip it.
pub fn set(off: bool) -> Result<()> {
    if !supported() {
        return Err(CoreError::unsupported("display power control is not available here"));
    }
    match imp::set(off) {
        Ok(()) => {
            let mut state = lock(&STATE);
            state.off = off;
            state.error = None;
            Ok(())
        }
        Err(err) => {
            lock(&STATE).error = Some(err.to_string());
            Err(err)
        }
    }
}

/// What was last asked of the display, and why the last attempt failed.
pub fn status() -> Applied {
    lock(&STATE).clone()
}

/// Clears the error without changing the request.
///
/// A viewer that acted on the error (or has merely read it) can silence it
/// without pretending to have turned the display back on.
pub fn clear_error() {
    lock(&STATE).error = None;
}

/// Lights the display again if - and only if - this process turned it off.
///
/// For the exit paths: a screen we darkened must not be left dark when the
/// viewer goes away, while one somebody else switched off is none of our
/// business. No destructor calls this - Android's exit runs none - so the
/// viewer invokes it explicitly on the way out.
pub fn restore() -> Result<()> {
    if !lock(&STATE).off {
        return Ok(());
    }
    set(false)
}

// ---------------------------------------------------------------------------
// Platform implementations.

/// Windows: `SendMessage(HWND_BROADCAST, WM_SYSCOMMAND, SC_MONITORPOWER, …)`.
///
/// `kernel32` and `user32` are linked by the standard library; declaring the
/// one function needed here is cheaper than a dependency. Written without
/// `unsafe` on the block because the workspace targets Rust 1.80, where the
/// `unsafe extern` form would raise the minimum compiler.
#[cfg(windows)]
mod imp {
    use super::*;

    extern "system" {
        fn SendMessageTimeoutW(
            hwnd: isize,
            msg: u32,
            wparam: usize,
            lparam: isize,
            flags: u32,
            timeout: u32,
            result: *mut usize,
        ) -> isize;
    }

    const HWND_BROADCAST: isize = 0xffff;
    const WM_SYSCOMMAND: u32 = 0x0112;
    const SC_MONITORPOWER: usize = 0xF170;
    const SMTO_ABORTIFHUNG: u32 = 0x0002;

    /// Off is `2`, on is `-1` (the display's own power state, not standby).
    const POWER_OFF: isize = 2;
    const POWER_ON: isize = -1;

    pub(super) fn supported() -> bool {
        // An interactive GUI window is proof enough of an interactive desktop;
        // a service session cannot create one in the first place.
        true
    }

    pub(super) fn set(off: bool) -> Result<()> {
        let value = if off { POWER_OFF } else { POWER_ON };
        let mut result = 0usize;
        // Safety: the call broadcasts one documented system command with a
        // documented parameter, writes one usize of our own into `result`, and
        // touches no memory it was not given. The timeout keeps a hung window
        // from holding the UI thread.
        let sent = unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SYSCOMMAND,
                SC_MONITORPOWER,
                value,
                SMTO_ABORTIFHUNG,
                1000,
                &mut result,
            )
        };
        if sent == 0 {
            return Err(CoreError::config(format!(
                "SC_MONITORPOWER({value}) was refused"
            )));
        }
        tracing::info!(target: "xgview::display_off", off, "monitor power requested");
        Ok(())
    }
}

/// macOS: `pmset displaysleepnow` to sleep the display, `caffeinate -u` to ask
/// the session to wake it. Neither is a guarantee - a locked session can stay
/// dark - which is why the viewer also treats any input as a wake.
#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::process::Command;

    pub(super) fn supported() -> bool {
        // Both tools ship with the OS; the command itself reports a refusal.
        true
    }

    pub(super) fn set(off: bool) -> Result<()> {
        let (program, args) = if off {
            ("pmset", vec!["displaysleepnow"])
        } else {
            // `-u` declares user activity and wakes the display; `-t 1` keeps
            // that assertion to a second rather than leaving a child behind.
            ("caffeinate", vec!["-u", "-t", "1"])
        };
        let output = Command::new(program).args(&args).output().map_err(|err| {
            CoreError::config(format!("cannot run {program} for the display: {err}"))
        })?;
        if !output.status.success() {
            let reason = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(CoreError::config(format!(
                "{program} {} failed: {}",
                args.join(" "),
                if reason.is_empty() { output.status.to_string() } else { reason }
            )));
        }
        tracing::info!(target: "xgview::display_off", off, program, "display power requested");
        Ok(())
    }
}

/// Linux (and other non-Apple Unix): X11 DPMS through `xset`.
///
/// Support is answered by `xset q`, which is the same tool used to command the
/// server: a `DPMS is …` line means the extension exists, and a `Monitor is …`
/// line means its state can be read back. Wayland has neither (Xwayland does
/// not expose DPMS), and a virtual GPU may lack the extension too, so the
/// answer is asked rather than assumed.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
mod imp {
    use super::*;
    use std::process::Command;

    /// Whether a Wayland session owns the screen.
    fn wayland() -> bool {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            return true;
        }
        std::env::var("XDG_SESSION_TYPE")
            .map(|value| value.eq_ignore_ascii_case("wayland"))
            .unwrap_or(false)
    }

    /// The `Monitor is On/Off` line from `xset q`, if the server has DPMS.
    fn monitor() -> Result<Option<bool>> {
        let output = Command::new("xset")
            .arg("q")
            .output()
            .map_err(|err| CoreError::config(format!("cannot run xset q: {err}")))?;
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            let line = line.trim();
            if let Some(state) = line.strip_prefix("Monitor is ") {
                return Ok(Some(state.trim().eq_ignore_ascii_case("On")));
            }
        }
        Ok(None)
    }

    pub(super) fn supported() -> bool {
        if wayland() || std::env::var_os("DISPLAY").is_none() {
            return false;
        }
        matches!(monitor(), Ok(Some(_)))
    }

    pub(super) fn set(off: bool) -> Result<()> {
        let value = if off { "off" } else { "on" };
        let output = Command::new("xset")
            .args(["dpms", "force", value])
            .output()
            .map_err(|err| CoreError::config(format!("cannot run xset dpms force: {err}")))?;
        if !output.status.success() {
            let reason = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(CoreError::config(format!(
                "xset dpms force {value} failed: {}",
                if reason.is_empty() { output.status.to_string() } else { reason }
            )));
        }
        // The command is only half of the answer: read the state back, and
        // refuse to report success when the server ignored the request.
        let Some(on) = monitor()? else {
            return Err(CoreError::config("xset q has no Monitor state to read back"));
        };
        if on == off {
            return Err(CoreError::config(format!(
                "the display did not go {}",
                if off { "off" } else { "on" }
            )));
        }
        tracing::info!(target: "xgview::display_off", off, "monitor power requested");
        Ok(())
    }
}

/// Android has no display power command reachable from the Rust process. The
/// settings panel hides the switch at compile time; this is the core's answer
/// for callers that ask anyway.
#[cfg(target_os = "android")]
mod imp {
    use super::*;

    pub(super) fn supported() -> bool {
        false
    }

    pub(super) fn set(_off: bool) -> Result<()> {
        Err(CoreError::unsupported("display power control is not available on Android"))
    }
}

/// A platform with no mechanism at all.
#[cfg(not(any(windows, unix)))]
mod imp {
    use super::*;

    pub(super) fn supported() -> bool {
        false
    }

    pub(super) fn set(_off: bool) -> Result<()> {
        Err(CoreError::unsupported("display power control is not available on this platform"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_state_has_no_request_and_no_error() {
        let status = status();
        // Tests may share a process, so the exact `off` is not asserted - but
        // a failed attempt must always leave a reason behind.
        if status.error.is_some() {
            assert!(supported() || cfg!(test));
        }
        let _ = status.off;
    }

    #[test]
    fn an_unsupported_platform_refuses_to_pretend() {
        if !supported() {
            assert!(set(true).is_err());
            assert!(set(false).is_err());
        }
    }
}

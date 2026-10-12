//! Where the window sits among the others.
//!
//! A monitoring wall is often the only thing on a screen, and a full screen one
//! that something can cover - a notification, a panel, the browser the viewer
//! just asked for - is not doing its job. So the window is asked to stay on top:
//! [`level`] is the whole decision, and [`OnTop`] is the two moments where the
//! app gives that up on purpose.
//!
//! What can be asked for is not the same everywhere. Windows, macOS and most X11
//! window managers keep a window above the others; a Wayland session cannot -
//! winit documents the window level as unsupported there - Android has nothing to
//! keep it above, and an XFCE session cannot have it at all, because xfwm4 refuses
//! `_NET_WM_STATE_ABOVE` on a full screen window and clears the state when one
//! goes full screen: exactly the state this is for. [`supported`] answers that
//! from the session's own variables, and the settings panel greys the switch out
//! where it answers `false`, the treatment `monitor_core::power` gives its own
//! switches: one that silently does nothing is worse than one the panel will not
//! offer.

use std::sync::OnceLock;
use std::time::Instant;

use egui::WindowLevel;

/// What the window's place among the others is doing, apart from the setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OnTop {
    /// Nothing to say: the setting decides.
    #[default]
    Configured,
    /// Taken off the top until the code that did it puts it back. A native file
    /// dialog is the case - the call that opens it returns when the dialog
    /// closes, so its end is known here rather than by a clock.
    Suspended,
    /// Taken off the top until a moment. A browser window is the case: nothing
    /// tells us when it appears, so a clock is the only end that can be named.
    SuspendedUntil(Instant),
}

/// Whether this platform can hold a window above the others at all.
///
/// Asked once and remembered, because the settings panel reaches it every frame
/// and the answer reads the environment.
pub fn supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(detect)
}

/// [`supported`], asked of the platform.
fn detect() -> bool {
    #[cfg(any(windows, target_os = "macos"))]
    {
        true
    }
    #[cfg(target_os = "android")]
    {
        false
    }
    // Everywhere else that is unix - in practice Linux, where the answer is
    // whether the session is X11 and runs a window manager that will hold a full
    // screen window above the others.
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
    {
        !wayland_session() && !xfce_session()
    }
    #[cfg(not(any(windows, unix)))]
    {
        false
    }
}

/// Whether this is a Wayland session, by the rule winit uses to pick a backend.
///
/// Wayland wins whenever `WAYLAND_DISPLAY` or `WAYLAND_SOCKET` is set and not
/// empty, `DISPLAY` or not: a Wayland session that also has XWayland still runs
/// on Wayland, where the window level is not implemented. Mirroring winit's rule
/// rather than looking for an X server is what keeps this answer matching what
/// will actually happen.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
fn wayland_session() -> bool {
    ["WAYLAND_DISPLAY", "WAYLAND_SOCKET"]
        .iter()
        .any(|name| std::env::var(name).is_ok_and(|value| !value.is_empty()))
}

/// Whether this is an XFCE session, whose window manager will not keep a full
/// screen window above the others.
///
/// xfwm4 refuses `_NET_WM_STATE_ABOVE` for a full screen window, and clears the
/// state when one goes full screen - which is the one moment the switch is about.
/// Verified against xfwm4 4.18: the same request is honoured while the window is
/// windowed and refused while it is full screen, for winit and for an outside
/// `wmctrl` alike, while Openbox honours it in both. There is nothing the viewer
/// can do about it from here, so the switch is not offered rather than offered
/// and ignored.
///
/// Asked of the session's variables, because the window manager itself cannot be
/// asked without an X connection this crate does not open: XFCE is what runs
/// xfwm4, and a session that names none of these is taken at its word.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
fn xfce_session() -> bool {
    ["XDG_CURRENT_DESKTOP", "XDG_SESSION_DESKTOP", "DESKTOP_SESSION"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .any(|value| value.to_ascii_lowercase().contains("xfce"))
}

/// The window level the current state asks for.
///
/// A wall stays on top only in full screen - a windowed one would sit over the
/// desktop and everything on it for no reason - and only where the platform can
/// do it at all, and not while something the viewer opened is coming up in front
/// of it.
pub fn level(
    supported: bool,
    configured: bool,
    fullscreen: bool,
    on_top: OnTop,
    now: Instant,
) -> WindowLevel {
    let held_off = match on_top {
        OnTop::Configured => false,
        OnTop::Suspended => true,
        OnTop::SuspendedUntil(until) => now < until,
    };
    if supported && configured && fullscreen && !held_off {
        WindowLevel::AlwaysOnTop
    } else {
        WindowLevel::Normal
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// The decision reached with everything left at its usual setting.
    fn asked_for(supported: bool, configured: bool, fullscreen: bool, on_top: OnTop) -> WindowLevel {
        level(supported, configured, fullscreen, on_top, Instant::now())
    }

    #[test]
    fn a_full_screen_wall_is_held_on_top_when_it_is_asked_to_be() {
        assert_eq!(asked_for(true, true, true, OnTop::Configured), WindowLevel::AlwaysOnTop);
    }

    #[test]
    fn a_windowed_wall_is_never_held_on_top() {
        // The option is about the wall that has the screen to itself; a windowed
        // one sitting over the desktop is not what it is for.
        assert_eq!(asked_for(true, true, false, OnTop::Configured), WindowLevel::Normal);
    }

    #[test]
    fn nothing_is_asked_of_a_platform_that_cannot_do_it() {
        assert_eq!(asked_for(false, true, true, OnTop::Configured), WindowLevel::Normal);
    }

    #[test]
    fn an_unchecked_option_asks_for_nothing() {
        assert_eq!(asked_for(true, false, true, OnTop::Configured), WindowLevel::Normal);
    }

    #[test]
    fn a_suspension_lets_what_was_opened_come_up_in_front() {
        let until = Instant::now() + Duration::from_secs(60);
        assert_eq!(asked_for(true, true, true, OnTop::Suspended), WindowLevel::Normal);
        assert_eq!(asked_for(true, true, true, OnTop::SuspendedUntil(until)), WindowLevel::Normal);
    }

    #[test]
    fn a_suspension_with_a_clock_ends() {
        let now = Instant::now();
        let until = now + Duration::from_secs(3);
        assert_eq!(level(true, true, true, OnTop::SuspendedUntil(until), now), WindowLevel::Normal);
        assert_eq!(
            level(true, true, true, OnTop::SuspendedUntil(until), until),
            WindowLevel::AlwaysOnTop,
            "the moment the clock names is the moment the wall is back on top"
        );
    }
}

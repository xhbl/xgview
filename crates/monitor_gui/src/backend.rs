//! What the Android backend markers say a launch should do.
//!
//! Three inputs - whether a previous launch left the marker that says it never
//! reached the renderer, whether the device's Vulkan version put it behind the
//! gate, and whether a `force-vulkan` file asks for Vulkan anyway - and what
//! follows from them: which backend to build, which marker to write or drop,
//! and what a caught panic is still allowed to do.
//!
//! It lives here rather than in `run_android` so the table can be exercised on
//! the host. Every wrong shape this mechanism has had (see DevMemo §19: a
//! switch that overrode the one marker it must not, and a restart with no
//! backend to come back on) was a wrong cell in the table rather than a wrong
//! device, and only a device could run the old code.
//!
//! `monitor_android` and this crate's own Android build are the only callers,
//! so the module is compiled for them and for the host's tests.

/// Why a launch is being sent to the GL backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlReason {
    /// The device has no Vulkan 1.1, or no Vulkan at all: the gate marker.
    Gate,
    /// A previous launch did not reach the renderer: the attempt marker.
    PreviousAttempt,
}

/// The backend a launch should ask wgpu for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Vulkan,
    Gl(GlReason),
}

impl Backend {
    /// Whether this launch is a Vulkan attempt.
    ///
    /// That is what the attempt marker claims, so it is written before such a
    /// launch and cleared by one that reaches the renderer - and never touched
    /// by a GL launch, which would only make the marker a lie.
    pub fn tries_vulkan(self) -> bool {
        matches!(self, Backend::Vulkan)
    }
}

/// What the three markers say a launch should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    /// The backend to build.
    pub backend: Backend,
    /// Drop the attempt marker: the gate makes an older attempt meaningless, and
    /// leaving it would have the log name the wrong reason for ever after.
    pub clear_attempt_marker: bool,
    /// A `force-vulkan` file is overriding the gate, which the log should say.
    pub forced_past_gate: bool,
}

/// The launch's backend, the marker to drop, and the warning to log.
///
/// `attempt_aborted` is the attempt marker's presence, `gated` the gate's, and
/// `forced` the `force-vulkan` file's.
pub fn decide(attempt_aborted: bool, gated: bool, forced: bool) -> Decision {
    // The attempt marker outranks the gate and the switch both. The launch it
    // was written by is the one that may not come back, so the launch after it
    // has to fall back rather than force its way into the same failure - and
    // the restart a caught panic asks for is itself a launch like that.
    let use_gl = attempt_aborted || (gated && !forced);
    let backend = if !use_gl {
        Backend::Vulkan
    } else if gated {
        // The standing reason first: it explains the launch whether or not an
        // attempt happened to fail underneath it.
        Backend::Gl(GlReason::Gate)
    } else {
        Backend::Gl(GlReason::PreviousAttempt)
    };
    Decision {
        backend,
        clear_attempt_marker: gated && !forced && attempt_aborted,
        forced_past_gate: forced && gated && !attempt_aborted,
    }
}

/// What a caught panic leaves to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanicAction {
    /// The renderer was up, so the backend is not what broke: bring the process
    /// back and leave the markers alone.
    Restart,
    /// A launch that could not draw with no backend left to try. Leaving the
    /// markers alone and staying down beats replaying the same panic for ever.
    GiveUp,
    /// A launch that could not draw on Vulkan: mark it, and come back on GL.
    MarkAndRestart,
}

/// What to do about a panic, given how far the launch got before it.
///
/// `renderer_up` is the flag set once the render pipeline exists, which is what
/// separates a launch that could not draw from a run-time failure hours later;
/// `on_gl` says the fallback backend was the one being built.
pub fn panic_action(renderer_up: bool, on_gl: bool) -> PanicAction {
    if renderer_up {
        PanicAction::Restart
    } else if on_gl {
        PanicAction::GiveUp
    } else {
        PanicAction::MarkAndRestart
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every cell of the gate table: attempt marker × version gate × switch.
    ///
    /// Written out rather than derived, because reading the table is the point
    /// - the bugs here were cells nobody had looked at.
    #[test]
    fn the_gate_table() {
        // (attempt, gate, forced) -> (backend, clear, warn)
        let table = [
            // A healthy device with no markers and no switch: Vulkan.
            (false, false, false, Backend::Vulkan, false, false),
            // The switch on a healthy device changes nothing.
            (false, false, true, Backend::Vulkan, false, false),
            // The gate alone: GL, and no marker to clear - nothing was tried.
            (false, true, false, Backend::Gl(GlReason::Gate), false, false),
            // The switch over the gate: Vulkan, and the log says why.
            (false, true, true, Backend::Vulkan, false, true),
            // A failed attempt on a device the gate says nothing about: GL,
            // and the marker stays - it is what keeps the next launch off
            // Vulkan.
            (true, false, false, Backend::Gl(GlReason::PreviousAttempt), false, false),
            // A failed attempt with the switch on: the attempt still wins.
            // This is the launch the caught panic restarts onto, and forcing
            // Vulkan again would replay the panic.
            (true, false, true, Backend::Gl(GlReason::PreviousAttempt), false, false),
            // Both markers: the gate is the standing reason, so it names the
            // backend and the stale attempt marker is dropped.
            (true, true, false, Backend::Gl(GlReason::Gate), true, false),
            // Both markers and the switch: the gate is overridden, but the
            // failed attempt is not, so the marker stays and Vulkan is not
            // tried - the switch only ever overrides the gate.
            (true, true, true, Backend::Gl(GlReason::Gate), false, false),
        ];

        for (attempt, gate, forced, backend, clear, warn) in table {
            let where_ = format!("attempt={attempt} gate={gate} forced={forced}");
            let decision = decide(attempt, gate, forced);
            assert_eq!(decision.backend, backend, "backend, {where_}");
            assert_eq!(decision.clear_attempt_marker, clear, "clear, {where_}");
            assert_eq!(decision.forced_past_gate, warn, "warn, {where_}");
        }
    }

    /// Only a launch that is about to try Vulkan owns the attempt marker. A GL
    /// launch that wrote it would pin the device to GL, which is the whole
    /// failure this table exists to prevent.
    #[test]
    fn only_a_vulkan_launch_owns_the_attempt_marker() {
        assert!(decide(false, false, false).backend.tries_vulkan());
        assert!(decide(false, true, true).backend.tries_vulkan());
        assert!(!decide(false, true, false).backend.tries_vulkan());
        assert!(!decide(true, false, false).backend.tries_vulkan());
        assert!(!decide(true, false, true).backend.tries_vulkan());
    }

    /// Every cell of the panic table: how far the launch got × the backend.
    #[test]
    fn the_panic_table() {
        // Up already: the backend has plainly worked, whatever it was.
        assert_eq!(panic_action(true, false), PanicAction::Restart);
        assert_eq!(panic_action(true, true), PanicAction::Restart);
        // Never up on Vulkan: mark it and come back on the fallback.
        assert_eq!(panic_action(false, false), PanicAction::MarkAndRestart);
        // Never up on the fallback either: there is nothing left to try, so
        // restarting would only replay it.
        assert_eq!(panic_action(false, true), PanicAction::GiveUp);
    }

    /// The two tables together, over every cell: a panic before the renderer
    /// came up is never answered with a restart onto the backend that is
    /// already GL - there is nothing left to try after it, and this is the
    /// invariant that catches a fallback which restarts itself for ever.
    #[test]
    fn no_restart_onto_a_backend_that_is_already_gl() {
        for attempt in [false, true] {
            for gate in [false, true] {
                for forced in [false, true] {
                    let on_gl = !decide(attempt, gate, forced).backend.tries_vulkan();
                    let action = panic_action(false, on_gl);
                    assert_eq!(
                        action,
                        if on_gl { PanicAction::GiveUp } else { PanicAction::MarkAndRestart },
                        "attempt={attempt} gate={gate} forced={forced}"
                    );
                }
            }
        }
    }
}

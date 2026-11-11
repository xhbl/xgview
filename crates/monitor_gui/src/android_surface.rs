//! The wall on a `SurfaceView`, without eframe and without `android-activity`.
//!
//! The reason is in `priv/docs/android-surface-plan.md`: a `NativeActivity` gives
//! its whole window to native drawing, and that makes both of the things this
//! app needs impossible - a view above the picture, and a compositor layer below
//! it. An ordinary activity with `SurfaceView`s has neither problem, but it also
//! hands out no event loop and no window: this module is that loop.
//!
//! What it reproduces from eframe is small, because the app only ever used two
//! things from it - the egui context and the wgpu state - and never the frame
//! handle. Input is the activity's to hand over: keys, one event per touch
//! pointer, and a wheel, pushed from the UI thread into the next frame.
//!
//! Reached from `SurfaceActivity` through `monitor_android`'s JNI entries.

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::android::SurfaceTicket;
use crate::app::XgViewApp;
use crate::backend;
use crate::RunOptions;
use monitor_core::config::AppConfig;

/// Events the activity has handed over, waiting for the next frame.
///
/// The activity pushes from the UI thread and the loop drains at the top of each
/// frame. A mutex rather than a channel because the loop must never block: a
/// frame that waits for input is a frame that stops drawing.
static PENDING: Mutex<Vec<egui::Event>> = Mutex::new(Vec::new());

/// An urgent request for a frame, and the thing that parks the loop until one
/// is due. The loop waits for egui's own `repaint_delay`, which is half a second
/// when the wall is idle; a key, a touch or a message that arrives in the middle
/// of that wait cannot wait for the timer to run out.
static WAKE: OnceLock<(Mutex<bool>, Condvar)> = OnceLock::new();

fn wake_pair() -> &'static (Mutex<bool>, Condvar) {
    WAKE.get_or_init(|| (Mutex::new(false), Condvar::new()))
}

/// Cuts short the wait for the next frame.
///
/// Called from the UI thread (an input event, a new surface, a changed inset)
/// and from the soft keyboard's queue. A timed repaint request is not urgent:
/// its delay is in `repaint_delay`, which the loop reads at the end of the
/// frame it was made in.
pub fn wake() {
    let (lock, cvar) = wake_pair();
    if let Ok(mut urgent) = lock.lock() {
        *urgent = true;
    }
    cvar.notify_all();
}

/// Whether the activity's window has focus, as last reported.
static FOCUSED: AtomicBool = AtomicBool::new(true);

/// The surface's size in pixels, as last reported.
static WIDTH: AtomicU32 = AtomicU32::new(1920);
static HEIGHT: AtomicU32 = AtomicU32::new(1080);

/// The display's density, so the interface is laid out at the size it should be.
static DENSITY: AtomicU32 = AtomicU32::new(2000);

/// The window the loop should be drawing into, or null while there is none.
///
/// The activity's `SurfaceView` hands a surface over, loses it, and gets another
/// one - a relayout is enough - so the loop is told which window is current and
/// attaches or detaches to match, rather than being started and stopped. The app
/// itself is built once and survives all of it.
static WINDOW: std::sync::atomic::AtomicPtr<c_void> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// Which activity's view `WINDOW` belongs to; see [`crate::android::SurfaceTicket`].
///
/// Two activities of this process can be alive at once, and a `surfaceDestroyed`
/// from the one that is going must not detach the other's wall. Every hand-over
/// takes a number, and the resize and the stop that follow carry it.
static WALL_TICKET: SurfaceTicket = SurfaceTicket::new();

/// The same, for the bar's surface.
static BAR_TICKET: SurfaceTicket = SurfaceTicket::new();

/// Whether the drawing thread has been started. There is one for the process.
static STARTED: AtomicBool = AtomicBool::new(false);

/// Which backend the launch picked, and whether a frame ever reached the screen.
///
/// Read after an unwind or a failed start, when everything `run` held is gone:
/// `backend::panic_action` needs both to say whether a fresh process is worth
/// having. Set once each, and never cleared - a process serves one launch.
static ON_GL: AtomicBool = AtomicBool::new(false);
static DREW: AtomicBool = AtomicBool::new(false);

/// Whether the overlay's surface can only be opaque.
///
/// An overlay that cannot be seen through would paint a black rectangle over
/// the picture, and nothing below it could draw over that. On such a device the
/// codec is told to give its frames back to the wall, which draws everything
/// itself - slower, but a picture rather than a black screen. See where the
/// surface path is suspended in `run`.
static OPAQUE_ONLY: AtomicBool = AtomicBool::new(false);

/// The window of the bar's own surface, and its size in pixels.
///
/// A second surface rather than part of the wall's: the bar has to sit *over* a
/// decoded picture, and nothing here can be see-through - so it is given a
/// rectangle of its own, on top of the picture, and draws an opaque bar in it.
/// Covering the picture under the bar is the point, not a cost.
static BAR_WINDOW: std::sync::atomic::AtomicPtr<c_void> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
static BAR_WIDTH: AtomicU32 = AtomicU32::new(1920);
static BAR_HEIGHT: AtomicU32 = AtomicU32::new(140);

/// Takes the surface the bar is drawn into and answers its identity.
///
/// A null window is refused: a view with no surface has nothing to identify,
/// and the activity keeps the token from the last one it did get.
pub fn start_bar(window: *mut c_void, width: i32, height: i32) -> u64 {
    if window.is_null() {
        return 0;
    }
    let token = BAR_TICKET.take();
    attach_bar(window, width, height);
    token
}

/// The bar's surface went away, and only this identity of it.
pub fn stop_bar(token: u64) {
    if BAR_TICKET.retire(token) {
        attach_bar(std::ptr::null_mut(), 0, 0);
    }
}

fn attach_bar(window: *mut c_void, width: i32, height: i32) {
    if width > 0 {
        BAR_WIDTH.store(width as u32, Ordering::Release);
    }
    if height > 0 {
        BAR_HEIGHT.store(height as u32, Ordering::Release);
    }
    BAR_WINDOW.store(window, Ordering::Release);
    wake();
}

/// The two directories the activity handed over, kept for the one thread.
static DIRECTORIES: Mutex<Option<(PathBuf, PathBuf)>> = Mutex::new(None);

/// Hands one input event to the next frame.
pub fn push_event(event: egui::Event) {
    if let Ok(mut pending) = PENDING.lock() {
        // Bounded: a frame that has stopped being drawn must not grow a queue
        // nobody reads. The number is generous - a second of held keys.
        if pending.len() < 256 {
            pending.push(event);
        }
    }
    wake();
}

/// Records whether the window has focus.
pub fn set_focused(focused: bool) {
    FOCUSED.store(focused, Ordering::Release);
    wake();
}

/// Translates one Android key and hands its events to the next frame.
///
/// `meta` is `KeyEvent.getMetaState()` and `unicode` is
/// `KeyEvent.getUnicodeChar()`. The translation - both the control and the
/// text a key types - is [`crate::keymap::key_events`], where it can be tested
/// without a device; this is only the queue it lands in.
///
/// Returns whether the interface took it. One it did not is one the system
/// should still see - volume, the media keys, the language switch - and the
/// activity answers the key event with this, so that it does.
pub fn push_key(code: i32, pressed: bool, repeat: bool, meta: i32, unicode: i32) -> bool {
    let Some(events) = crate::keymap::key_events(code, pressed, repeat, meta, unicode) else {
        return false;
    };
    for event in events {
        push_event(event);
    }
    true
}

/// How many fingers are on the glass, so a tap and a pinch can be told apart.
static FINGERS: AtomicU32 = AtomicU32::new(0);

/// Translates one Android touch pointer and hands it to the next frame.
///
/// `phase` is the pointer's own phase rather than the event's action, because a
/// `ACTION_MOVE` carries every finger that is down: the activity sends one call
/// per pointer, so what arrives here is already per-finger.
///
/// Two kinds of event come out of one finger. The `Touch` is what a pinch is
/// read from - egui takes the ratio between two of them for itself - and the
/// pointer events are what a tap is read from: the wall's `track_tap` looks at
/// the primary button, so a finger that never moved the pointer would be a
/// picture nothing could be pressed on. With a second finger down the pointer
/// is let go and only the touches continue, or the two would fight over where
/// the pointer is.
pub fn push_touch(phase: i32, id: i32, x: f32, y: f32, force: f32) {
    let phase = match phase {
        0 => egui::TouchPhase::Start,
        1 => egui::TouchPhase::Move,
        2 => egui::TouchPhase::End,
        _ => egui::TouchPhase::Cancel,
    };
    let pixels_per_point = DENSITY.load(Ordering::Acquire) as f32 / 1000.0;
    let pos = egui::pos2(x / pixels_per_point, y / pixels_per_point);
    push_event(egui::Event::Touch {
        device_id: egui::TouchDeviceId(0),
        id: egui::TouchId(id as u64),
        phase,
        pos,
        force: Some(force),
    });

    let before = FINGERS.load(Ordering::Acquire);
    let after = match phase {
        egui::TouchPhase::Start => before.saturating_add(1),
        egui::TouchPhase::End | egui::TouchPhase::Cancel => before.saturating_sub(1),
        egui::TouchPhase::Move => before,
    };
    FINGERS.store(after, Ordering::Release);

    let modifiers = egui::Modifiers::default();
    let button = |pressed: bool| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers,
    };
    if before <= 1 {
        // One finger, and it is the only one - or it is the one being lifted.
        // The lift is the reason this cannot ask for `after == 1`: a finger
        // coming up leaves none behind, so `after` is 0, and the release below
        // was the one event that never arrived. A wall told nothing is a wall
        // that still has the pointer down: it keeps the page where the drag
        // left it - half a swipe, with the outgoing page showing on one side -
        // and it answers no further press, because the finger it believes is
        // on the glass is still there. That is exactly what a swipe did on a
        // phone, and it looked like touch had stopped working.
        match phase {
            egui::TouchPhase::Start => {
                push_event(egui::Event::PointerMoved(pos));
                push_event(button(true));
            }
            egui::TouchPhase::Move => push_event(egui::Event::PointerMoved(pos)),
            egui::TouchPhase::End | egui::TouchPhase::Cancel => {
                push_event(button(false));
                // A finger that lifts takes the pointer with it, and egui is
                // told so by the platform layer it was written for. Without
                // this the wall keeps the last position hovered: the control
                // under it stays lit, its tooltip stays up, and a lit row in a
                // picker stays lit, until the next touch somewhere else. Only
                // when nothing is left on the glass - a second finger still
                // down has a position of its own to report.
                if after == 0 {
                    push_event(egui::Event::PointerGone);
                }
            }
        }
    } else if before == 1 && after == 2 {
        // A second finger arrived: the press is no longer a tap but half a
        // pinch, so the pointer is let go where it stands.
        push_event(button(false));
    }
}

/// Hands over a wheel or trackpad scroll, in pixels of travel.
pub fn push_scroll(dx: f32, dy: f32) {
    if dx == 0.0 && dy == 0.0 {
        return;
    }
    let pixels_per_point = DENSITY.load(Ordering::Acquire) as f32 / 1000.0;
    push_event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(dx / pixels_per_point, dy / pixels_per_point),
        modifiers: egui::Modifiers::default(),
    });
}

/// Records the surface's new size, in pixels, if its identity is still up.
pub fn resize(token: u64, width: i32, height: i32) {
    if !WALL_TICKET.holds(token) {
        return;
    }
    if width > 0 {
        WIDTH.store(width as u32, Ordering::Release);
    }
    if height > 0 {
        HEIGHT.store(height as u32, Ordering::Release);
    }
    wake();
}

/// Detaches the loop from its surface, if its identity is still up.
///
/// The app keeps running: a surface that goes away is usually a relayout on its
/// way back, and rebuilding the wall - its channels, its runtime, its audio -
/// for that would drop every camera for a second.
///
/// The token is what makes this safe to call twice, and safe to call from an
/// activity that is already gone: `surfaceDestroyed` and `onDestroy` both stop
/// the wall, and the old activity's `onDestroy` arrives after the new one has
/// started its own. See [`SurfaceTicket`].
pub fn stop(token: u64) {
    if !WALL_TICKET.retire(token) {
        return;
    }
    WINDOW.store(std::ptr::null_mut(), Ordering::Release);
    wake();
}

/// Runs one future to completion on a runtime of its own.
fn block_on<F: std::future::Future>(future: F) -> Option<F::Output> {
    let runtime = tokio::runtime::Builder::new_current_thread().build().ok()?;
    Some(runtime.block_on(future))
}

/// A fresh `egui::Context` for the bar's own surface: the same theme, the same
/// faces, the same pixels per point as the wall's, on a different screen.
///
/// One function because the context and the renderer that draws it are one pair
/// - see where the bar's surface is made in the loop.
fn bar_screen(pixels_per_point: f32) -> egui::Context {
    let context = egui::Context::default();
    context.set_pixels_per_point(pixels_per_point);
    crate::theme::install(&context);
    crate::fonts::install_for(&context, &monitor_i18n::current_language());
    context
}

/// This launch's hold on the Vulkan attempt marker, and the backend it chose.
///
/// The same mechanism `run_android` runs, and for the same reason: some old
/// Vulkan drivers - Adreno on Android 8.x - hand out a device and then lose it
/// while the first pipeline is built, which aborts the process before anything
/// is drawn. Nothing can be asked about that in advance, so the attempt is the
/// test: the marker is written before Vulkan is used, survives such an abort,
/// and sends the next launch to GL.
///
/// Owned rather than written at the top and cleared at the bottom, because this
/// loop has many ways out - a surface that cannot be configured, an adapter
/// that cannot be had - and the marker's whole value is that a launch which did
/// not draw left it behind. `Drop` is the only place every one of those paths
/// passes through.
///
/// The other half is the rule [`backend::failed_start_clears_marker`] carries: a
/// launch that the marker itself sent to GL and which then failed would pin the
/// device to a backend that cannot serve it - the SHIELD's GL cannot start at
/// all - so that marker goes with it.
struct Attempt {
    /// Where the marker lives, when the configuration directory is known.
    path: Option<PathBuf>,
    /// What this launch is drawing with; see [`backend::decide`].
    backend: backend::Backend,
    /// The marker has been dealt with: kept for a failed attempt, or taken away
    /// by a frame that got through.
    done: bool,
}

impl Attempt {
    /// Reads the three markers, writes or drops what this launch owns, and says
    /// which backend to build.
    ///
    /// `gated` is the file `MainActivity.applyVulkanGate` wrote before the
    /// native thread started, and `forced` the `force-vulkan` file a transfer
    /// can put in the shared directory.
    fn claim(marker: Option<PathBuf>, gated: bool, forced: bool) -> Self {
        let decision = backend::decide(
            marker.as_deref().is_some_and(|path| path.exists()),
            gated,
            forced,
        );

        // The gate is a reason of its own, and one that does not go away, so an
        // attempt marker sitting under it says nothing true. Dropping it keeps
        // the log honest and leaves a forced run repeatable.
        if decision.clear_attempt_marker {
            if let Some(path) = &marker {
                let _ = std::fs::remove_file(path);
                tracing::info!(
                    target: "xgview",
                    "the version gate supersedes the previous attempt; its marker is cleared"
                );
            }
        }
        if decision.forced_past_gate {
            tracing::warn!(
                target: "xgview",
                "force-vulkan is present; trying Vulkan although this device has no Vulkan 1.1"
            );
        }
        match decision.backend {
            backend::Backend::Gl(reason) => match reason {
                backend::GlReason::Gate => tracing::warn!(
                    target: "xgview",
                    "this device has no Vulkan 1.1; using the GL backend"
                ),
                backend::GlReason::PreviousAttempt => tracing::warn!(
                    target: "xgview",
                    "the previous launch did not reach its first frame; using the GL backend"
                ),
            },
            backend::Backend::Vulkan => {
                if let Some(path) = &marker {
                    if let Some(dir) = path.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let _ = std::fs::write(path, b"");
                    tracing::info!(
                        target: "xgview",
                        "marked this launch as a Vulkan attempt; the mark is cleared once the wall draws"
                    );
                }
            }
        }
        Attempt { path: marker, backend: decision.backend, done: false }
    }

    /// The backend to build the wgpu instance with.
    fn backend(&self) -> backend::Backend {
        self.backend
    }

    /// A frame got through: this launch is not a failed start, whatever it is
    /// drawing with.
    ///
    /// A Vulkan attempt that reached a frame did not fail, so its marker has
    /// done its job and goes - waiting for the process to end instead would
    /// leave it behind for a run that drew perfectly well. A GL launch never
    /// wrote one; the marker it was sent here by is the right answer for this
    /// device and stays, which is what `done` says to `Drop`.
    fn settled(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        if !self.backend.tries_vulkan() {
            return;
        }
        if let Some(path) = self.path.as_deref().filter(|path| path.exists()) {
            let _ = std::fs::remove_file(path);
            tracing::info!(
                target: "xgview",
                "the wall is drawing on Vulkan; its attempt marker is cleared"
            );
        }
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        // Left alone for a Vulkan attempt: the marker is what sends the next
        // launch to GL, and this launch is the evidence that it should.
        if self.done || !backend::failed_start_clears_marker(self.backend) {
            return;
        }
        if let Some(path) = self.path.as_deref().filter(|path| path.exists()) {
            let _ = std::fs::remove_file(path);
            tracing::warn!(
                target: "xgview",
                "the GL fallback could not start either; the next launch tries Vulkan again"
            );
        }
    }
}

/// Starts the wall on `window`, on a thread of its own.
///
/// `internal` and `external` are the two directories the activity hands out: the
/// private one the configuration lives in, and the shared one a transfer can put
/// a language pack in. They come from Java because there is no `AndroidApp` here
/// to ask.
/// Hands back the identity of the surface, which the activity keeps for its
/// later resize and stop. A null window answers `0`: nobody's.
pub fn start(window: *mut c_void, internal: String, external: String, density: f32) -> u64 {
    if window.is_null() {
        return 0;
    }
    if density > 0.0 {
        DENSITY.store((density * 1000.0) as u32, Ordering::Release);
    }
    if let Ok(mut directories) = DIRECTORIES.lock() {
        *directories = Some((PathBuf::from(internal), PathBuf::from(external)));
    }
    let token = WALL_TICKET.take();
    WINDOW.store(window, Ordering::Release);
    if !STARTED.swap(true, Ordering::AcqRel) {
        std::thread::spawn(|| {
            // A failure here does not take the activity with it: this thread
            // ends, the surfaces stay on screen, and nothing draws - a black
            // wall nobody can press. The older front end never showed that,
            // because returning from `android_main` finishes its activity.
            //
            // So both ways out are answered the way `run_android` answers them:
            // a lost launch goes to a fresh process (`restart_soon`, which the
            // activity refuses once it has seen too many), and one with no
            // backend left to come back on is left down rather than replayed.
            // `Attempt` has already dealt with the marker by the time this
            // runs - a Vulkan attempt that died leaves it for the next launch,
            // a GL one it chose takes it away on the way out.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
            let message = match &outcome {
                Ok(Ok(())) => return,
                Ok(Err(error)) => error.to_string(),
                Err(payload) => crate::panic_message(payload.as_ref()),
            };
            let drew = DREW.load(Ordering::Relaxed);
            let on_gl = ON_GL.load(Ordering::Relaxed);
            tracing::error!(target: "xgview", %message, drew, on_gl, "the wall stopped");
            match backend::panic_action(drew, on_gl) {
                backend::PanicAction::GiveUp => tracing::error!(
                    target: "xgview",
                    "the GL fallback could not start either, and there is nothing left to try; the wall stays down"
                ),
                backend::PanicAction::Restart => {
                    if !crate::android::restart_soon() {
                        tracing::warn!(
                            target: "xgview",
                            "no restart was scheduled; the wall stays down until the next launch"
                        );
                    }
                }
                backend::PanicAction::MarkAndRestart => {
                    if !crate::android::restart_soon() {
                        tracing::warn!(
                            target: "xgview",
                            "no restart was scheduled; the next launch reads the marker and uses GL"
                        );
                    }
                }
            }
            // The process goes either way: one whose renderer has failed has
            // nothing left to serve, and the alarm `restart_soon` set is held
            // by the system, so it survives this exit. See `run_android`.
            crate::exit_now();
        });
    }
    wake();
    token
}

fn run() -> anyhow::Result<()> {
    crate::init_android_logging();
    crate::install_panic_hook();

    let Some((internal, external)) = DIRECTORIES.lock().ok().and_then(|d| d.clone()) else {
        anyhow::bail!("the activity never handed its directories over");
    };
    // The same two directories `run_android` installs, and for the same reasons:
    // the private one is the only writable place guaranteed to exist, and the
    // shared one is where a transfer can drop a language pack.
    monitor_core::config::set_config_home(internal);
    crate::android::set_external_files(external.clone());
    monitor_codec::dump::set_dir(external.clone());

    let runtime = crate::build_runtime()?;
    let handle = runtime.handle().clone();

    let config_path = AppConfig::default_path();
    let config = AppConfig::load_or_default(&config_path);
    // Full screen unless the viewer asked for the wall's own bars to stay up
    // the last time; see `AppConfig::android_pin_bars`. The window is the
    // activity's and stays full screen either way - the system's bars are never
    // shown - so this decides only whether the wall's bars may hide when idle.
    let fullscreen = !config.android_pin_bars;
    let options = RunOptions {
        config,
        config_path: Some(config_path.clone()),
        fullscreen,
        from_autostart: false,
        supervised: false,
        extra_lang_dir: Some(external.join("langs")),
    };

    // Held for the life of this loop: it writes the marker before Vulkan is
    // tried, takes it away on the first frame that gets through, and leaves it
    // for the next launch if this one never draws. See `Attempt`.
    let mut attempt = Attempt::claim(
        crate::vulkan_attempt_marker(),
        crate::vulkan_gate_marker().is_some_and(|path| path.exists()),
        crate::force_vulkan_in(&external),
    );
    // For the thread that owns this one: a panic unwinds out of `run` with its
    // locals, and the answer to it depends on where the launch got to.
    ON_GL.store(!attempt.backend().tries_vulkan(), Ordering::Relaxed);

    let instance = {
        // Which backend, from the same three markers the older front end reads:
        // the gate the activity wrote before this thread started, an attempt
        // marker a launch that never drew left behind, and the `force-vulkan`
        // file. Picked before the instance is built because an instance cannot
        // be asked for another backend afterwards. See `Attempt` and `backend`.
        let mut descriptor = wgpu::InstanceDescriptor::default();
        if !attempt.backend().tries_vulkan() {
            // Asked for in the descriptor rather than through `WGPU_BACKEND`:
            // setting that means `std::env::set_var`, which the standard
            // library does not promise is safe once the process has threads.
            descriptor.backends = wgpu::Backends::GL;
        }
        wgpu::Instance::new(&descriptor)
    };
    // Everything that outlives a surface. The activity's `SurfaceView` hands one
    // over, loses it, and gets another; the wall, its device and its textures
    // stay put across all of it.
    let mut built: Option<(egui::Context, egui_wgpu::RenderState, XgViewApp)> = None;
    let mut surface: Option<(wgpu::Surface<'_>, wgpu::SurfaceConfiguration)> = None;
    let mut attached: *mut c_void = std::ptr::null_mut();
    // The bar's own swapchain, over the picture - and its own renderer, because
    // two `egui::Context`s share nothing: each numbers its textures from
    // `Managed(0)`, so the font atlas of one would land on the other's.
    let mut bar: Option<(wgpu::Surface<'_>, wgpu::SurfaceConfiguration, egui_wgpu::Renderer)> =
        None;
    let mut bar_attached: *mut c_void = std::ptr::null_mut();
    let mut last_focus: Option<egui::Id> = None;
    // What the strip's frame was last told about the pointer. It is carried
    // across because the strip paints and does not read: the wall's context is
    // the one the events arrive in, and without a copy of the pointer the
    // controls there are drawn idle however hard they are being pressed.
    let mut last_pointer: Option<egui::Pos2> = None;
    let mut last_pointer_down = false;
    let mut bar_open = false;
    // Whether the overlay had something to say last frame; see the bar's own
    // block below.
    let mut overlay_shown = false;
    // Starts hidden: the wall comes up on the grid, and the picture's view has
    // nothing to show until a viewport is magnified.
    let mut video_shown = false;
    // How many more frames the wall keeps painting after the view has been
    // asked to come back. Moving a `SurfaceView` is a UI-thread traversal and
    // then a compositor frame, and on a run of fast swipes both can slip past
    // not just the frame the request was made in but the next one or two: the
    // UI thread is busiest exactly when the viewer is stepping through
    // cameras. One frame of grace was enough for a single swipe and not for
    // three in a row. Three more is a twentieth of a second, during which the
    // wall paints the sub-stream picture under a view that may already be up.
    const VIDEO_RAISE_FRAMES: u8 = 3;
    let mut raise_hold: u8 = 0;
    // A turned picture has no surface of its own: the wall draws it from a
    // texture it turns itself, so the codec has to hand frames back as bytes
    // while one is up. Asked once per change, not once per frame.
    let mut surface_suspended = false;
    // The rectangle the picture's view was last given, in physical pixels. A
    // relayout is not free - it resizes the surface the codec renders into - so
    // the rectangle is only sent when the wall's answer changes, and forgotten
    // when the view goes away so that the first frame of the next magnification
    // sends it again.
    let mut video_rect: Option<(f32, f32, f32, f32)> = None;
    // How much bigger than its box the picture's view is drawn, and where that
    // drawing starts, as last sent. Same terms as the box: a transform is free
    // to repeat but pointless to.
    let mut video_zoom: Option<(f32, f32, f32)> = None;
    // How far the moving page has taken the view, in pixels, as last sent.
    // Unlike the box this is a translation, so it costs nothing to repeat - but
    // it is still only sent when it changes.
    let mut video_shift: Option<f32> = None;
    crate::android::set_video_visible(false);

    // The bar has a screen of its own: a surface is a rectangle, so the only way
    // it can show the bar is for the bar to be all that is drawn in it. Same
    // theme, same faces, same pixels per point - a different screen.
    //
    // Rebuilt whenever its renderer is, and for the same reason: a context and a
    // renderer are one pair here. `egui` sends a texture's whole atlas once, on
    // the frame it makes it, and nothing but the deltas after that - so a new
    // renderer under an old context is a renderer with no atlas, and the first
    // delta it is handed is a partial update of a texture it never allocated.
    // That panics, and the panic is fatal: it aborted the process on the way
    // back from the background, where the bar's surface is destroyed and made
    // again while the wall keeps running.
    let bar_pixels = DENSITY.load(Ordering::Acquire) as f32 / 1000.0;
    let mut bar_context = bar_screen(bar_pixels);

    let started = Instant::now();
    let mut last = Instant::now();
    // Set when the surface under us went away; the option itself cannot be
    // cleared where that is noticed, because the frame's work holds it borrowed.
    let mut lost = false;
    loop {
        if lost {
            lost = false;
            surface = None;
            attached = std::ptr::null_mut();
        }
        let window = WINDOW.load(Ordering::Acquire);
        if window.is_null() {
            // Detached: the surface went away and no new one has arrived. The
            // app stays built, so coming back is a surface, not a restart.
            if surface.is_some() {
                surface = None;
                attached = std::ptr::null_mut();
                tracing::info!(target: "xgview", "the wall let go of its surface");
            }
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        if built.is_none() || attached != window {
            let Some(window_ptr) = std::ptr::NonNull::new(window) else {
                continue;
            };
            let handle_ndk = wgpu::rwh::RawWindowHandle::AndroidNdk(
                wgpu::rwh::AndroidNdkWindowHandle::new(window_ptr),
            );
            // Safety: the activity keeps the `SurfaceView` alive until it calls
            // `stop`, and a new window replaces this one only after that.
            let fresh = unsafe {
                instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: wgpu::rwh::RawDisplayHandle::Android(
                        wgpu::rwh::AndroidDisplayHandle::new(),
                    ),
                    raw_window_handle: handle_ndk,
                })
            };
            let fresh = match fresh {
                Ok(fresh) => fresh,
                Err(error) => {
                    tracing::warn!(target: "xgview", %error, "no surface to draw into");
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
            };
            let (width, height) = (WIDTH.load(Ordering::Acquire), HEIGHT.load(Ordering::Acquire));

            if let Some((_, render_state, _)) = built.as_ref() {
                // A second surface on the same device: the wall carries on.
                let Some(fresh_config) =
                    fresh.get_default_config(&render_state.adapter, width, height)
                else {
                    tracing::warn!(target: "xgview", width, height, "the new surface has no default config");
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                };
                // The same alpha mode the first attach asks for: a surface that
                // comes back is one whose format the platform may have decided
                // again, and `Auto` picks opaque when it can.
                let caps = fresh.get_capabilities(&render_state.adapter);
                let fresh_config = wgpu::SurfaceConfiguration {
                    alpha_mode: if caps
                        .alpha_modes
                        .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
                    {
                        wgpu::CompositeAlphaMode::PreMultiplied
                    } else {
                        caps.alpha_modes[0]
                    },
                    ..fresh_config
                };
                fresh.configure(&render_state.device, &fresh_config);
                tracing::info!(target: "xgview", width, height, "the wall took a new surface");
                surface = Some((fresh, fresh_config));
                attached = window;
                continue;
            }

            let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&fresh),
                force_fallback_adapter: false,
            }))
            .ok_or_else(|| anyhow::anyhow!("no runtime for the adapter request"))?
            .map_err(|error| anyhow::anyhow!("no adapter for the activity's surface: {error}"))?;
            // What the device is asked for, on the same terms eframe asks: the
            // GL backend on an old phone offers GLES 3.1, whose limits are the
            // WebGL 2 floor - 4 storage buffers per shader stage against the
            // 8 a default request demands - and a request above what the
            // adapter has is refused outright, which is a wall that never
            // draws. A device that reaches the gate's GL fallback is exactly
            // the sort to have limits like that; see `Attempt`.
            let base_limits = if adapter.get_info().backend == wgpu::Backend::Gl {
                wgpu::Limits::downlevel_webgl2_defaults()
            } else {
                wgpu::Limits::default()
            };
            let descriptor = wgpu::DeviceDescriptor {
                label: Some("xgview wall"),
                required_limits: wgpu::Limits {
                    // The picture's texture and the wall are both drawn into a
                    // surface this size, and a 4K screen wants more than the
                    // downlevel floor allows.
                    max_texture_dimension_2d: 8192,
                    ..base_limits
                },
                ..Default::default()
            };
            let (device, queue) = block_on(adapter.request_device(&descriptor))
                    .ok_or_else(|| anyhow::anyhow!("no runtime for the device request"))?
                    .map_err(|error| {
                        anyhow::anyhow!("no device for the activity's surface: {error}")
                    })?;

            let Some(fresh_config) = fresh.get_default_config(&adapter, width, height) else {
                // A surface the activity has already let go of offers nothing,
                // and one of those arrives on the way to a good one. Waiting for
                // the next is the whole handling it needs.
                tracing::info!(
                    target: "xgview",
                    width,
                    height,
                    "a surface offered no config; waiting for the next"
                );
                std::thread::sleep(Duration::from_millis(50));
                continue;
            };
            // The picture is on a surface *under* this one, so this one has to
            // let it through where nothing is drawn. `Auto` picks opaque when it
            // can, which would put a black frame over the video however little
            // egui painted - the alpha mode has to be asked for.
            let caps = fresh.get_capabilities(&adapter);
            let fresh_config = wgpu::SurfaceConfiguration {
                alpha_mode: if caps
                    .alpha_modes
                    .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
                {
                    wgpu::CompositeAlphaMode::PreMultiplied
                } else {
                    caps.alpha_modes[0]
                },
                ..fresh_config
            };
            fresh.configure(&device, &fresh_config);
            tracing::info!(
                target: "xgview",
                alpha_mode = ?fresh_config.alpha_mode,
                supported_alpha = ?caps.alpha_modes,
                format = ?fresh_config.format,
                supported_formats = ?caps.formats,
                "the wall's surface"
            );

            let renderer = egui_wgpu::Renderer::new(&device, fresh_config.format, None, 1, false);
            let render_state = egui_wgpu::RenderState {
                adapter,
                available_adapters: Vec::new(),
                device,
                queue,
                target_format: fresh_config.format,
                renderer: std::sync::Arc::new(egui::mutex::RwLock::new(renderer)),
            };

            let context = egui::Context::default();
            let pixels_per_point = DENSITY.load(Ordering::Acquire) as f32 / 1000.0;
            context.set_pixels_per_point(pixels_per_point);

            let app = XgViewApp::new(
                &context,
                &render_state,
                options.clone(),
                config_path.clone(),
                handle.clone(),
            );
            tracing::info!(
                target: "xgview",
                width = fresh_config.width,
                height = fresh_config.height,
                pixels_per_point,
                "the wall is drawing on the activity's surface"
            );
            surface = Some((fresh, fresh_config));
            attached = window;
            built = Some((context, render_state, app));
            continue;
        }

        let Some((context, render_state, app)) = built.as_mut() else {
            continue;
        };
        let Some((surface_ref, surface_config)) = surface.as_mut() else {
            continue;
        };
        let device = render_state.device.clone();
        let queue = render_state.queue.clone();

        let (width, height) = (WIDTH.load(Ordering::Acquire), HEIGHT.load(Ordering::Acquire));
        if width != surface_config.width || height != surface_config.height {
            surface_config.width = width;
            surface_config.height = height;
            surface_ref.configure(&device, surface_config);
        }
        let frame = match surface_ref.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Timeout) => {
                // A frame that never began. The loop no longer sleeps at the
                // end of every frame, so retrying this with no pause would spin
                // at full speed while a swapchain is slow to come back.
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(error) => {
                // The surface under us went away. Drop it and wait for the next
                // one rather than tearing the wall down.
                tracing::info!(target: "xgview", %error, "the surface stopped taking frames");
                lost = true;
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());

        let events = PENDING
            .lock()
            .map(|mut pending| std::mem::take(&mut *pending))
            .unwrap_or_default();
        let now = Instant::now();
        // egui subtracts this from every repaint request it is given. Left at
        // the real frame interval it becomes a feedback loop of its own: a
        // half-second idle wait makes the next request ask for almost nothing,
        // and the loop alternates a long sleep with none at all. A window's
        // cadence is a sixtieth - what the older front end's event loop
        // reports - so that is the ceiling here. The real clock still travels
        // in `RawInput::time`, so animations keep their own time.
        let predicted = now
            .duration_since(last)
            .as_secs_f32()
            .clamp(1.0 / 120.0, 1.0 / 60.0);
        last = now;
        let pixels_per_point = DENSITY.load(Ordering::Acquire) as f32 / 1000.0;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(
                    surface_config.width as f32 / pixels_per_point,
                    surface_config.height as f32 / pixels_per_point,
                ),
            )),
            time: Some(started.elapsed().as_secs_f64()),
            predicted_dt: predicted,
            focused: FOCUSED.load(Ordering::Acquire),
            events,
            ..Default::default()
        };
        // A turned picture is the one the surface cannot draw: rotating the
        // view does not rotate the frames the compositor scans out of it, and
        // there is no texture for the wall's turned mesh to sample. Suspend the
        // surface path for as long as the turn is up and the codec gives the
        // frames back to the wall, which draws them exactly as the older front
        // end always has. See `monitor_codec::amediacodec::set_surface_suspended`.
        //
        // An opaque-only overlay asks for the same suspension for good: with no
        // see-through layer over the picture, the wall is the only place the
        // OSD and the bar can be drawn, and it can only draw them if the codec
        // hands its frames back.
        //
        // A window that floats over the picture asks for it as well. It takes
        // no layout space, so the picture does not move out of its way, and
        // because the picture's view is above the wall the window would be
        // covered whole - invisible and unpressable at once. Suspending is what
        // puts the wall back in charge of the screen; see
        // `XgViewApp::floating_layer_open` for what counts as one.
        let suspend = app.picture_turned()
            || OPAQUE_ONLY.load(Ordering::Acquire)
            || app.floating_layer_open(&context);
        // The Java side is told as well, not just the codec: a view hidden while
        // the surface path is suspended keeps the frame it was last given, and
        // that frame is no longer being replaced by anything. Left standing it
        // would sit over the wall's own picture - which is exactly what a
        // portrait picture turned a quarter turn looked like: the pre-turn
        // frame in the middle of the rotated picture. See
        // `android::set_video_suspended`.
        if suspend != surface_suspended {
            surface_suspended = suspend;
            monitor_codec::amediacodec::set_surface_suspended(suspend);
            crate::android::set_video_suspended(suspend);
            tracing::info!(target: "xgview", suspended = suspend, "the surface path was suspended");
        }
        // The activity moves its view on the UI thread, a frame or more after
        // the request reaches it. `video_shown` is the answer this loop gave
        // the activity at the end of the *previous* frame, so while it is false
        // the view is still off screen; `raise_hold` then covers the frames the
        // UI thread and the compositor take to catch up. Throughout both the
        // wall goes on painting its sub-stream picture. Asking for the view
        // and standing aside in the same frame was a frame with neither
        // picture on it - the dark flash between the low-resolution wait and
        // the first main-stream frame.
        app.set_picture_layer_held(!video_shown || raise_hold > 0);
        if raise_hold > 0 {
            raise_hold -= 1;
        }
        let output = context.run(input, |ctx| app.frame(ctx));
        // egui's answer to "when next?". Every frame the application asks for
        // 33 ms while a live picture or an animation is running, or half a
        // second when the wall is idle. Both contexts may ask; the sooner wins.
        let mut repaint_delay = output
            .viewport_output
            .values()
            .map(|viewport| viewport.repaint_delay)
            .min()
            .unwrap_or(Duration::MAX);

        // The picture's view is asked for by what the frame wants, not by what
        // the wall did: a magnified viewport with a session really rendering
        // into that view. The wall's own answer is held back one frame (see
        // `set_picture_layer_held` above), so the request is made first and the
        // view has a frame to arrive before the wall leaves its area blank.
        // A `SurfaceView` keeps the last frame it was given and is drawn over
        // the wall, so one left visible while the wall paints its own picture
        // covers it - and one shown while empty is a black rectangle over
        // everything.
        let on_layer = app.picture_layer_wanted();
        let changed = on_layer != video_shown;
        if changed {
            // Placed again before it is shown again: the rectangle it was left
            // at belongs to whatever was magnified then, which may have been
            // another camera with another shape. See the call below, which is
            // made after the box so that the first frame the view is seen in is
            // already the right one.
            video_rect = None;
            video_zoom = None;
            video_shift = None;
        }
        // Where the picture is. The view the codec renders into is full screen
        // until it is told otherwise, and a full-screen surface stretches the
        // picture into the screen's shape - which is why the fit switch used to
        // change the wall's mind and nothing on the screen. Rounded to a pixel,
        // and sent only when the answer changes: this resizes the surface the
        // codec is drawing into, which is not something to do every frame.
        //
        // Nor during a seek's handover. There is no session rendering then, but
        // the view is still up holding the last picture at the geometry it was
        // given; re-placing it from a box measured off the wall's stale camera
        // texture would resize the surface under that picture and squeeze it
        // into the wrong aspect for the length of the wait. See
        // `XgViewApp::picture_size`.
        if on_layer {
            // A page a finger is dragging takes the view with it. The box is
            // where the page will come to rest and the travel is a translation:
            // moving a `SurfaceView` by its rectangle means a layout pass per
            // frame, which resizes the surface the codec draws into and lands
            // the picture a frame behind its page. See `picture_slide`.
            //
            // Only the drag: a page *settling* after a swipe is two cameras on
            // screen at once, and the view is hidden for the length of it. See
            // `XgViewApp::picture_on_layer`.
            let sliding = app.picture_sliding();
            let shift = match sliding {
                true => (app.picture_slide() * pixels_per_point).round(),
                false => 0.0,
            };
            if video_shift != Some(shift) {
                video_shift = Some(shift);
                crate::android::set_video_shift(shift);
            }
            // The box the page comes to rest in - the travel is the translation
            // above, so it is taken back out here and the box changes only when
            // the *shape* does: once per step, not once per frame. A step to a
            // camera of another shape changes it, and the view has to know: left
            // at the old camera's box, its picture is squeezed into a shape it
            // never had.
            if let Some(rect) = app
                .picture_rect()
                .map(|rect| rect.translate(egui::vec2(-app.picture_slide(), 0.0)))
                .filter(|_| crate::picture_on_its_own_surface())
            {
                let left = (rect.min.x * pixels_per_point).round();
                let top = (rect.min.y * pixels_per_point).round();
                let right = (rect.max.x * pixels_per_point).round();
                let bottom = (rect.max.y * pixels_per_point).round();
                let wanted = (left, top, right, bottom);
                if video_rect != Some(wanted) {
                    video_rect = Some(wanted);
                    tracing::info!(
                        target: "xgview",
                        rect = ?wanted,
                        "the picture's rectangle"
                    );
                    crate::android::set_video_rect(left, top, right - left, bottom - top);
                }
            }
            // What the picture shows of itself: the window a pinch has left it
            // at, drawn as a scale of the view - bigger than the box, offset so
            // that the part being looked at is the part in it. The window is in
            // the picture's own coordinates, so where the picture's corner goes
            // is that window's corner times the box **at that scale**: the view
            // is scaled about its own corner, and a translation given to a
            // scaled view is added to the scaled drawing, not to the box. Leave
            // the scale out and the picture is both placed wrong and moves at a
            // fraction of the finger - `1/scale` of it, which is exactly what a
            // drag at 4x looked like. See `setVideoZoom`.
            if let Some(box_) = app
                .picture_rect()
                .filter(|_| crate::picture_on_its_own_surface())
            {
                let zoom = app.picture_zoom();
                let window = zoom.window();
                let scale = zoom.scale;
                let drawn = egui::vec2(
                    box_.width() * pixels_per_point,
                    box_.height() * pixels_per_point,
                );
                let dx = (-window.min.x * drawn.x * scale).round();
                let dy = (-window.min.y * drawn.y * scale).round();
                let wanted = (scale, dx, dy);
                if video_zoom != Some(wanted) {
                    video_zoom = Some(wanted);
                    crate::android::set_video_zoom(scale, dx, dy);
                }
            }
        }
        // Shown, hidden - and only ever told after the box and the travel above,
        // so that a view coming back is never seen for a frame at the geometry
        // of the camera before the step: one such frame is a second picture of
        // the new camera squeezed into the old one's shape.
        if changed {
            video_shown = on_layer;
            if on_layer {
                raise_hold = VIDEO_RAISE_FRAMES;
            }
            tracing::info!(target: "xgview", visible = on_layer, "the picture's view");
            crate::android::set_video_visible(on_layer);
        }
        let jobs = context.tessellate(output.shapes, output.pixels_per_point);
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [surface_config.width, surface_config.height],
            pixels_per_point,
        };
        {
            let mut renderer = render_state.renderer.write();
            for (id, delta) in &output.textures_delta.set {
                renderer.update_texture(&device, &queue, *id, delta);
            }
        }
        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let prepared = render_state.renderer.write().update_buffers(
            &device,
            &queue,
            &mut encoder,
            &jobs,
            &screen,
        );
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("wall"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.0,
                                g: 0.0,
                                b: 0.0,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                })
                .forget_lifetime();
            render_state.renderer.read().render(&mut pass, &jobs, &screen);
        }
        queue.submit(prepared.into_iter().chain(std::iter::once(encoder.finish())));
        {
            let mut renderer = render_state.renderer.write();
            for id in &output.textures_delta.free {
                renderer.free_texture(id);
            }
        }
        frame.present();
        // The first frame that reached the screen: a Vulkan attempt this far is
        // one that can draw, so its marker has done its job. No-op afterwards.
        DREW.store(true, Ordering::Relaxed);
        attempt.settled();

        // The bar and the corner display, over the picture. Their own surface
        // and their own swapchain, presented only while there is something to
        // put on them - a grid has neither, and a transparent frame every
        // sixtieth of a second is a full-screen layer for nothing.
        let bar_window = BAR_WINDOW.load(Ordering::Acquire);
        if bar_window.is_null() {
            bar = None;
            bar_attached = std::ptr::null_mut();
        } else if bar.is_none() || bar_attached != bar_window {
            if let Some(window_ptr) = std::ptr::NonNull::new(bar_window) {
                let handle_ndk = wgpu::rwh::RawWindowHandle::AndroidNdk(
                    wgpu::rwh::AndroidNdkWindowHandle::new(window_ptr),
                );
                // Safety: the activity keeps the `SurfaceView` alive until it
                // calls `stop`, and a new window replaces this one only after.
                let fresh = unsafe {
                    instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                        raw_display_handle: wgpu::rwh::RawDisplayHandle::Android(
                            wgpu::rwh::AndroidDisplayHandle::new(),
                        ),
                        raw_window_handle: handle_ndk,
                    })
                };
                if let Ok(fresh) = fresh {
                    let (width, height) =
                        (BAR_WIDTH.load(Ordering::Acquire), BAR_HEIGHT.load(Ordering::Acquire));
                    if let Some(config) = fresh.get_default_config(&render_state.adapter, width, height)
                    {
                        fresh.configure(&render_state.device, &config);
                        let caps = fresh.get_capabilities(&render_state.adapter);
                        tracing::info!(
                            target: "xgview",
                            width = config.width,
                            height = config.height,
                            alpha_mode = ?config.alpha_mode,
                            supported_alpha = ?caps.alpha_modes,
                            format = ?config.format,
                            "the bar has a surface of its own"
                        );
                        // The overlay is drawn over the picture, so it is only
                        // an overlay if it can be see-through. A device whose
                        // surfaces offer nothing but `Opaque` would paint a
                        // black rectangle over the picture instead. There is
                        // no second arrangement of layers that fixes it, but
                        // there is an older one: give the frames back to the
                        // wall and let it draw the whole screen itself.
                        let opaque = caps
                            .alpha_modes
                            .iter()
                            .all(|mode| *mode == wgpu::CompositeAlphaMode::Opaque);
                        OPAQUE_ONLY.store(opaque, Ordering::Release);
                        if opaque {
                            tracing::warn!(
                                target: "xgview",
                                "this surface cannot be see-through: the pictures go back to the wall"
                            );
                            wake();
                        }
                        let renderer = egui_wgpu::Renderer::new(
                            &render_state.device,
                            config.format,
                            None,
                            1,
                            false,
                        );
                        bar = Some((fresh, config, renderer));
                        bar_attached = bar_window;
                        // The context goes with the renderer: the one that was
                        // here belonged to the renderer that has just been
                        // dropped, and the atlas it had drawn is a texture this
                        // one has never been given. See `bar_screen`.
                        bar_context = bar_screen(bar_pixels);
                        last_focus = None;
                        // A new context knows nothing, so what the last one was
                        // told is forgotten with it: the next frame says it
                        // again rather than assuming it is still known, or a
                        // control under a stationary finger would stay unlit.
                        last_pointer = None;
                        last_pointer_down = false;
                    }
                }
            }
        } else if let Some((bar_surface, bar_config, _)) = bar.as_mut() {
            // A turn changes the window's shape without replacing the Surface
            // object, so `bar_attached` still matches and the branch above does
            // not run. The wall's own surface reconfigures on every resize;
            // this one has to as well, or its swapchain keeps the landscape
            // rectangle and the overlay lays its opaque bands out against a
            // screen that no longer exists - a black picture with its controls
            // in a corner. See `resize` for the wall's side of the same turn.
            let (width, height) = (
                BAR_WIDTH.load(Ordering::Acquire),
                BAR_HEIGHT.load(Ordering::Acquire),
            );
            if width > 0
                && height > 0
                && (width != bar_config.width || height != bar_config.height)
            {
                bar_config.width = width;
                bar_config.height = height;
                bar_surface.configure(&render_state.device, bar_config);
            }
        }
        // Something to say, or one frame left to say nothing in: the bar that
        // was up has to be painted away, and after that the surface is left
        // alone - it keeps its last frame, which is the empty one - until it is
        // wanted again. It is wanted only while the wall's frame is dropped;
        // otherwise the wall draws its own corners and bar.
        let overlay_wanted = app.picture_on_layer();
        let overlay_drawn = overlay_wanted || overlay_shown;
        overlay_shown = overlay_wanted;
        if let Some((bar_surface, bar_config, bar_renderer)) =
            bar.as_mut().filter(|_| overlay_drawn)
        {
            match bar_surface.get_current_texture() {
                Ok(bar_frame) => {
                    let view = bar_frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
                    let mut encoder =
                        device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
                    // The bar's own frame.
                    let bar_rect = egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(
                            bar_config.width as f32 / pixels_per_point,
                            bar_config.height as f32 / pixels_per_point,
                        ),
                    );
                    // The wall's screen in the strip's own coordinates: the two
                    // surfaces differ by the strip's height, so a rectangle the
                    // wall measured reaches this frame shifted by that much.
                    // Wall-space rectangles - the picture's box, the pointer,
                    // the focus ring - are translated by `shift` below.
                    let screen_w = WIDTH.load(Ordering::Acquire) as f32 / pixels_per_point;
                    let screen_h = HEIGHT.load(Ordering::Acquire) as f32 / pixels_per_point;
                    let strip_h = bar_config.height as f32 / pixels_per_point;
                    let shift = egui::vec2(0.0, strip_h - screen_h);
                    let screen = egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(screen_w, screen_h),
                    );
                    let picture = screen.translate(shift);
                    // What the wall actually laid its corners, frame and bar
                    // out against: its tile, with the gutter a kept navigation
                    // bar claims and the bands of its own bars left out. The
                    // screen is not that rectangle when a gutter is kept, and a
                    // bar laid out against the screen then sits under the
                    // system bar while the wall takes presses higher up.
                    let body = app
                        .magnified_tile()
                        .unwrap_or(screen)
                        .translate(shift);
                    // This frame is a paint, not an update: the presses are
                    // found in the wall's frame, where the pointer and the
                    // remote already are. What a paint cannot work out for
                    // itself is carried across - the focus ring below, and the
                    // pointer here. Without the pointer every control is drawn
                    // idle, so a button that is being held down looks untouched
                    // until the state it changed shows up, which on a box with
                    // a mouse or a finger reads as "the click did nothing".
                    let (pointer_pos, pointer_down) = context.input(|input| {
                        (input.pointer.latest_pos(), input.pointer.primary_down())
                    });
                    let mut bar_events = Vec::new();
                    if pointer_pos != last_pointer {
                        last_pointer = pointer_pos;
                        bar_events.push(match pointer_pos {
                            Some(pos) => egui::Event::PointerMoved(pos + shift),
                            None => egui::Event::PointerGone,
                        });
                    }
                    if pointer_down != last_pointer_down {
                        last_pointer_down = pointer_down;
                        if let Some(pos) = pointer_pos {
                            bar_events.push(egui::Event::PointerButton {
                                pos: pos + shift,
                                button: egui::PointerButton::Primary,
                                pressed: pointer_down,
                                modifiers: egui::Modifiers::default(),
                            });
                        }
                    }
                    let bar_input = egui::RawInput {
                        screen_rect: Some(bar_rect),
                        time: Some(started.elapsed().as_secs_f64()),
                        events: bar_events,
                        // The same window, so the same answer: a strip that
                        // believed it had the focus while the notification
                        // shade was down would keep drawing a ring on a
                        // control nobody can reach.
                        focused: FOCUSED.load(Ordering::Acquire),
                        ..Default::default()
                    };
                    // The ring the remote puts on a control is the one thing a
                    // paint cannot work out for itself: egui draws it for
                    // whatever its own memory says has the focus, and the focus
                    // lives in the wall's context - the one that reads the
                    // arrows. So it is carried across, every frame, and the
                    // strip's copy of the bar lights the same control the
                    // wall's copy would.
                    //
                    // Both ways: the wall gives the focus up when the viewer
                    // leaves the bar - Back, a press on the picture, the window
                    // losing it - and a mirror that only ever hands the focus
                    // out would keep the ring on the last control for ever.
                    match context.memory(|memory| memory.focused()) {
                        Some(focused) => {
                            bar_context.memory_mut(|memory| memory.request_focus(focused))
                        }
                        None => bar_context.memory_mut(|memory| {
                            if let Some(stale) = memory.focused() {
                                memory.surrender_focus(stale);
                            }
                        }),
                    }
                    {
                        let now_focused = context.memory(|memory| memory.focused());
                        if now_focused != last_focus {
                            last_focus = now_focused;
                            tracing::debug!(
                                target: "xgview::tilebar",
                                focused = ?now_focused,
                                "the focus the strip was given"
                            );
                        }
                    }
                    let open = app.bar_open();
                    if open != bar_open {
                        bar_open = open;
                        tracing::info!(
                            target: "xgview",
                            open,
                            strip = ?bar_rect,
                            "the bar's own frame"
                        );
                    }
                    let bar_output = bar_context
                        .run(bar_input, |ctx| app.draw_overlay_only(ctx, picture, body));
                    repaint_delay = repaint_delay.min(
                        bar_output
                            .viewport_output
                            .values()
                            .map(|viewport| viewport.repaint_delay)
                            .min()
                            .unwrap_or(Duration::MAX),
                    );
                    let bar_jobs =
                        bar_context.tessellate(bar_output.shapes, bar_output.pixels_per_point);
                    let bar_screen = egui_wgpu::ScreenDescriptor {
                        size_in_pixels: [bar_config.width, bar_config.height],
                        pixels_per_point,
                    };
                    {
                        for (id, delta) in &bar_output.textures_delta.set {
                            bar_renderer.update_texture(&device, &queue, *id, delta);
                        }
                    }
                    let prepared = bar_renderer.update_buffers(
                        &device,
                        &queue,
                        &mut encoder,
                        &bar_jobs,
                        &bar_screen,
                    );
                    {
                        let mut pass = encoder
                            .begin_render_pass(&wgpu::RenderPassDescriptor {
                                label: Some("bar"),
                                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                    view: &view,
                                    resolve_target: None,
                                    ops: wgpu::Operations {
                                        // Nothing under the bar is the bar's:
                                        // the strip is see-through wherever it
                                        // has nothing to say, so the picture
                                        // runs under it as it does everywhere
                                        // else.
                                        load: wgpu::LoadOp::Clear(wgpu::Color {
                                            r: 0.0,
                                            g: 0.0,
                                            b: 0.0,
                                            a: 0.0,
                                        }),
                                        store: wgpu::StoreOp::Store,
                                    },
                                })],
                                depth_stencil_attachment: None,
                                timestamp_writes: None,
                                occlusion_query_set: None,
                            })
                            .forget_lifetime();
                        bar_renderer.render(&mut pass, &bar_jobs, &bar_screen);
                    }
                    queue.submit(prepared.into_iter().chain(std::iter::once(encoder.finish())));
                    for id in &bar_output.textures_delta.free {
                        bar_renderer.free_texture(id);
                    }
                    bar_frame.present();
                }
                Err(error) => {
                    tracing::info!(target: "xgview", %error, "the bar's surface stopped taking frames");
                    bar = None;
                    bar_attached = std::ptr::null_mut();
                }
            }
        }

        // Wait for the next frame egui asked for, instead of compositing both
        // surfaces sixty times a second whether or not anything changed. A
        // live channel asks for 33 ms and an idle wall for half a second - the
        // same two answers eframe's loop gets on the older front end. The
        // picture the codec scans straight into its own surface needs no
        // repaint from here; one the wall draws from a texture is a live
        // channel, and `needs_animation` has already asked for the 33 ms.
        //
        // egui has already taken `predicted_dt` off the delay it asks for, so
        // the frame's own cost is not subtracted a second time here.
        //
        // Urgent work does not wait for the timer. A key, a touch, a new
        // surface or a worker thread's message calls `wake`, which cuts the
        // wait short; the cap is a net under any state that changes without
        // one.
        let wait = repaint_delay.min(Duration::from_secs(1));
        if !wait.is_zero() {
            let (lock, cvar) = wake_pair();
            let mut urgent = lock.lock().unwrap_or_else(|error| error.into_inner());
            if *urgent {
                // Asked for while this frame was being drawn: the frame it
                // wants is the next one, so there is nothing to wait for.
                *urgent = false;
            } else {
                let (mut guard, _) = cvar
                    .wait_timeout(urgent, wait)
                    .unwrap_or_else(|error| error.into_inner());
                *guard = false;
            }
        }
    }
}

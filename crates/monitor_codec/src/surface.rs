//! The surface a decoded picture is rendered into on Android, and what is true
//! of it.
//!
//! A codec configured with this window renders into it and never hands a picture
//! back: the compositor scans the buffer out, and the pixels never enter this
//! process. That is the path every Android video player takes, and the reason
//! none of them care which box they run on. The wall keeps its own surface for
//! the interface, above this one, and leaves the picture's area unpainted.
//!
//! The state used to be five independent atomics - the window, a "has a
//! picture" flag, the time of the last one, a session count and a request
//! counter. A session draining a frame from a window that had just been
//! destroyed could read the *old* request, lose the race to
//! `set_surface_window`'s clear, and then put "this window has a picture" back
//! onto a brand-new, empty surface. The wall kept away from that black rectangle
//! for a whole handover. Here the answers are one record under one lock, and a
//! generation a session can carry: the check and the write cannot be seen apart.
//!
//! Nothing here needs the NDK, which is why it lives in its own module rather
//! than in `amediacodec`: the race it settles is pure state, and its tests run
//! on the desktop where the Android decoder cannot be compiled.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// Everything that is true of the surface a decoded picture is rendered into.
#[derive(Default)]
struct SurfaceFact {
    /// The `ANativeWindow` as an address; `0` while there is none.
    window: usize,
    /// Bumped whenever the window is really replaced. A session opened under an
    /// earlier number belongs to a surface that is gone.
    generation: u64,
    /// Bumped whenever a new camera's picture is asked for. Frames still
    /// draining from the session opened for the old one are not its answer.
    request: u64,
    /// When a picture was last handed to this window, in milliseconds since the
    /// epoch. Zero means this window has never held one.
    last_presented: u64,
    /// Sessions rendering into *this* generation of the window. Reset with the
    /// generation, so a session that outlives its surface cannot keep a new
    /// one's answer true.
    sessions: usize,
    /// Which of the activity's views the codec was configured with. Stepping
    /// to the other camera's view changes this and nothing else: both views
    /// are alive, so the picture the wall was showing is still the right thing
    /// to keep showing while the new session opens. The codec still has to be
    /// reopened, because it was pointed at a different `ANativeWindow`.
    window_epoch: u64,
}

impl SurfaceFact {
    const fn new() -> Self {
        Self {
            window: 0,
            generation: 0,
            request: 0,
            last_presented: 0,
            sessions: 0,
            window_epoch: 0,
        }
    }
}

static SURFACE_FACT: Mutex<SurfaceFact> = Mutex::new(SurfaceFact::new());

/// The one way anyone reads or writes the surface's record.
///
/// A poisoned lock is answered from its contents rather than refused: every
/// write here is a few stores, so a panic while holding it cannot have left a
/// half-written record worth hiding.
fn with_surface<R>(f: impl FnOnce(&mut SurfaceFact) -> R) -> R {
    match SURFACE_FACT.lock() {
        Ok(mut fact) => f(&mut fact),
        Err(poisoned) => f(&mut poisoned.into_inner()),
    }
}

/// What a session needs to know about the surface it is opening on, read as one
/// answer: the window to hand the codec, and the generation and request to
/// carry back when it renders.
#[derive(Clone, Copy)]
pub(crate) struct SurfaceSnapshot {
    pub(crate) window: *mut c_void,
    pub(crate) generation: u64,
    pub(crate) request: u64,
    pub(crate) window_epoch: u64,
}

pub(crate) fn surface_snapshot() -> SurfaceSnapshot {
    with_surface(|fact| SurfaceSnapshot {
        window: fact.window as *mut c_void,
        generation: fact.generation,
        request: fact.request,
        window_epoch: fact.window_epoch,
    })
}

/// Takes the surface a decoded picture is rendered into, or null when it went
/// away. See `SurfaceActivity` and `monitor_gui::android`.
/// Three different hand-overs reach this door, and only one of them is a
/// surface being destroyed and made again:
///
/// - `replaced`: `surfaceCreated` / `surfaceDestroyed`. The new surface is
///   empty, so everything the old one remembered goes. This is the case the
///   generation was introduced for.
/// - `switched`: the wall stepped to the other camera's view. Both views are
///   alive; the only thing that changed is which `ANativeWindow` the codec was
///   pointed at. It has to be reopened, but the picture the viewer is looking
///   at, the request it answered and the sessions still drawing are all still
///   true - clearing them here is what made a channel step flash the wall's
///   480p fallback before the main stream arrived.
/// - neither: `surfaceChanged` re-binding the same surface. Only the pointer
///   is refreshed.
///
/// `window_replaced` stays as the null-boundary fallback. The activity is the
/// only witness that can tell a switch from a re-bind: both carry two non-null
/// `ANativeWindow*` that say nothing about whether the surface is the same one.
pub fn set_surface_window(window: *mut c_void, replaced: bool, switched: bool) {
    let did_replace = with_surface(|fact| replace_window(fact, window, replaced, switched));
    if did_replace {
        tracing::info!(
            target: "xgview::codec",
            present = !window.is_null(),
            "the surface a picture is rendered into came or went"
        );
    } else if switched {
        tracing::debug!(
            target: "xgview::codec",
            "the picture stepped to the other camera's view"
        );
    }
}

fn replace_window(
    fact: &mut SurfaceFact,
    window: *mut c_void,
    said: bool,
    switched: bool,
) -> bool {
    let replaced = said || window_replaced(fact.window as *mut c_void, window);
    fact.window = window as usize;
    if replaced {
        fact.generation = fact.generation.wrapping_add(1);
        fact.request = fact.request.wrapping_add(1);
        fact.last_presented = 0;
        fact.sessions = 0;
        fact.window_epoch = fact.window_epoch.wrapping_add(1);
    } else if switched {
        fact.window_epoch = fact.window_epoch.wrapping_add(1);
    }
    replaced
}

/// Whether the surface crossed a null boundary, which is a replacement no
/// matter what the activity said: the activity calls `nativeStopVideo` before a
/// new surface is created, so a resize that merely re-binds the window never
/// passes through one. This is the fallback; the word of what a hand-over is
/// comes from the activity, in `replaced`.
fn window_replaced(previous: *mut c_void, window: *mut c_void) -> bool {
    previous.is_null() || window.is_null()
}

/// Whether the wall needs the pictures back as bytes while a surface is up.
///
/// A magnified picture turned a quarter turn on an upright wall is drawn by
/// sampling a texture sideways; a codec rendering straight into a surface has
/// no texture to turn, and rotating the view does not rotate the frames the
/// compositor scans out. While such a picture is up the surface path is
/// suspended: the codec hands frames back, the wall draws them with the same
/// turned mesh it has always used, and the view is hidden. A television has no
/// portrait to turn to, so the box this surface path exists for never suspends.
static SURFACE_SUSPENDED: AtomicBool = AtomicBool::new(false);

/// Suspends or resumes rendering into the activity's surface.
///
/// Both directions make every open codec close and reopen on the other kind of
/// output, so the change is not instant: a few access units go by before the
/// wall has a frame to draw, or the surface has a picture in it. The wall and
/// the view decide what is on screen from [`surface_presenting`], not from this
/// flag, so nothing is left blank in between.
pub fn set_surface_suspended(suspended: bool) {
    SURFACE_SUSPENDED.store(suspended, Ordering::Release);
}

pub(crate) fn surface_suspended() -> bool {
    SURFACE_SUSPENDED.load(Ordering::Acquire)
}

/// Whether a magnified picture has asked for the activity's surface.
///
/// The window outlives the view: it is created with the activity and stays
/// there while the viewer looks at a grid, with the view merely moved off
/// screen. A wide stream opened on a grid is still a wide stream, and one
/// configured with that window would render every frame into a surface nobody
/// is showing - the tile would wait for a first picture for ever, because the
/// codec hands none back. See `AMediaCodecDecoder::open`.
///
/// The magnified viewport is the only thing the surface is for, so its owner
/// sets this. `set_video_owner` answers it on the frame the zoom changes; the
/// wall's `picture_layer_wanted` and the codec's choice of output then read the
/// same answer. A device whose surface has gone away, or one that has suspended
/// the path for a turned picture, still answers false from the other two terms.
static SURFACE_LAYER_WANTED: AtomicBool = AtomicBool::new(false);

/// Says whether the magnified viewport is asking the surface for a picture.
pub fn set_surface_layer_wanted(wanted: bool) {
    SURFACE_LAYER_WANTED.store(wanted, Ordering::Release);
}

pub(crate) fn surface_layer_wanted() -> bool {
    SURFACE_LAYER_WANTED.load(Ordering::Acquire)
}

fn layer_active(suspended: bool, wanted: bool, window: bool) -> bool {
    !suspended && wanted && window
}

/// Whether there is a surface a picture could be rendered into right now.
pub fn video_layer_active() -> bool {
    layer_active(
        surface_suspended(),
        surface_layer_wanted(),
        with_surface(|fact| fact.window != 0),
    )
}

/// How long a surface whose next session has not drawn yet still counts as
/// holding its last picture. A seek's decoder needs a few hundred milliseconds
/// to open and reach its first frame (260-600 ms measured); a surface that was
/// destroyed, or a session that never came back, must not be waited on.
const HANDOVER_GRACE: Duration = Duration::from_secs(2);

pub(crate) fn millis_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Whether a picture is on the activity's surface and the wall should leave its
/// area alone.
///
/// A session counts itself in only after the window it was opened for has
/// rendered a picture. When that window is replaced the count is reset with the
/// generation, so a session left over from it cannot keep the new one's answer
/// true: that was a black hole the wall kept away from until the codec reopened.
/// The window is part of the answer for the same reason.
///
/// The other gap is a handover between two sessions on one window, which is
/// what every seek is. `last_presented` is what keeps the answer true through
/// it: the last picture is still there, and it is the frame a viewer waiting for
/// a seek should go on looking at. `set_surface_window` clears that memory, so
/// a surface that was destroyed and made again cannot claim to hold a picture.
pub fn surface_presenting() -> bool {
    if surface_suspended() {
        return false;
    }
    let now = millis_now();
    with_surface(|fact| presenting(fact, now))
}

fn presenting(fact: &SurfaceFact, now: u64) -> bool {
    fact.window != 0 && (fact.sessions > 0 || within_handover(fact.last_presented, now))
}

/// Whether the activity's surface is still there and still holds the last
/// picture it was given.
///
/// Unlike [`surface_presenting`] this asks nothing about a decoder rendering
/// right now, nor how long ago one stopped: it says only that leaving the area
/// unpainted still leaves a picture on screen. The wall needs that while a seek
/// is in flight, because the wait for a recording's range is a network request
/// and can outlast [`HANDOVER_GRACE`] before a decoder is even opened.
/// `set_surface_window` clears the memory, so a window that was destroyed and
/// made again cannot claim to be holding one.
pub fn surface_holds_picture() -> bool {
    if surface_suspended() {
        return false;
    }
    with_surface(|fact| holds_picture(false, fact.window != 0, fact.last_presented != 0))
}

fn holds_picture(suspended: bool, window: bool, has_picture: bool) -> bool {
    !suspended && window && has_picture
}

/// Forgets the picture the surface was holding, without touching the window.
///
/// Hiding the picture's view moves it out of the way rather than destroying its
/// surface (see `SurfaceActivity.setVideoVisible`), so the last frame stays in
/// its buffers. Showing that view again for another recording must not count
/// that frame as the picture of *this* one: a seek in flight would keep the wall
/// away from its area, and the viewer would watch the previous camera until the
/// new session draws. The next rendered picture sets the memory again, in
/// `drain`; the handover grace starts over with it.
pub fn forget_surface_picture() {
    with_surface(|fact| fact.last_presented = 0);
}

/// Asks the surface for a new picture, forgetting the one it holds.
///
/// Called when the wall steps to another camera
/// (`XgViewApp::track_surface_picture`). A `SurfaceView` keeps the last frame
/// it was given, so the old session can still be draining its late frames while
/// the new one connects; its `request` no longer matches and `drain` refuses
/// them. Hiding the view is not such a request - its surface and its session
/// survive the move - so that one uses `forget_surface_picture` alone.
///
/// A *window* being replaced advances both `generation` and `request` in
/// `set_surface_window`, in the same lock that forgets the picture.
pub fn begin_surface_picture() {
    with_surface(|fact| {
        fact.request = fact.request.wrapping_add(1);
        fact.last_presented = 0;
    });
}

/// Whether a session opened under `session` still answers the request the
/// surface is holding now.
fn answers_current_request(session: u64, current: u64) -> bool {
    session == current
}

/// Records one picture a session rendered into the surface.
///
/// `generation` and `request` are what the session read when it opened. The
/// check and the write are one locked act: a session whose surface was replaced
/// between reading them and rendering must not put the memory of a picture
/// onto the new, empty window. Returns whether this session has just been
/// counted as presenting, which it needs to know so that `close` takes it out
/// again exactly once.
pub(crate) fn surface_rendered(
    generation: u64,
    request: u64,
    now: u64,
    counted: bool,
) -> bool {
    with_surface(|fact| rendered(fact, generation, request, now, counted))
}

/// Whether a session's frame still answers the surface that is up: the window
/// is there, it is the generation the session was opened under, and the picture
/// it was opened for is the one still being asked for.
fn answers_surface(generation: u64, request: u64, fact: &SurfaceFact) -> bool {
    fact.window != 0
        && fact.generation == generation
        && answers_current_request(request, fact.request)
}

/// The write half of [`surface_rendered`], on a record the caller holds the
/// lock for. Kept as a pure function so the race it settles can be written down
/// as a test rather than described.
fn rendered(
    fact: &mut SurfaceFact,
    generation: u64,
    request: u64,
    now: u64,
    counted: bool,
) -> bool {
    if !answers_surface(generation, request, fact) {
        return false;
    }
    fact.last_presented = now;
    if counted {
        return false;
    }
    fact.sessions = fact.sessions.saturating_add(1);
    true
}

/// One session that had counted itself in is gone.
///
/// A generation that has been replaced already reset its count, and a session
/// left over from it must not decrement the new generation's.
pub(crate) fn surface_session_closed(generation: u64) {
    with_surface(|fact| session_closed(fact, generation));
}

fn session_closed(fact: &mut SurfaceFact, generation: u64) {
    if fact.generation == generation {
        fact.sessions = fact.sessions.saturating_sub(1);
    }
}

/// Whether a handover is still young enough that the surface's last picture is
/// the one a viewer should be left looking at. Zero means this window has never
/// held a picture; a clock that went backwards says nothing about a gap either.
fn within_handover(last: u64, now: u64) -> bool {
    last != 0
        && now >= last
        && now - last < HANDOVER_GRACE.as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::{
        answers_current_request, answers_surface, holds_picture, layer_active, presenting,
        rendered, replace_window, session_closed, set_surface_layer_wanted, set_surface_suspended,
        set_surface_window, video_layer_active, window_replaced, within_handover, SurfaceFact,
    };
    use std::ffi::c_void;

    /// A seek's gap keeps the last picture on the surface; a window that never
    /// drew one, and one whose decoder never came back, do not.
    #[test]
    fn a_handover_keeps_the_last_picture_only_for_a_short_gap() {
        let last = 10_000;
        assert!(!within_handover(0, last), "a fresh surface holds nothing");
        assert!(within_handover(last, last), "the frame just rendered");
        assert!(within_handover(last, last + 500), "a seek's decoder opening");
        assert!(!within_handover(last, last + 2_000), "two seconds is too long");
        assert!(!within_handover(last, last - 1), "a clock that went backwards");
    }

    /// A long wait for a recording's range is still a wait on a picture that is
    /// on screen, but only while the window is there and has drawn before.
    #[test]
    fn a_surface_holds_its_picture_through_a_long_wait() {
        assert!(holds_picture(false, true, true), "a seek is in flight");
        assert!(!holds_picture(false, true, false), "the window never drew");
        assert!(!holds_picture(false, false, true), "the window went away");
        assert!(!holds_picture(true, true, true), "the surface is suspended");
    }

    /// A step to another camera asks the surface for a new picture. Frames
    /// still draining from the session opened for the old one belong to that
    /// request, and must not say the new picture has arrived.
    #[test]
    fn a_new_request_accepts_only_its_own_sessions() {
        assert!(answers_current_request(3, 3), "this camera's first frame");
        assert!(!answers_current_request(2, 3), "the camera being left");
        assert!(!answers_current_request(4, 3), "a session from the future");
    }

    /// A change of picture size hands the same surface back. Only a surface
    /// that was destroyed and made again forgets the picture it was holding.
    #[test]
    fn a_resize_does_not_forget_the_picture_but_a_new_surface_does() {
        let window = 1usize as *mut c_void;
        let other = 2usize as *mut c_void;
        assert!(!window_replaced(window, window), "the same window re-bound");
        assert!(!window_replaced(window, other), "a new wrapper, same surface");
        assert!(window_replaced(window, std::ptr::null_mut()), "destroyed");
        assert!(window_replaced(std::ptr::null_mut(), window), "created");
    }

    /// A window destroyed and made again is a new generation. A frame the old
    /// session was already draining when the swap happened must not put the
    /// memory of a picture onto the new, empty surface - which is the race that
    /// made the wall keep away from a black rectangle for a whole handover.
    #[test]
    fn a_late_frame_cannot_mark_a_replaced_window_as_presenting() {
        let mut fact = SurfaceFact::new();
        fact.window = 1;
        fact.generation = 4;
        fact.request = 9;
        assert!(answers_surface(4, 9, &fact));
        assert!(rendered(&mut fact, 4, 9, 1_000, false), "first picture counted");
        assert!(presenting(&fact, 1_000));

        // `set_surface_window(null, true)` followed by a new window: both
        // numbers move on and the record starts empty.
        assert!(replace_window(&mut fact, std::ptr::null_mut(), true, false));
        assert!(replace_window(&mut fact, 2usize as *mut c_void, true, false));
        assert!(!answers_surface(4, 9, &fact), "the old session's generation");
        assert!(!rendered(&mut fact, 4, 9, 1_050, true), "its late frame");
        assert_eq!(fact.last_presented, 0, "the new window is still empty");
        assert_eq!(fact.sessions, 0);
        assert!(!presenting(&fact, 1_050));
        assert!(!holds_picture(false, true, fact.last_presented != 0));
    }

    /// Stepping to another camera advances only the request, not the window.
    /// Late frames of the camera being left are refused, but the new session's
    /// first frame is taken and starts a fresh handover clock.
    #[test]
    fn a_new_camera_refuses_the_old_sessions_late_frames() {
        let mut fact = SurfaceFact::new();
        fact.window = 1;
        fact.generation = 4;
        fact.request = 9;
        assert!(rendered(&mut fact, 4, 9, 1_000, false));

        fact.request = 10;
        fact.last_presented = 0;
        assert!(!answers_surface(4, 9, &fact));
        assert!(!rendered(&mut fact, 4, 9, 1_050, true));
        assert_eq!(fact.last_presented, 0);

        assert!(rendered(&mut fact, 4, 10, 1_100, false), "the new camera's frame");
        assert_eq!(fact.last_presented, 1_100);
        assert_eq!(fact.sessions, 2, "the old session has not closed yet");
        assert!(presenting(&fact, 1_100));
    }

    /// `surfaceChanged` re-binds the same surface and must not restart a seek's
    /// handover. Stepping to the other camera's view is not a replacement
    /// either - both views are alive, and clearing the picture here is what
    /// made a channel step flash the wall's 480p fallback - but it does have
    /// to tell the codec to reopen on the new `ANativeWindow`.
    #[test]
    fn a_step_to_another_view_keeps_the_picture_and_reopens_the_codec() {
        let mut fact = SurfaceFact::new();
        let first = 1usize as *mut c_void;
        let other = 2usize as *mut c_void;
        assert!(replace_window(&mut fact, first, true, false));
        let generation = fact.generation;
        let request = fact.request;
        let epoch = fact.window_epoch;
        fact.last_presented = 1_000;
        fact.sessions = 1;

        assert!(!replace_window(&mut fact, first, false, false), "the same surface re-bound");
        assert_eq!(fact.generation, generation);
        assert_eq!(fact.window_epoch, epoch);
        assert_eq!(fact.last_presented, 1_000, "the picture survives the resize");

        assert!(!replace_window(&mut fact, other, false, true), "a switch is not a replacement");
        assert_eq!(fact.generation, generation, "no new generation");
        assert_eq!(fact.request, request, "the first frame of the new session still answers");
        assert_eq!(fact.last_presented, 1_000, "no 480p fallback is needed");
        assert_eq!(fact.sessions, 1, "the session is still drawing");
        assert_eq!(fact.window_epoch, epoch + 1, "the codec is reopened on the new view");
        assert!(answers_surface(generation, request, &fact));

        assert!(replace_window(&mut fact, std::ptr::null_mut(), true, false), "a real destroy");
        assert_eq!(fact.generation, generation + 1);
        assert_eq!(fact.window_epoch, epoch + 2);
        assert_eq!(fact.last_presented, 0);
        assert_eq!(fact.sessions, 0);
    }

    /// A session left over from a replaced window closes later. Its subtraction
    /// must not take the new generation's count below what it has.
    #[test]
    fn a_session_left_over_cannot_lower_the_new_windows_count() {
        let mut fact = SurfaceFact::new();
        fact.window = 2;
        fact.generation = 6;
        fact.sessions = 1;
        session_closed(&mut fact, 4);
        assert_eq!(fact.sessions, 1);
        session_closed(&mut fact, 6);
        assert_eq!(fact.sessions, 0);
        session_closed(&mut fact, 6);
        assert_eq!(fact.sessions, 0, "a second close cannot go negative");
    }

    /// The activity's window is created once and outlives its view: it is
    /// still there on a grid, with the view moved off screen. A stream wide
    /// enough for the surface is opened on a grid too - a tile pinned to the
    /// main stream - and a codec given that window would render into a view
    /// nobody is showing while the tile waits for a frame it never receives.
    #[test]
    fn a_window_without_a_magnified_view_is_not_an_active_layer() {
        assert!(!layer_active(false, false, true), "a grid has a window but no view");
        assert!(!layer_active(false, true, false), "a view with no window");
        assert!(!layer_active(true, true, true), "a turned picture suspends the path");
        assert!(layer_active(false, true, true), "a magnified picture on a surface");
    }

    /// The flag is read through the public answer the codec and the views ask,
    /// in both directions, and the other two terms still hold it down.
    #[test]
    fn the_magnified_view_is_what_makes_the_layer_active() {
        let window = 1usize as *mut c_void;
        set_surface_window(window, true, false);
        set_surface_suspended(false);
        set_surface_layer_wanted(false);
        assert!(!video_layer_active(), "the grid's window is not a picture layer");

        set_surface_layer_wanted(true);
        assert!(video_layer_active(), "the magnified view asks for the surface");
        set_surface_suspended(true);
        assert!(!video_layer_active(), "suspending the path wins over the view");
        set_surface_suspended(false);
        assert!(video_layer_active(), "the turn was undone");

        set_surface_window(std::ptr::null_mut(), true, false);
        assert!(!video_layer_active(), "the surface went away");
        set_surface_layer_wanted(false);
    }
}

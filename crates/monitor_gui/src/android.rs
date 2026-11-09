//! Android helpers that ask the activity to do what the native side cannot:
//! report the start-on-boot state, open the two system screens that grant it,
//! say which edge the navigation bar's strip is on, and export / import the
//! configuration through the public Downloads folder and the system's document
//! picker.
//!
//! Everything here calls a static method on `MainActivity` - see the matching
//! methods there - over the same JNI bridge `keyboard` uses, including the
//! reasoning for attaching the VM to the UI thread permanently. Only compiled
//! on Android.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI8, Ordering};
use std::sync::Mutex;

use jni::objects::{GlobalRef, JString, JValue};
use jni::JNIEnv;

/// The window's safe insets, in physical pixels, as last reported by the
/// activity: the strip the navigation bar keeps and the edge a notch covers -
/// the top while the wall stands upright, the bottom when the device is held the
/// other way up, the left or right while it lies on its side. Which edge the bar
/// lies on is the device's to decide - a landscape phone carries it along a
/// short edge, a tablet whose natural orientation is landscape along the bottom
/// one - so all four are reported and the wall leaves each of them free. The
/// notch's edge is reported even when the bars are hidden, because it is part of
/// the display the window still covers. Zero on a television and on an external
/// display, which have neither.
static INSET_LEFT_PX: AtomicI32 = AtomicI32::new(0);
static INSET_TOP_PX: AtomicI32 = AtomicI32::new(0);
static INSET_RIGHT_PX: AtomicI32 = AtomicI32::new(0);
static INSET_BOTTOM_PX: AtomicI32 = AtomicI32::new(0);

/// The activity, kept so the native window can be reached from the UI thread.
static APP: std::sync::OnceLock<android_activity::AndroidApp> = std::sync::OnceLock::new();

/// Remembers the activity, once, at startup.
pub fn set_native_app(app: android_activity::AndroidApp) {
    let _ = APP.set(app);
}

/// Moves the native window's buffer geometry to the size the display now has.
///
/// Android 8.x leaves that geometry at the orientation the surface was created
/// in when a turn is handled in place, and winit reads it back as the window
/// size. The activity calls this from `onConfigurationChanged`, then calls its
/// own `surfaceChanged` so the framework sees the difference and raises the
/// resize callback winit listens for. See `MainActivity.onConfigurationChanged`.
pub fn set_window_geometry(width: i32, height: i32) {
    let Some(app) = APP.get() else {
        return;
    };
    let Some(window) = app.native_window() else {
        return;
    };
    let (had_width, had_height) = (window.width(), window.height());
    if had_width == width && had_height == height {
        return;
    }
    match window.set_buffers_geometry(width, height, None) {
        Ok(()) => tracing::info!(
            target: "xgview",
            from = %format_args!("{had_width}x{had_height}"),
            to = %format_args!("{width}x{height}"),
            "the window's buffers follow the display"
        ),
        Err(error) => tracing::warn!(
            target: "xgview",
            %error,
            width,
            height,
            "cannot resize the window's buffers"
        ),
    }
}

/// Whether the Android context has been installed in this process.
///
/// `ndk_context::initialize_android_context` asserts that it is called exactly
/// once and **aborts the process** on a second call, and there are two callers
/// that cannot see each other: `android-activity`, which installs it before
/// `android_main` on the `NativeActivity` path, and [`install_activity`]. They
/// also both happen in one process more often than the design suggests - a boot
/// launch that lost the race and was retried into the other front end, a wall
/// started by hand beside one that was already up - and the `SurfaceView`'s own
/// `surfaceCreated` fires again after a relayout, which re-enters the same JNI
/// entry point. Whichever arrives first installs it; the other leaves it alone,
/// because the value is process-global and there is nothing better about the
/// second one.
static CONTEXT: std::sync::Once = std::sync::Once::new();

/// Installs the two things `android-activity` used to install for the wall.
///
/// The `NativeActivity` path gets them from `android-activity` before
/// `android_main` runs: a `GlobalRef` to the activity, which every call back
/// into Java goes through, and the Android context, which the audio sink's
/// Oboe backend asks for and which panics the process when it is missing. A
/// `SurfaceView` path has no `AndroidApp`, so the activity hands both over here.
pub fn install_activity(env: &mut JNIEnv, class: &jni::objects::JClass, activity: &jni::objects::JObject) {
    // The class and its VM are what the keyboard needs, and setting them twice
    // is harmless - the second is refused and dropped.
    crate::keyboard::java_ready(env, class);
    CONTEXT.call_once(|| install_context(env, activity));
}

/// Tells [`install_activity`] that `android-activity` has already been there.
///
/// Called first thing in `run_android`, which only ever runs on the
/// `NativeActivity` path - the context is installed before it, and claiming it
/// here is what keeps a `SurfaceView` started later in the same process from
/// installing it a second time and aborting.
pub fn native_activity_installed_context() {
    CONTEXT.call_once(|| {});
}

/// The one install, run at most once by [`CONTEXT`].
fn install_context(env: &JNIEnv, activity: &jni::objects::JObject) {
    let Ok(vm) = env.get_java_vm() else {
        tracing::warn!(target: "xgview", "no JavaVM; the audio sink will not find its context");
        return;
    };
    // A global reference, leaked on purpose: the context has to outlive this
    // call, and a local reference would be dead the moment it returned. The
    // pointer is taken first, then the reference itself is leaked - forgetting
    // the pointer would do nothing, because a pointer is `Copy`.
    let Ok(held) = env.new_global_ref(activity) else {
        tracing::warn!(target: "xgview", "cannot hold the activity; the audio sink has no context");
        return;
    };
    let context = held.as_obj().as_raw() as *mut std::ffi::c_void;
    std::mem::forget(held);
    // Safety: both pointers are valid for the life of the process - the VM is
    // the one this call came through, and the context is the global reference
    // just made.
    unsafe {
        ndk_context::initialize_android_context(vm.get_java_vm_pointer().cast(), context);
    }
}

/// Hands the decoder the surface a decoded picture is rendered into.
///
/// The wall keeps its own surface for the interface; this is the one underneath
/// it, and a codec given it renders straight into a buffer the compositor scans
/// out. A null window means the surface went away, and the next session falls
/// back to whatever it would otherwise use.
pub fn set_video_surface(window: *mut std::ffi::c_void) {
    monitor_codec::amediacodec::set_surface_window(window);
}

/// Shows or hides the view the picture is scanned out of.
///
/// A `SurfaceView` keeps the last frame it was given, and it is drawn over the
/// wall: left standing when the wall goes back to the grid, it covers it with a
/// still picture. There is no way to clear one - the buffer is the codec's, not
/// the app's - so the view goes away instead, and comes back when the magnified
/// viewport does.
pub fn set_video_visible(visible: bool) {
    call_void_bool("setVideoVisible", visible);
}

/// Places the view the picture is scanned out of, in physical pixels.
///
/// The view is a full-screen `SurfaceView` until this says otherwise, and a
/// codec rendering into a full-screen surface stretches its picture into the
/// screen's shape: the fit switch changed the wall's idea of where the picture
/// is and nothing on the screen. This is that idea, made real - the box
/// `XgViewApp::picture_rect` gives, which is where the wall would have drawn
/// the picture itself.
///
/// Only the box travels here; what the picture shows inside it does not. A
/// window a pinch pushes in is a crop of the picture, and moving in and out of
/// it will be a scale of this view - not another box, because a relayout
/// resizes the surface the codec is rendering into.
pub fn set_video_rect(x: f32, y: f32, width: f32, height: f32) {
    let (Some(class), Some(mut env)) = (class(), attach()) else {
        return;
    };
    let called = env.call_static_method(
        class,
        "setVideoRect",
        "(FFFF)V",
        &[
            JValue::Float(x),
            JValue::Float(y),
            JValue::Float(width),
            JValue::Float(height),
        ],
    );
    if let Err(error) = called {
        tracing::warn!(target: "xgview", %error, "the picture's rectangle was refused");
        env.exception_describe().ok();
    }
    // Cleared, as everywhere else here: an exception left pending aborts the
    // process on the next JNI call.
    clear_exception(&mut env);
}

/// Records the window insets the activity reported.
///
/// Called from the JNI entry point in `monitor_android`, on the UI thread.
pub fn set_insets(left_px: i32, top_px: i32, right_px: i32, bottom_px: i32) {
    INSET_LEFT_PX.store(left_px, Ordering::Relaxed);
    INSET_TOP_PX.store(top_px, Ordering::Relaxed);
    INSET_RIGHT_PX.store(right_px, Ordering::Relaxed);
    INSET_BOTTOM_PX.store(bottom_px, Ordering::Relaxed);
}

/// The window's safe insets in physical pixels: the navigation bar's strip and
/// the edge a notch covers. See the statics above.
pub fn insets_px() -> (i32, i32, i32, i32) {
    (
        INSET_LEFT_PX.load(Ordering::Relaxed).max(0),
        INSET_TOP_PX.load(Ordering::Relaxed).max(0),
        INSET_RIGHT_PX.load(Ordering::Relaxed).max(0),
        INSET_BOTTOM_PX.load(Ordering::Relaxed).max(0),
    )
}

/// The last value sent to [`set_reserve_navigation_bar`], so the activity is told
/// only when it changes. `-1` until the first call.
static RESERVE_SENT: AtomicI8 = AtomicI8::new(-1);

/// Tells the activity whether the navigation bar keeps its strip (`true`) or the
/// wall hides it (`false`). Only sent when it changes.
pub fn set_reserve_navigation_bar(reserve: bool) {
    let wanted = reserve as i8;
    if RESERVE_SENT.load(Ordering::Relaxed) == wanted {
        return;
    }
    let (Some(class), Some(mut env)) = (class(), attach()) else {
        // The activity has not called `nativeReady` yet: try again next frame.
        return;
    };
    let _ = env.call_static_method(
        class,
        "setReserveNavigationBar",
        "(Z)V",
        &[JValue::Bool(reserve as u8)],
    );
    RESERVE_SENT.store(wanted, Ordering::Relaxed);
}

/// The last orientation sent to the activity, so it is told only when it
/// changes. `-1` until the first call.
static ORIENTATION_SENT: AtomicI8 = AtomicI8::new(-1);

/// Tells the activity which way the wall should stand: `true` for portrait,
/// `false` for landscape. Only sent when it changes. Both honour the user's
/// rotation lock; see `MainActivity.setOrientation`.
pub fn set_orientation(portrait: bool) {
    let wanted = portrait as i8;
    let sent = ORIENTATION_SENT.load(Ordering::Relaxed);
    if sent == wanted {
        return;
    }
    let called = call_bool_bool("setOrientation", portrait);
    if called {
        ORIENTATION_SENT.store(wanted, Ordering::Relaxed);
    }
}

/// Forgets the last orientation sent, so the next frame sends it again.
///
/// Called when the activity resumes: a request made while nothing was resumed
/// found no activity and was not remembered as sent, and a platform may also
/// have dropped a request made from the background. Re-sending the orientation
/// the native side is actually laying out for is what puts the window back in
/// step, without keeping a second copy of that answer in Java.
pub fn reset_orientation() {
    ORIENTATION_SENT.store(-1, Ordering::Relaxed);
}

/// Set when the activity has come back to the foreground, cleared when the
/// application has acted on it. See [`request_resume`].
static RESUME_REQUESTED: AtomicBool = AtomicBool::new(false);

/// The activity came back to the foreground.
///
/// The channels may have spent the time in the background counting failures -
/// a Wi-Fi that slept, a socket that went half open - and grown their backoff
/// to its ceiling. [`take_resume`] is how the application learns to retry them
/// at once rather than wait that backoff out.
pub fn request_resume() {
    RESUME_REQUESTED.store(true, Ordering::Relaxed);
}

/// Whether the activity has come back since the last call, clearing the flag.
pub fn take_resume() -> bool {
    RESUME_REQUESTED.swap(false, Ordering::Relaxed)
}

/// Whether the device can stand upright - it has a touch screen and is not a
/// television. The wall offers no portrait mode where this is `false`.
pub fn supports_portrait() -> bool {
    call_bool("supportsPortrait")
}

/// Whether the activity was started by the boot receiver, as read from the
/// launch intent's `com.xhbl.xgview.FROM_AUTOSTART` extra; see `BootReceiver`.
pub fn from_autostart() -> bool {
    call_bool("getFromAutostart")
}

/// Whether XGView may draw over other apps.
///
/// This is the permission that lifts Android's refusal to start an activity
/// from the boot broadcast, so it is what the boot receiver needs to relaunch
/// the wall; see `BootReceiver` and the manifest.
pub fn overlay_allowed() -> bool {
    call_bool("overlayAllowed")
}

/// Whether XGView is the device's home app - the other way to start on boot,
/// needing no permission at all.
pub fn is_home_app() -> bool {
    call_bool("isHomeApp")
}

/// Opens the system screen that grants [`overlay_allowed`].
pub fn open_overlay_settings() {
    call_void("openOverlaySettings");
}

/// Asks the activity to bring itself back up a moment from now, in a fresh
/// process, and reports whether it agreed.
///
/// Called from the fallback the renderer's panic is caught in: the process is
/// about to exit and the wall has to come back without anyone touching the
/// remote, which an activity cannot do from a process that is going away. The
/// activity hands the relaunch to `AlarmManager`, whose alarm is held by the
/// system rather than by this process; see `MainActivity.restartSoon`.
///
/// `false` means no alarm was set: there was no activity to talk to (a panic
/// early enough that `onCreate` had not registered one), or the activity's own
/// accounting says the app is in a crash loop and another restart would only
/// feed it. Either way the caller has a process it cannot bring back.
pub fn restart_soon() -> bool {
    let (Some(class), Some(mut env)) = (class(), attach()) else {
        return false;
    };
    let scheduled = env
        .call_static_method(class, "restartSoon", "()Z", &[])
        .and_then(|value| value.z())
        .unwrap_or(false);
    // Cleared, as everywhere else here: an exception left pending aborts the
    // process on the next JNI call.
    clear_exception(&mut env);
    scheduled
}

/// Opens the system screen that chooses the device's home app.
pub fn open_home_settings() {
    call_void("openHomeSettings");
}

/// The text on the system clipboard, empty when there is none.
pub fn clipboard_text() -> String {
    let (Some(class), Some(mut env)) = (class(), attach()) else {
        return String::new();
    };
    let result = env
        .call_static_method(class, "getClipboardText", "()Ljava/lang/String;", &[])
        .and_then(|value| value.l());
    let text = match result {
        Ok(object) if !object.is_null() => {
            let string = JString::from(object);
            env.get_string(&string).map(String::from).unwrap_or_default()
        }
        _ => String::new(),
    };
    clear_exception(&mut env);
    text
}

/// Puts `text` on the system clipboard.
pub fn set_clipboard_text(text: &str) {
    let (Some(class), Some(mut env)) = (class(), attach()) else {
        return;
    };
    let Ok(string) = env.new_string(text) else {
        return;
    };
    let _ = env.call_static_method(
        class,
        "setClipboardText",
        "(Ljava/lang/String;)V",
        &[JValue::Object(&string)],
    );
    clear_exception(&mut env);
}

/// Opens a URL (`http:`, `mailto:`, ...) in whatever application handles it.
///
/// egui's own opener is compiled out on Android - eframe is built without its
/// `webbrowser` feature - so a link clicked on the About tab would otherwise do
/// nothing.
pub fn open_uri(url: &str) {
    let (Some(class), Some(mut env)) = (class(), attach()) else {
        return;
    };
    let Ok(string) = env.new_string(url) else {
        return;
    };
    let _ = env.call_static_method(
        class,
        "openUri",
        "(Ljava/lang/String;)V",
        &[JValue::Object(&string)],
    );
    clear_exception(&mut env);
}

/// A Java exception left pending aborts the process on the next JNI call - see
/// the note on `keyboard::ACTIVITY` - so one is cleared rather than kept.
fn clear_exception(env: &mut JNIEnv) {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
}

fn call_bool(method: &str) -> bool {
    let (Some(class), Some(mut env)) = (class(), attach()) else {
        return false;
    };
    env.call_static_method(class, method, "()Z", &[])
        .and_then(|value| value.z())
        .unwrap_or(false)
}

fn call_void(method: &str) {
    let (Some(class), Some(mut env)) = (class(), attach()) else {
        return;
    };
    let _ = env.call_static_method(class, method, "()V", &[]);
}

/// Asks Android to keep the CPU running and/or the screen on.
///
/// Installed as `monitor_core::power`'s Android implementation, because the two
/// halves are Java's - a `PARTIAL_WAKE_LOCK` from `PowerManager`, and
/// `FLAG_KEEP_SCREEN_ON` on the activity's window - and this is where the
/// bridge to the activity lives.
///
/// A call that did not reach the activity is reported rather than swallowed:
/// the two are separate requests on this platform, the panel shows the failure
/// under the switches, and a wake request that silently did nothing is the one
/// thing this module exists to avoid.
pub fn set_keep_awake(system: bool, display: bool) -> monitor_core::Result<()> {
    let sleep = call_void_bool("setPreventSleep", system);
    let screen = call_void_bool("setKeepScreenOn", display);
    if sleep && screen {
        return Ok(());
    }
    Err(monitor_core::CoreError::unsupported(
        "the activity did not take the wake request",
    ))
}

/// Calls a Java static method taking one `boolean`, reporting whether it ran.
fn call_void_bool(method: &str, value: bool) -> bool {
    let (Some(class), Some(mut env)) = (class(), attach()) else {
        return false;
    };
    let called = env
        .call_static_method(class, method, "(Z)V", &[JValue::Bool(u8::from(value))])
        .is_ok();
    // Cleared, as everywhere else here: an exception left pending aborts the
    // process on the next JNI call.
    clear_exception(&mut env);
    called
}

/// Calls a Java static method taking and returning a `boolean`.
fn call_bool_bool(method: &str, value: bool) -> bool {
    let (Some(class), Some(mut env)) = (class(), attach()) else {
        return false;
    };
    let answer = env
        .call_static_method(class, method, "(Z)Z", &[JValue::Bool(u8::from(value))])
        .and_then(|value| value.z())
        .unwrap_or(false);
    clear_exception(&mut env);
    answer
}

fn class() -> Option<&'static GlobalRef> {
    crate::keyboard::activity_class()
}

fn attach() -> Option<JNIEnv<'static>> {
    // Permanently, not `attach_current_thread`: this runs on the UI thread,
    // which the runtime owns. See `keyboard::set_wanted` for what the plain
    // form would do to the process on the next Java call.
    crate::keyboard::vm()?.attach_current_thread_permanently().ok()
}

/// Where an export ended up, or why it could not even be attempted.
pub enum ExportOutcome {
    /// Written; the path to show the viewer.
    Written(String),
    /// Android 9 has no `MediaStore` route into the public Downloads folder, so
    /// the write needs `WRITE_EXTERNAL_STORAGE`. The system dialog has been
    /// asked for, and the export is one press away once it is answered.
    NeedsStoragePermission,
}

/// Writes the exported configuration into the public `Download/xgview` folder.
///
/// The write itself is the activity's: `MediaStore` from Android 10 on, and a
/// plain file where there is no `MediaStore` route. Fails only when the
/// activity has something to say about it; the missing Android 9 permission is
/// reported as [`ExportOutcome::NeedsStoragePermission`] rather than an error.
pub fn write_download(name: &str, text: &str) -> Result<ExportOutcome, String> {
    let (Some(class), Some(mut env)) = (class(), attach()) else {
        return Err("the activity is not up".to_owned());
    };
    let (Ok(name), Ok(text)) = (env.new_string(name), env.new_string(text)) else {
        return Err("cannot hand the configuration to the activity".to_owned());
    };
    let result = env.call_static_method(
        class,
        "writeDownloadFile",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        &[JValue::Object(&name), JValue::Object(&text)],
    );
    let path = match result {
        Ok(value) => match value.l() {
            // A null answer is the Android 9 storage permission having been
            // asked for, not a failure; see `MainActivity.writeDownloadFile`.
            Ok(object) if object.is_null() => None,
            Ok(object) => {
                let object = JString::from(object);
                let path = match env.get_string(&object) {
                    Ok(path) => Some(String::from(path)),
                    Err(error) => {
                        return Err(take_exception(&mut env).unwrap_or_else(|| error.to_string()));
                    }
                };
                path
            }
            Err(error) => return Err(take_exception(&mut env).unwrap_or_else(|| error.to_string())),
        },
        Err(error) => return Err(take_exception(&mut env).unwrap_or_else(|| error.to_string())),
    };
    Ok(match path {
        Some(path) => ExportOutcome::Written(path),
        None => ExportOutcome::NeedsStoragePermission,
    })
}

/// Whether the device has a document picker at all.
///
/// A television build can ship without `DocumentsUI`, and - as on the boxes
/// this was written for - without a single `DocumentsProvider` behind it,
/// while still answering the intent with the Android TV framework's
/// placeholder, which opens nothing. That is not a picker, and
/// [`Self::pick_config_file`] on such a device leaves the viewer looking at a
/// screen that does nothing. What asks is the activity; see
/// `MainActivity.hasDocumentPicker`.
pub fn has_document_picker() -> bool {
    call_bool("hasDocumentPicker")
}

/// Asks the activity to open the system's document picker for an import.
///
/// The answer comes back later, through [`java_config_picked`] and
/// [`take_picked_config`]: a picker is not something to wait for inside a
/// frame.
pub fn pick_config_file() {
    let (Some(class), Some(mut env)) = (class(), attach()) else {
        return;
    };
    let _ = env.call_static_method(class, "pickConfigFile", "()V", &[]);
    clear_exception(&mut env);
}

/// The app's own folder on the shared volume, remembered from the entry point.
///
/// `/storage/emulated/0/Android/data/<package>/files`: the one place a file
/// transfer - or the viewer's own file dialog - can put a document that the
/// app may read back without a permission, on every Android version. `None`
/// when the entry point never had one to hand over, which is what the viewer
/// falls back to its current directory for.
static EXTERNAL_FILES: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Hands the folder over, once, from the entry point.
pub fn set_external_files(dir: impl Into<PathBuf>) {
    let _ = EXTERNAL_FILES.set(dir.into());
}

/// That folder, if the entry point handed one over.
pub fn external_files_dir() -> Option<PathBuf> {
    EXTERNAL_FILES.get().cloned()
}

/// What the document picker answered since the last frame.
pub enum Picked {
    /// The text of the document the viewer chose.
    Config(String),
    /// The viewer backed out; nothing happened, and nothing is said about it.
    Cancelled,
    /// The document could not be read.
    Failed(String),
}

/// The picker's answer, waiting for the next frame to pick it up.
///
/// A slot rather than a queue: the picker is modal, so one answer is in flight
/// at a time, and a second would only overwrite the first.
static PICKED: Mutex<Option<Picked>> = Mutex::new(None);

/// The document an import picked, or why it could not be read.
///
/// Called from the JNI entry point in `monitor_android`; see
/// `MainActivity.onActivityResult`.
pub fn java_config_picked(env: &mut JNIEnv, text: &JString, error: &JString) {
    let text = jstring(env, text);
    let error = jstring(env, error);
    let picked = match (text, error) {
        (Some(text), _) => Picked::Config(text),
        (None, Some(error)) => Picked::Failed(error),
        (None, None) => Picked::Cancelled,
    };
    if let Ok(mut slot) = PICKED.lock() {
        *slot = Some(picked);
    }
    crate::keyboard::wake();
}

/// Takes the picker's answer, if one has arrived.
pub fn take_picked_config() -> Option<Picked> {
    PICKED.lock().ok().and_then(|mut slot| slot.take())
}

/// A Java string, or `None` when the reference is null.
fn jstring(env: &mut JNIEnv, value: &JString) -> Option<String> {
    if value.is_null() {
        return None;
    }
    env.get_string(value).ok().map(String::from)
}

/// Whether XGView may use the microphone, asking for the permission when it is
/// missing.
///
/// `true` means talk-back can open its capture now. `false` means the system
/// dialog has just been put up, and the viewer presses the switch again once it
/// has been answered - the same shape as the export's storage permission on
/// Android 9. See `MainActivity.requestRecordAudio`.
pub fn request_record_audio() -> bool {
    call_bool("requestRecordAudio")
}

/// What the activity's capture answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TalkCapture {
    /// The microphone permission is not granted yet; the dialog has been asked
    /// for, and the viewer presses the switch again once it is answered.
    Permission,
    /// The capture could not be opened: no device, or the audio service refused.
    NoDevice,
    /// The capture is open. `cancelled` says whether the platform's echo
    /// canceller came with it.
    Opened { cancelled: bool },
}

/// Opens the microphone for a call, with Android's own echo canceller.
///
/// The capture itself is Java's: an `AudioRecord` on the `VOICE_COMMUNICATION`
/// source, the one a device processes for speech, drained by a thread that hands
/// its samples back through `MainActivity.nativeTalkPcm` and into
/// [`crate::mic`], which pushes them into the talk hub exactly as the `cpal`
/// capture does elsewhere. See `MainActivity.startTalkCapture`.
pub fn start_talk_capture() -> TalkCapture {
    match call_int("startTalkCapture") {
        Some(-1) => TalkCapture::Permission,
        Some(answer) if answer >= 0 => TalkCapture::Opened { cancelled: answer > 0 },
        _ => TalkCapture::NoDevice,
    }
}

/// Stops a call's capture. Safe when no call is up.
pub fn stop_talk_capture() {
    call_void("stopTalkCapture");
}

/// Opens the playback the camera's sound leaves through, on the path the
/// platform's echo canceller has a reference signal for.
///
/// An `AudioTrack` with `USAGE_VOICE_COMMUNICATION`, at the source's own rate and
/// channel count: the other half of the arrangement [`start_talk_capture`] makes
/// on the input side. See `MainActivity.startTalkPlayback`.
pub fn start_playback(rate: u32, channels: u32) -> bool {
    call_bool_int_int("startTalkPlayback", rate as i32, channels as i32)
}

/// Hands one block of 16 bit interleaved samples to the track. Non-blocking on
/// the Java side: a full track drops the block rather than stalling the worker,
/// which is also pumping the picture.
pub fn push_playback(samples: &[i16]) {
    let (Some(class), Some(mut env)) = (class(), attach()) else {
        return;
    };
    let count = samples.len().min(i32::MAX as usize);
    let Ok(array) = env.new_short_array(count as i32) else {
        clear_exception(&mut env);
        return;
    };
    if env.set_short_array_region(&array, 0, &samples[..count]).is_err() {
        clear_exception(&mut env);
        return;
    }
    let _ = env.call_static_method(
        class,
        "pushTalkPlayback",
        "([SI)V",
        &[JValue::Object(&array), JValue::Int(count as i32)],
    );
    clear_exception(&mut env);
}

/// The playback's volume, `0.0..=1.0`.
pub fn set_playback_volume(volume: f32) {
    let (Some(class), Some(mut env)) = (class(), attach()) else {
        return;
    };
    let _ = env.call_static_method(
        class,
        "setTalkPlaybackVolume",
        "(F)V",
        &[JValue::Float(volume.clamp(0.0, 1.0))],
    );
    clear_exception(&mut env);
}

/// Closes the playback.
pub fn stop_playback() {
    call_void("stopTalkPlayback");
}

/// Calls a Java static method taking two `int`s and answering a `boolean`.
fn call_bool_int_int(method: &str, first: i32, second: i32) -> bool {
    let (Some(class), Some(mut env)) = (class(), attach()) else {
        return false;
    };
    let answer = env
        .call_static_method(class, method, "(II)Z", &[JValue::Int(first), JValue::Int(second)])
        .and_then(|value| value.z())
        .unwrap_or(false);
    clear_exception(&mut env);
    answer
}

/// Calls a Java static method taking nothing and answering an `int`.
fn call_int(method: &str) -> Option<i32> {
    let (Some(class), Some(mut env)) = (class(), attach()) else {
        return None;
    };
    let answer = env
        .call_static_method(class, method, "()I", &[])
        .and_then(|value| value.i())
        .ok();
    // Cleared, as everywhere here: an exception left pending aborts the next
    // JNI call.
    clear_exception(&mut env);
    answer
}

/// The message of a pending Java exception, taken out of the way.
///
/// A pending exception aborts the process on the next JNI call - see the note
/// on [`crate::keyboard`] - so one is always cleared, message or not.
fn take_exception(env: &mut JNIEnv) -> Option<String> {
    let throwable = env.exception_occurred().ok()?;
    let _ = env.exception_clear();
    if throwable.is_null() {
        return None;
    }
    let message = env
        .call_method(&throwable, "getMessage", "()Ljava/lang/String;", &[])
        .ok()
        .and_then(|value| value.l().ok())
        .filter(|object| !object.is_null())
        .and_then(|object| {
            let object = JString::from(object);
            let text = env.get_string(&object).ok().map(String::from);
            text
        });
    // `getMessage` may itself have thrown; leave nothing pending either way.
    let _ = env.exception_clear();
    message
}


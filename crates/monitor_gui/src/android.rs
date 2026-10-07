//! Android helpers that ask the activity to do what the native side cannot:
//! report the start-on-boot state, open the two system screens that grant it,
//! say how wide the navigation bar's strip is, and export / import the
//! configuration through the public Downloads folder and the system's document
//! picker.
//!
//! Everything here calls a static method on `MainActivity` - see the matching
//! methods there - over the same JNI bridge `keyboard` uses, including the
//! reasoning for attaching the VM to the UI thread permanently. Only compiled
//! on Android.

use std::sync::atomic::{AtomicI32, AtomicI8, Ordering};
use std::sync::Mutex;

use jni::objects::{GlobalRef, JString, JValue};
use jni::JNIEnv;

/// The horizontal window insets, in physical pixels, as last reported by the
/// activity: the navigation bar's strip. Zero on a television and on an external
/// display, which have no navigation bar.
static INSET_LEFT_PX: AtomicI32 = AtomicI32::new(0);
static INSET_RIGHT_PX: AtomicI32 = AtomicI32::new(0);

/// Records the horizontal insets the activity reported.
///
/// Called from the JNI entry point in `monitor_android`, on the UI thread.
pub fn set_horizontal_insets(left_px: i32, right_px: i32) {
    INSET_LEFT_PX.store(left_px, Ordering::Relaxed);
    INSET_RIGHT_PX.store(right_px, Ordering::Relaxed);
}

/// The horizontal window insets, in physical pixels, as last reported by the
/// activity: the navigation bar's strip. Zero on a television and on an external
/// display, which have no navigation bar.
pub fn horizontal_insets_px() -> (i32, i32) {
    (
        INSET_LEFT_PX.load(Ordering::Relaxed).max(0),
        INSET_RIGHT_PX.load(Ordering::Relaxed).max(0),
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


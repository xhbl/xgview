//! Android helpers that ask the activity to do what the native side cannot:
//! report the start-on-boot state, open the two system screens that grant it,
//! and say how wide the navigation bar's strip is.
//!
//! Everything here calls a static method on `MainActivity` - see the matching
//! methods there - over the same JNI bridge `keyboard` uses, including the
//! reasoning for attaching the VM to the UI thread permanently. Only compiled
//! on Android.

use std::sync::atomic::{AtomicI32, AtomicI8, Ordering};

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

fn class() -> Option<&'static GlobalRef> {
    crate::keyboard::activity_class()
}

fn attach() -> Option<JNIEnv<'static>> {
    // Permanently, not `attach_current_thread`: this runs on the UI thread,
    // which the runtime owns. See `keyboard::set_wanted` for what the plain
    // form would do to the process on the next Java call.
    crate::keyboard::vm()?.attach_current_thread_permanently().ok()
}

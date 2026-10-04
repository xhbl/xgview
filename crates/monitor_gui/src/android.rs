//! Android helpers that ask the activity to do what the native side cannot:
//! report the start-on-boot state, and open the two system screens that grant
//! it.
//!
//! Everything here calls a static method on `MainActivity` - see the matching
//! methods there - over the same JNI bridge `keyboard` uses, including the
//! reasoning for attaching the VM to the UI thread permanently. Only compiled
//! on Android.

use jni::objects::GlobalRef;
use jni::JNIEnv;

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

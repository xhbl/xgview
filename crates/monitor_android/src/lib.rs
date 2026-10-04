//! Android entry point (NativeActivity) for XGView.
//!
//! On Android the crate is compiled as a `cdylib` loaded by the activity
//! declared in `android/AndroidManifest.xml`. The boot receiver defined there
//! relaunches the activity after `BOOT_COMPLETED`.

/// Starts the eframe application from the Android activity.
///
/// A viewer that fails to start is over: the activity finishes and the screen
/// falls back to the launcher, which looks exactly like a crash with nothing to
/// read. The error is logged rather than dropped - `run_android` installs the
/// logcat logger before anything can fail, so it reaches `adb logcat -s xgview`.
#[cfg(target_os = "android")]
#[no_mangle]
// `AndroidApp` is not `repr(C)`, but this is the signature android-activity
// looks up by name and calls; it cannot be changed from here.
#[allow(improper_ctypes_definitions)]
pub extern "C" fn android_main(app: android_activity::AndroidApp) {
    if let Err(error) = monitor_gui::run_android(app) {
        tracing::error!(target: "xgview", %error, "the viewer stopped");
    }
}

/// Non Android targets expose the same symbol as a no-op so that the crate can
/// be part of the workspace on desktop builds.
#[cfg(not(target_os = "android"))]
pub fn android_main() {}

// The soft keyboard's text: the activity's input connection calls these (see
// `MainActivity.installTextInput`), and they hand what was typed to
// `monitor_gui::keyboard`, which turns it into egui's own events.
//
// They live here rather than in `monitor_gui` because a `#[no_mangle]` symbol
// exported from a dependency crate is hidden by the linker: kept there, the
// activity found no implementation and died of `UnsatisfiedLinkError` in
// `onCreate`, before the first frame.

/// The activity is up and can be talked to.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_MainActivity_nativeReady(
    env: jni::JNIEnv,
    class: jni::objects::JClass,
) {
    monitor_gui::keyboard::java_ready(&env, &class);
}

/// The keyboard typed something.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_MainActivity_nativeCommit(
    mut env: jni::JNIEnv,
    _class: jni::objects::JClass,
    text: jni::objects::JString,
) {
    monitor_gui::keyboard::java_commit(&mut env, &text);
}

/// The keyboard deleted some characters.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_MainActivity_nativeBackspace(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
    count: jni::sys::jint,
) {
    monitor_gui::keyboard::java_backspace(count);
}

/// The keyboard's Done / Enter key was pressed.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_MainActivity_nativeEnter(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
) {
    monitor_gui::keyboard::java_enter();
}

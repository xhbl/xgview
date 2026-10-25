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

/// The keyboard went away without the application asking: the viewer pressed
/// Back, or the input method's own hide button.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_MainActivity_nativeKeyboardHidden(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
) {
    monitor_gui::keyboard::java_keyboard_hidden();
}

/// The document an import picked, or why it could not be read. See
/// `MainActivity.onActivityResult`.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_MainActivity_nativeConfigPicked(
    mut env: jni::JNIEnv,
    _class: jni::objects::JClass,
    text: jni::objects::JString,
    error: jni::objects::JString,
) {
    monitor_gui::android::java_config_picked(&mut env, &text, &error);
}

/// The window insets changed: the strip the navigation bar keeps, one figure
/// per edge, in pixels. See `monitor_gui::android`.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_MainActivity_nativeInsets(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
    left: jni::sys::jint,
    top: jni::sys::jint,
    right: jni::sys::jint,
    bottom: jni::sys::jint,
) {
    monitor_gui::android::set_insets(left, top, right, bottom);
}

/// The activity turned: the display's new size, in pixels. See
/// `monitor_gui::android::set_window_geometry`.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_MainActivity_nativeWindowGeometry(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
    width: jni::sys::jint,
    height: jni::sys::jint,
) {
    monitor_gui::android::set_window_geometry(width, height);
}

/// The activity resumed: send the orientation the wall is laid out for again.
/// See `monitor_gui::android::reset_orientation`.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_MainActivity_nativeResumed(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
) {
    monitor_gui::android::reset_orientation();
}

/// A block of a call's capture, handed back by the activity's record thread.
///
/// `samples` is 16 bit mono PCM, `count` how many of it are valid and `rate` the
/// rate the device recorded at; the downmix to the back channel's 8 kHz is
/// `monitor_gui::mic`'s, the same one the other platforms' capture goes through.
/// See `MainActivity.startTalkCapture` and `monitor_gui::mic::android_push`.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_MainActivity_nativeTalkPcm(
    mut env: jni::JNIEnv,
    _class: jni::objects::JClass,
    samples: jni::objects::JShortArray,
    count: jni::sys::jint,
    rate: jni::sys::jint,
) {
    // A block is a fiftieth of a second; anything far larger is a caller that
    // went wrong, and is refused rather than allocated.
    let count = count.clamp(0, 96_000) as usize;
    if count == 0 {
        return;
    }
    let mut block = vec![0i16; count];
    if env.get_short_array_region(&samples, 0, &mut block).is_err() {
        // A pending exception aborts the next JNI call - see the note on the
        // keyboard's entry points - so it is cleared here.
        let _ = env.exception_clear();
        return;
    }
    monitor_gui::mic::android_push(&block, rate.max(0) as u32);
}

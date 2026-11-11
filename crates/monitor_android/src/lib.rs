//! Android entry point for XGView.
//!
//! On Android the crate is compiled as a `cdylib` loaded by the activity the
//! launcher starts, and it exports the JNI symbols those activities call back
//! into. Two front ends share it: `MainActivity`, the `NativeActivity` whose
//! window is the whole screen, and `SurfaceActivity`, an ordinary activity
//! where the picture and the overlay are surfaces of their own - see
//! `monitor_gui::android_surface`. The boot receiver defined in
//! `android/AndroidManifest.xml` relaunches whichever of them the shape it was
//! started in asks for after `BOOT_COMPLETED`.

// Structural guard: the two front ends must not grow separate copies of the
// soft keyboard or the language-pack unpacker again. See `parity_tests`.
#[cfg(test)]
mod parity_tests;

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

// The same four, for the wall on surfaces of its own. JNI finds a method by the
// class it is declared on, so a second front end needs its own exports even
// though they do the same thing - the input method is the activity's, and so is
// the view it types into.

/// The keyboard typed something; see `SurfaceActivity`.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_SurfaceActivity_nativeCommit(
    mut env: jni::JNIEnv,
    _class: jni::objects::JClass,
    text: jni::objects::JString,
) {
    monitor_gui::keyboard::java_commit(&mut env, &text);
}

/// The keyboard deleted some characters; see `SurfaceActivity`.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_SurfaceActivity_nativeBackspace(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
    count: jni::sys::jint,
) {
    monitor_gui::keyboard::java_backspace(count);
}

/// The keyboard's Done / Enter key was pressed; see `SurfaceActivity`.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_SurfaceActivity_nativeEnter(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
) {
    monitor_gui::keyboard::java_enter();
}

/// The keyboard went away unasked; see `SurfaceActivity`.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_SurfaceActivity_nativeKeyboardHidden(
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
    // The channels may have grown their backoff while the process was in the
    // background; the application retries them on the next frame.
    monitor_gui::android::request_resume();
}
// `ANativeWindow_fromSurface`, which gives a window this side owns.
#[cfg(target_os = "android")]
#[link(name = "android")]
extern "C" {
    fn ANativeWindow_fromSurface(
        env: *mut jni::sys::JNIEnv,
        surface: jni::sys::jobject,
    ) -> *mut std::ffi::c_void;
}

/// The wall, on a `SurfaceView` the activity owns.
///
/// This is the entry `SurfaceActivity` uses instead of the `NativeActivity`
/// path: the window is the view's surface, and everything eframe used to supply
/// (the loop, the surface, the input) comes from here. See
/// `monitor_gui::android_surface` and `priv/docs/android-surface-plan.md`.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_SurfaceActivity_nativeStartWall(
    mut env: jni::JNIEnv,
    _class: jni::objects::JClass,
    surface: jni::objects::JObject,
    internal: jni::objects::JString,
    external: jni::objects::JString,
    density: jni::sys::jfloat,
    activity: jni::objects::JObject,
    bridge: jni::objects::JClass,
) -> jni::sys::jlong {
    if surface.is_null() {
        return 0;
    }
    // What `android-activity` installs before `android_main`: the activity every
    // call back into Java goes through, and the Android context the audio sink
    // asks for. The class the calls go to is `MainActivity`'s, which is where
    // the static methods the wall uses live; this activity has none of them.
    monitor_gui::android::install_activity(&mut env, &bridge, &activity);
    let internal = env.get_string(&internal).map(String::from).unwrap_or_default();
    let external = env.get_string(&external).map(String::from).unwrap_or_default();
    let window =
        unsafe { ANativeWindow_fromSurface(env.get_native_interface(), surface.as_raw()) };
    monitor_gui::android_surface::start(window, internal, external, density) as jni::sys::jlong
}

/// The wall's surface changed size. `token` says which activity's surface it
/// was, so a late callback from one that is gone is refused. See
/// `monitor_gui::android::SurfaceTicket`.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_SurfaceActivity_nativeResizeWall(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
    token: jni::sys::jlong,
    width: jni::sys::jint,
    height: jni::sys::jint,
) {
    monitor_gui::android_surface::resize(token as u64, width, height);
}

/// The wall's surface is gone: stop drawing into it, if it is still ours.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_SurfaceActivity_nativeStopWall(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
    token: jni::sys::jlong,
) {
    monitor_gui::android_surface::stop(token as u64);
}

/// Whether the activity's window has focus.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_SurfaceActivity_nativeWallFocus(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
    focused: jni::sys::jboolean,
) {
    monitor_gui::android_surface::set_focused(focused != 0);
}

/// One key, pressed or released, with the modifiers held at the time and the
/// character the device's key map made of it (`KeyEvent.getUnicodeChar()`).
///
/// The codes are Android's (`KeyEvent.KEYCODE_*`); the translation to egui's
/// keys and text is here rather than in Java so that the one place that knows
/// what the interface listens for is the side that draws it.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_SurfaceActivity_nativeKey(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
    code: jni::sys::jint,
    pressed: jni::sys::jboolean,
    repeat: jni::sys::jboolean,
    meta: jni::sys::jint,
    unicode: jni::sys::jint,
) -> jni::sys::jboolean {
    u8::from(monitor_gui::android_surface::push_key(
        code,
        pressed != 0,
        repeat != 0,
        meta,
        unicode,
    ))
}

/// One touch pointer, in its own phase.
///
/// `phase` is 0 start, 1 move, 2 end, 3 cancel; `id` is the pointer's id from
/// `MotionEvent`; the position is in pixels and the pressure is `getPressure`.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_SurfaceActivity_nativeTouch(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
    phase: jni::sys::jint,
    id: jni::sys::jint,
    x: jni::sys::jfloat,
    y: jni::sys::jfloat,
    force: jni::sys::jfloat,
) {
    monitor_gui::android_surface::push_touch(phase, id, x, y, force);
}

/// A wheel or trackpad scroll, in pixels of travel.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_SurfaceActivity_nativeScroll(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
    dx: jni::sys::jfloat,
    dy: jni::sys::jfloat,
) {
    monitor_gui::android_surface::push_scroll(dx, dy);
}
/// The bar's own surface, over the picture.
///
/// A rectangle of its own, opaque, on top of the picture - which is how a
/// control bar sits over a decoded frame without anything having to be
/// see-through. See `monitor_gui::android_surface::start_bar`.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_SurfaceActivity_nativeStartBar(
    env: jni::JNIEnv,
    _class: jni::objects::JClass,
    surface: jni::objects::JObject,
    width: jni::sys::jint,
    height: jni::sys::jint,
) -> jni::sys::jlong {
    if surface.is_null() {
        return 0;
    }
    let window = unsafe { ANativeWindow_fromSurface(env.get_native_interface(), surface.as_raw()) };
    monitor_gui::android_surface::start_bar(window, width, height) as jni::sys::jlong
}

/// The bar's surface is gone, if it is still ours.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_SurfaceActivity_nativeStopBar(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
    token: jni::sys::jlong,
) {
    monitor_gui::android_surface::stop_bar(token as u64);
}

/// The surface a decoded picture is rendered into, under the wall.
///
/// The codec is configured with this window rather than with an image reader,
/// so the picture is scanned out by the compositor and never read back. See
/// `monitor_gui::android::set_video_surface`.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_SurfaceActivity_nativeStartVideo(
    env: jni::JNIEnv,
    _class: jni::objects::JClass,
    surface: jni::objects::JObject,
    mode: jni::sys::jint,
) -> jni::sys::jlong {
    if surface.is_null() {
        return 0;
    }
    let window = unsafe { ANativeWindow_fromSurface(env.get_native_interface(), surface.as_raw()) };
    monitor_gui::android::start_video_surface(window, mode) as jni::sys::jlong
}

/// The picture's surface is gone, if it is still ours.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_xhbl_xgview_SurfaceActivity_nativeStopVideo(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
    token: jni::sys::jlong,
) {
    monitor_gui::android::stop_video_surface(token as u64);
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
    env: jni::JNIEnv,
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

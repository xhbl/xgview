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
pub extern "C" fn android_main(app: android_activity::AndroidApp) {
    if let Err(error) = monitor_gui::run_android(app) {
        tracing::error!(target: "xgview", %error, "the viewer stopped");
    }
}

/// Non Android targets expose the same symbol as a no-op so that the crate can
/// be part of the workspace on desktop builds.
#[cfg(not(target_os = "android"))]
pub fn android_main() {}

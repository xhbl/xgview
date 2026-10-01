//! Android entry point (NativeActivity) for XGView.
//!
//! On Android the crate is compiled as a `cdylib` loaded by the activity
//! declared in `android/AndroidManifest.xml`. The boot receiver defined there
//! relaunches the activity after `BOOT_COMPLETED`.

/// Starts the eframe application from the Android activity.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "C" fn android_main(app: android_activity::AndroidApp) {
    monitor_gui::run_android(app);
}

/// Non Android targets expose the same symbol as a no-op so that the crate can
/// be part of the workspace on desktop builds.
#[cfg(not(target_os = "android"))]
pub fn android_main() {}

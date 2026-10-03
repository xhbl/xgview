// The APK packaging of the XGView native library.
//
// The module holds no sources of its own: it compiles the `BootReceiver` the
// repository carries, packages the manifest and resources next to it, and drops
// the `libmonitor_android.so` cargo-ndk writes into `android-build/jniLibs`
// under `lib/arm64-v8a/`. The library name must keep matching
// `android.app.lib_name` in the manifest.
plugins {
    id("com.android.application")
}

android {
    namespace = "com.xhbl.xgview"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.xhbl.xgview"
        // The SOW targets Amlogic / Shield class boxes at API 28 or newer.
        //
        // An API 22 box (Mi Box 3 Pro / MT8693, Android 5.1) installs and starts
        // - the native library only needs libraries API 21 ships - but its GLES
        // driver has none of the object-label entry points wgpu's GL backend
        // calls (`glObjectLabel`), and it has no Vulkan to fall back on, so the
        // renderer aborts on its first labelled resource. Asking wgpu for a GLES
        // 3.1 context instead of the default 3.0 (`WGPU_GLES_MINOR_VERSION=1`)
        // was tried: the driver still does not offer the entry point. The floor
        // therefore stays at 28 rather than letting a device install an app that
        // cannot draw.
        minSdk = 28
        targetSdk = 34
        versionCode = 1
        versionName = "1.0.0"
    }

    sourceSets {
        getByName("main") {
            manifest.srcFile("../../crates/monitor_android/android/AndroidManifest.xml")
            java.srcDirs("../../crates/monitor_android/android/java")
            res.srcDirs("../../crates/monitor_android/android/res")
            // Populated by scripts/build-android.sh (cargo-ndk).
            jniLibs.srcDirs("../../android-build/jniLibs")
        }
    }

    buildTypes {
        getByName("debug") {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

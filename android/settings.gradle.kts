// The Android application project around the `monitor_android` cdylib.
//
// It exists to turn the shared library cargo-ndk produces into an installable
// APK: the module points at the manifest, Java sources and resources the
// repository already carries under `crates/monitor_android/android`, and at the
// `jniLibs` directory cargo-ndk writes to, so nothing is copied around.
pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "xgview"
include(":app")

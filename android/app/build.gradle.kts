import java.util.Properties

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

// Release signing.
//
// The keystore and its passwords live with the repository -
// `android/xgview-release.jks` and the `android/keystore.properties` that says
// how to unlock it - and `scripts/build-android.ps1` creates both, with a
// generated password, the first time a release APK is built. Without the
// properties the release build falls back to the debug key: that still
// installs, but a different key can neither be published nor upgrade a
// release-signed installation.
val keystorePropertiesFile = rootProject.file("keystore.properties")
val keystoreProperties = Properties()
if (keystorePropertiesFile.exists()) {
    keystorePropertiesFile.inputStream().use { keystoreProperties.load(it) }
}

// The version is declared once, in the workspace manifest the Rust crates share
// (`[workspace.package] version` in the repository root Cargo.toml), and is read
// here so that the version the platform reports and the one the file name
// carries cannot drift apart. Gradle has no view of a Cargo workspace, so the
// line is found by hand.
val cargoVersion: String = run {
    val manifest = rootProject.file("../Cargo.toml")
    var inWorkspacePackage = false
    var found: String? = null
    manifest.forEachLine { raw ->
        val line = raw.trim()
        when {
            line == "[workspace.package]" -> inWorkspacePackage = true
            inWorkspacePackage && line.startsWith("[") -> inWorkspacePackage = false
            inWorkspacePackage && found == null && line.startsWith("version") ->
                Regex("\"([0-9]+\\.[0-9]+\\.[0-9]+)\"").find(line)?.let { found = it.groupValues[1] }
        }
    }
    found ?: throw GradleException("no version = \"x.y.z\" under [workspace.package] in $manifest")
}
val cargoVersionParts = cargoVersion.split(".").map { it.toInt() }
require(cargoVersionParts.size == 3 && cargoVersionParts.all { it in 0..99 }) {
    "unexpected version in Cargo.toml: $cargoVersion (want three components, each 0..99)"
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
        versionName = cargoVersion
        // Android wants one monotonically increasing integer, and the Rust
        // version only ever grows in its last component, so packing the three
        // into one keeps that order: 1.1.8 -> 10108. This is what an install
        // over an older one is compared with, so it has to keep growing.
        versionCode = cargoVersionParts[0] * 10_000 + cargoVersionParts[1] * 100 + cargoVersionParts[2]
    }

    sourceSets {
        getByName("main") {
            manifest.srcFile("../../crates/monitor_android/android/AndroidManifest.xml")
            java.srcDirs("../../crates/monitor_android/android/java")
            res.srcDirs("../../crates/monitor_android/android/res")
            // Populated by scripts/build-android.sh (cargo-ndk).
            jniLibs.srcDirs("../../android-build/jniLibs")
            // Language packs (.ftl) shipped as APK assets. The native library
            // extracts them to the configuration directory on launch; see
            // `extract_lang_assets` in monitor_gui/src/lib.rs. English is
            // embedded in the binary, so only the extra packs live here.
            assets.srcDirs("../../langs")
        }
    }

    signingConfigs {
        if (keystorePropertiesFile.exists()) {
            create("release") {
                // `storeFile` is resolved against the Gradle project root, so
                // the properties file can carry the plain file name and the
                // checkout stays movable.
                storeFile = rootProject.file(keystoreProperties.getProperty("storeFile"))
                storePassword = keystoreProperties.getProperty("storePassword")
                keyAlias = keystoreProperties.getProperty("keyAlias")
                keyPassword = keystoreProperties.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        getByName("debug") {
            isMinifyEnabled = false
        }
        getByName("release") {
            // There is nothing here for R8 to shrink - the code is the activity
            // and the receiver, the work is all in the native library - and
            // leaving it off keeps the mapping file out of the way.
            isMinifyEnabled = false
            signingConfig = signingConfigs.findByName("release")
                ?: signingConfigs.getByName("debug").also {
                    logger.warn(
                        "XGView: android/keystore.properties is missing, " +
                            "so the release APK is signed with the debug key. " +
                            "Build through scripts/build-android.ps1 to sign it properly."
                    )
                }
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

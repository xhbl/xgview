#requires -Version 5.1
<#
.SYNOPSIS
    Builds the Android APK for XGView on Windows.

.DESCRIPTION
    The Windows counterpart of build-android.sh when the toolchain lives on
    this machine rather than in WSL: cross compiles the native library with
    cargo-ndk, assembles the APK with Gradle, and leaves it in `target` named
    after the version and the ABI(s) it carries:

        target\xgview-<version>-<abi>.apk          release (the default)
        target\xgview-<version>-<abi>-debug.apk    with -DebugBuild

    The release APK is signed with the project's own keystore, kept under
    `android\` and created the first time a release build runs; see the signing
    section below. `-DebugBuild` builds the debug variant, which is signed with
    the local debug key and is for development only.

    The toolchain - NDK, JDK, Gradle, SDK - is looked up under
    XGVIEW_ANDROID_ROOT, or C:\Android by default. `adb` is only needed to
    deploy (see deploy-android.ps1), not to build.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\build-android.ps1

.EXAMPLE
    # The debug variant, another ABI and API level (cargo-ndk flags).
    powershell -ExecutionPolicy Bypass -File scripts\build-android.ps1 -DebugBuild -Abis armeabi-v7a -Api 30
#>
[CmdletBinding()]
param(
    # Android ABI(s) to cross compile for, exactly as cargo-ndk -t takes them
    # (space separated for several).
    [string]$Abis = 'arm64-v8a',

    # Android API level (cargo-ndk -P). Kept equal to `minSdk` in
    # android\app\build.gradle.kts, so the library is never compiled against a
    # platform the manifest refuses to install on.
    [int]$Api = 27,

    # Build the debug APK instead of the release one. It is signed with this
    # machine's debug key and carries android:debuggable, so it is for
    # development; the release APK is the one to deploy. Not named `-Debug`,
    # which `[CmdletBinding()]` already defines as the common parameter that
    # controls debug output.
    [switch]$DebugBuild
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'common.ps1')

$repoRoot = Split-Path -Parent $PSScriptRoot
$androidRoot = if ($env:XGVIEW_ANDROID_ROOT) { $env:XGVIEW_ANDROID_ROOT } else { 'C:\Android' }

# One directory per folder, so a version bump needs no edit here.
$ndk = (Get-ChildItem (Join-Path $androidRoot 'ndk') -Directory -ErrorAction SilentlyContinue | Select-Object -First 1).FullName
$jdk = (Get-ChildItem (Join-Path $androidRoot 'jdk') -Directory -ErrorAction SilentlyContinue | Select-Object -First 1).FullName
$sdk = Join-Path $androidRoot 'sdk'
$gradleDir = Get-ChildItem (Join-Path $androidRoot 'gradle\gradle-*') -Directory -ErrorAction SilentlyContinue | Select-Object -First 1
$gradle = if ($gradleDir) { Join-Path $gradleDir.FullName 'bin\gradle.bat' }

foreach ($tool in @($ndk, $jdk, $sdk, $gradle)) {
    if (-not $tool -or -not (Test-Path $tool)) {
        throw "toolchain not found: $tool (set XGVIEW_ANDROID_ROOT, or ANDROID_NDK_HOME / JAVA_HOME / ANDROID_HOME)"
    }
}

# cargo-ndk reads the NDK from here; Gradle reads the SDK and the JDK.
$env:ANDROID_NDK_HOME = $ndk
$env:ANDROID_HOME = $sdk
$env:ANDROID_SDK_ROOT = $sdk
$env:JAVA_HOME = $jdk

$variant = if ($DebugBuild) { 'debug' } else { 'release' }
$task = if ($DebugBuild) { 'assembleDebug' } else { 'assembleRelease' }

# Release signing.
#
# Gradle reads where the keystore is, and how to unlock it, from
# `android\keystore.properties`. Both it and the keystore are kept with the
# repository, and are created here, once, the first time a release build runs,
# with a generated password so there is nothing to remember. They are what
# keeps a release APK upgradeable in place: one signed with a key that is gone
# can only be uninstalled and installed anew, losing the configuration with it.
if (-not $DebugBuild) {
    $keystore = Join-Path $repoRoot 'android\xgview-release.jks'
    $keystoreProperties = Join-Path $repoRoot 'android\keystore.properties'
    $hasKeystore = Test-Path $keystore
    $hasProperties = Test-Path $keystoreProperties
    if ($hasKeystore -ne $hasProperties) {
        throw "release signing is half set up: keep both, or neither, of $keystore and $keystoreProperties"
    }
    if (-not $hasKeystore) {
        Write-Host '==> creating the release keystore (android\xgview-release.jks)' -ForegroundColor Cyan
        $alphabet = 'abcdefghijklmnopqrstuvwxyz0123456789'
        $password = -join (1..32 | ForEach-Object { $alphabet[(Get-Random -Maximum $alphabet.Length)] })
        $keytool = Join-Path $jdk 'bin\keytool.exe'
        & $keytool -genkeypair -v -keystore $keystore -storetype PKCS12 -alias xgview `
            -keyalg RSA -keysize 2048 -validity 10000 `
            -storepass $password -keypass $password `
            -dname 'CN=XGView, OU=XGView, O=XGView, C=CN'
        if ($LASTEXITCODE -ne 0) { throw "keytool failed (exit $LASTEXITCODE)" }
        # `storeFile` is taken relative to the Gradle project root (android\).
        @(
            'storeFile=xgview-release.jks'
            "storePassword=$password"
            'keyAlias=xgview'
            "keyPassword=$password"
        ) | Set-Content -Path $keystoreProperties -Encoding ASCII
    }
}

Push-Location $repoRoot
try {
    Write-Host "==> native library ($Abis, API $Api, release)" -ForegroundColor Cyan
    & cargo ndk -t $Abis -P $Api -o android-build/jniLibs build --release -p monitor_android
    if ($LASTEXITCODE -ne 0) { throw "cargo ndk failed (exit $LASTEXITCODE)" }

    Write-Host "==> APK (gradle $task)" -ForegroundColor Cyan
    & $gradle -p android $task --console=plain
    if ($LASTEXITCODE -ne 0) { throw "gradle failed (exit $LASTEXITCODE)" }

    $built = Join-Path $repoRoot "android\app\build\outputs\apk\$variant\app-$variant.apk"
    if (-not (Test-Path $built)) { throw "APK not found: $built" }

    $version = Get-AppVersion -Manifest (Join-Path $repoRoot 'Cargo.toml')
    # The platform the APK is named for is the ABI it carries, so a build for
    # another target is a differently named file rather than an overwrite. The
    # debug variant carries a suffix of its own, so building it cannot replace
    # the release APK a previous run left under the plain name.
    $platform = ($Abis -split '\s+') -join '-'
    $suffix = if ($DebugBuild) { '-debug' } else { '' }
    $apk = Join-Path $repoRoot "target\xgview-$version-$platform$suffix.apk"
    Copy-Item $built $apk -Force

    Write-Host ''
    Write-Host "==> $apk" -ForegroundColor Cyan
}
finally {
    Pop-Location
}

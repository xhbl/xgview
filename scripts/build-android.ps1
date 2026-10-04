#requires -Version 5.1
<#
.SYNOPSIS
    Builds the Android APK for XGView on Windows.

.DESCRIPTION
    The Windows counterpart of build-android.sh when the toolchain lives on
    this machine rather than in WSL: cross compiles the native library with
    cargo-ndk, assembles the APK with Gradle, and leaves it in `target` named
    after the version and the ABI(s) it carries:

        target\xgview-<version>-<abi>.apk

    The toolchain - NDK, JDK, Gradle, SDK - is looked up under
    XGVIEW_ANDROID_ROOT, or C:\Android by default. `adb` is only needed to
    deploy (see deploy-android.ps1), not to build.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\build-android.ps1

.EXAMPLE
    # Another ABI and API level (cargo-ndk flags).
    powershell -ExecutionPolicy Bypass -File scripts\build-android.ps1 -Abis armeabi-v7a -Api 30
#>
[CmdletBinding()]
param(
    # Android ABI(s) to cross compile for, exactly as cargo-ndk -t takes them
    # (space separated for several).
    [string]$Abis = 'arm64-v8a',

    # Android API level (cargo-ndk -P).
    [int]$Api = 28
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

Push-Location $repoRoot
try {
    Write-Host "==> native library ($Abis, API $Api, release)" -ForegroundColor Cyan
    & cargo ndk -t $Abis -P $Api -o android-build/jniLibs build --release -p monitor_android
    if ($LASTEXITCODE -ne 0) { throw "cargo ndk failed (exit $LASTEXITCODE)" }

    Write-Host '==> APK (gradle assembleDebug)' -ForegroundColor Cyan
    & $gradle -p android assembleDebug --console=plain
    if ($LASTEXITCODE -ne 0) { throw "gradle failed (exit $LASTEXITCODE)" }

    $built = Join-Path $repoRoot 'android\app\build\outputs\apk\debug\app-debug.apk'
    if (-not (Test-Path $built)) { throw "APK not found: $built" }

    $version = Get-AppVersion -Manifest (Join-Path $repoRoot 'Cargo.toml')
    # The platform the APK is named for is the ABI it carries, so a build for
    # another target is a differently named file rather than an overwrite.
    $platform = ($Abis -split '\s+') -join '-'
    $apk = Join-Path $repoRoot "target\xgview-$version-$platform.apk"
    Copy-Item $built $apk -Force

    Write-Host ''
    Write-Host "==> $apk" -ForegroundColor Cyan
}
finally {
    Pop-Location
}

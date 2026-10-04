#requires -Version 5.1
<#
.SYNOPSIS
    Installs a built XGView APK on an Android device over adb.

.DESCRIPTION
    The Windows counterpart of deploy-android.sh. It installs the APK, grants
    the SYSTEM_ALERT_WINDOW special permission the boot receiver needs - `-g`
    does not grant a special permission, and without it Android 10+ aborts the
    start the boot receiver asks for (see the README, "Start on boot") - and
    launches the app.

    It builds nothing: run build-android.ps1 first.

.EXAMPLE
    # The device already connected over USB, or the only one adb sees.
    powershell -ExecutionPolicy Bypass -File scripts\deploy-android.ps1

.EXAMPLE
    # Wireless debugging, and follow the app's log.
    powershell -ExecutionPolicy Bypass -File scripts\deploy-android.ps1 192.168.10.42 -Logcat
#>
[CmdletBinding()]
param(
    # Wireless adb target, "192.168.10.42" or "192.168.10.42:5555". Omit to use
    # the device that is already connected.
    [string]$Device,

    # Port used when -Device is given without one.
    [int]$AdbPort = 5555,

    # APK to install. Defaults to the single target\xgview-<version>-<abi>.apk
    # build-android.ps1 left there.
    [string]$Apk,

    # Install but do not start the app.
    [switch]$NoLaunch,

    # Follow the app's logcat after launching (Ctrl+C to stop).
    [switch]$Logcat
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'common.ps1')

$repoRoot = Split-Path -Parent $PSScriptRoot
$appId = 'com.xhbl.xgview'

$adb = 'C:\Apps\ADB\adb.exe'
$onPath = Get-Command adb -ErrorAction SilentlyContinue
if ($onPath) { $adb = $onPath.Source }
if (-not (Test-Path $adb)) { throw "adb not found: $adb (put adb on PATH)" }

if (-not $Apk) {
    # The newest APK of this version, whatever ABI it was built for and
    # whichever variant it is - the name carries both, and the caller should
    # not have to repeat them. Release and debug builds can sit side by side,
    # and the one just built is the one meant.
    $version = Get-AppVersion -Manifest (Join-Path $repoRoot 'Cargo.toml')
    $candidates = @(Get-ChildItem (Join-Path $repoRoot "target\xgview-$version-*.apk") -ErrorAction SilentlyContinue)
    if ($candidates.Count -eq 0) {
        throw "no APK for version $version in target. Run scripts\build-android.ps1 first, or pass -Apk <path>."
    }
    $Apk = ($candidates | Sort-Object LastWriteTime -Descending | Select-Object -First 1).FullName
}
if (-not (Test-Path $Apk)) {
    throw "APK not found: $Apk."
}

if ($Device) {
    $serial = if ($Device -match ':') { $Device } else { "${Device}:${AdbPort}" }
    Write-Host "==> adb connect $serial" -ForegroundColor Cyan
    & $adb connect $serial | Out-Null
    & $adb -s $serial wait-for-device
}
else {
    $serial = (& $adb get-serialno).Trim()
    if (-not $serial -or $serial -eq 'unknown') {
        throw 'no device connected: plug in the device, or pass -Device <ip>'
    }
}
Write-Host "    device $serial" -ForegroundColor DarkGray

Write-Host '==> installing' -ForegroundColor Cyan
& $adb -s $serial install -r -g $Apk
if ($LASTEXITCODE -ne 0) { throw "adb install failed (exit $LASTEXITCODE)" }

# SYSTEM_ALERT_WINDOW is a special permission, so -g does not grant it, and
# without it Android 10+ aborts the start the boot receiver asks for - leaving
# the box dark after a power cut. An APK installed by hand asks the viewer to do
# it once from the app's System tab instead.
Write-Host '==> allowing the background start at boot (SYSTEM_ALERT_WINDOW)' -ForegroundColor Cyan
& $adb -s $serial shell appops set $appId SYSTEM_ALERT_WINDOW allow | Out-Null

if (-not $NoLaunch) {
    Write-Host "==> launching $appId" -ForegroundColor Cyan
    & $adb -s $serial shell monkey -p $appId -c android.intent.category.LAUNCHER 1 | Out-Null
}

if ($Logcat) {
    Write-Host '==> logcat (Ctrl+C to stop)' -ForegroundColor Cyan
    & $adb -s $serial logcat -s xgview XGView.BootReceiver
}

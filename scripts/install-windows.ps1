#requires -Version 5.1
<#
.SYNOPSIS
    Installs XGView on a Windows machine / Mini PC and (optionally) registers it
    for start-on-boot.

.DESCRIPTION
    Builds the release binary, copies it to a per-user install directory and
    registers the start-on-boot entry.

    Two registration strategies are supported:

      * Registry (default) - calls `xgview.exe --install-autostart`, which writes
        `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`. The app starts when
        the *user* logs in, which is the right behaviour when the Mini PC is
        configured with automatic logon.

      * Scheduled task (-TaskScheduler) - registers a task with an "At log on"
        trigger and a restart-on-failure policy, so a crashed viewer comes back
        without a reboot. Note that Windows still requires an interactive logon
        unless the machine is set up for auto-logon; a dedicated service would be
        needed to start truly headless.

.EXAMPLE
    # Build + install for the current user and enable start-on-boot (registry).
    powershell -ExecutionPolicy Bypass -File scripts\install-windows.ps1

.EXAMPLE
    # Use a scheduled task with automatic restart instead of the Run key.
    powershell -ExecutionPolicy Bypass -File scripts\install-windows.ps1 -TaskScheduler

.EXAMPLE
    # Install an existing build without recompiling.
    powershell -ExecutionPolicy Bypass -File scripts\install-windows.ps1 -NoBuild
#>
[CmdletBinding()]
param(
    # Skip `cargo build --release` and install the existing target\release binary.
    [switch]$NoBuild,

    # Register a scheduled task (with restart on failure) instead of the HKCU Run key.
    [switch]$TaskScheduler,

    # Do not register any start-on-boot entry.
    [switch]$NoAutostart,

    # Install directory. Defaults to %LOCALAPPDATA%\XGView.
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'XGView'),

    # Extra arguments forwarded to xgview.exe on every launch (e.g. --fullscreen).
    [string]$Arguments = '',

    # Name of the scheduled task when -TaskScheduler is used.
    [string]$TaskName = 'XGView'
)

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$exeName = 'xgview.exe'
$sourceExe = Join-Path $repoRoot "target\release\$exeName"
$targetExe = Join-Path $InstallDir $exeName

function Write-Step([string]$Message) {
    Write-Host "==> $Message" -ForegroundColor Cyan
}

# The desktop decoder links FFmpeg dynamically, so the executable imports
# `avcodec-*.dll` and its neighbours; Windows resolves those from the program's
# own directory or from `PATH`. On the build machine vcpkg puts them on `PATH`,
# an installed copy has neither - hence the files are copied next to the binary.
function Get-FfmpegRuntimeDlls {
    param([string]$Exe)

    # What the executable actually imports, read out of its own bytes: a build
    # without the `ffmpeg` feature imports none of these and copies nothing.
    # The names carry the version (`avcodec-63`), which is the linker's answer
    # and not something to hard-code here - the wrong version is a missing dll
    # at launch.
    $bytes = [IO.File]::ReadAllBytes($Exe)
    $imports = [Text.Encoding]::ASCII.GetString($bytes) -split '[^\x20-\x7E]' |
        Where-Object { $_ -match '^(av|sw)[a-z]+-\d+\.dll$' } |
        Sort-Object -Unique
    if (-not $imports) { return @() }

    $candidates = @()
    if ($env:VCPKG_ROOT) { $candidates += (Join-Path $env:VCPKG_ROOT 'installed\x64-windows\bin') }
    if ($env:FFMPEG_DIR) { $candidates += (Join-Path $env:FFMPEG_DIR 'bin') }

    $available = @{}
    foreach ($dir in $candidates) {
        if (-not (Test-Path $dir)) { continue }
        Get-ChildItem -Path $dir -Filter *.dll |
            ForEach-Object { $available[$_.Name] = $_.FullName }
    }

    foreach ($name in $imports) {
        if (-not $available.ContainsKey($name)) {
            throw "FFmpeg runtime '$name' is imported by the binary but was not found. Point VCPKG_ROOT or FFMPEG_DIR at the tree it was built against."
        }
    }

    # Every av*/sw* dll of that tree, not only the ones imported directly: the
    # imported ones pull dependencies of their own, which Windows resolves from
    # the same directory as the executable.
    return $available.Values | Where-Object { (Split-Path $_ -Leaf) -match '^(av|sw)[a-z]+-\d+\.dll$' }
}

# ---------------------------------------------------------------- build
if (-not $NoBuild) {
    Write-Step 'Building the release binary (cargo build --release)'
    Push-Location $repoRoot
    try {
        cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
    }
    finally {
        Pop-Location
    }
}

if (-not (Test-Path $sourceExe)) {
    throw "Binary not found: $sourceExe. Run without -NoBuild first."
}

# ---------------------------------------------------------------- install
Write-Step "Installing into $InstallDir"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Copy-Item -Path $sourceExe -Destination $targetExe -Force
Write-Host "    $targetExe"

# The FFmpeg runtime the binary imports, beside it: without these the installed
# viewer exits at launch with "dll not found", which is not a state the machine
# should be left in after a successful-looking install.
$runtimeDlls = Get-FfmpegRuntimeDlls -Exe $sourceExe
foreach ($dll in $runtimeDlls) {
    Copy-Item -Path $dll -Destination $InstallDir -Force
    Write-Host "    $(Split-Path $dll -Leaf)"
}
if (-not $runtimeDlls) {
    Write-Host '    no ffmpeg runtime dlls (built without the ffmpeg feature)'
}

# The configuration lives next to the user profile; make sure it exists so the
# first launch finds a well known location.
$configRoot = Join-Path $env:APPDATA 'xgview'
New-Item -ItemType Directory -Force -Path $configRoot | Out-Null
Write-Host "    config directory: $configRoot"

# ---------------------------------------------------------------- autostart
if ($NoAutostart) {
    Write-Step 'Skipping the start-on-boot registration (-NoAutostart)'
}
elseif ($TaskScheduler) {
    Write-Step "Registering the scheduled task '$TaskName'"

    $action = New-ScheduledTaskAction -Execute $targetExe -Argument $Arguments
    $trigger = New-ScheduledTaskTrigger -AtLogOn
    # Restart up to three times, one minute apart: a viewer that lost its network
    # at boot still comes back on its own.
    $settings = New-ScheduledTaskSettingsSet `
        -AllowStartIfOnBatteries `
        -DontStopIfGoingOnBatteries `
        -ExecutionTimeLimit ([TimeSpan]::Zero) `
        -RestartCount 3 `
        -RestartInterval (New-TimeSpan -Minutes 1) `
        -MultipleInstances IgnoreNew
    $principal = New-ScheduledTaskPrincipal `
        -UserId "$env:USERDOMAIN\$env:USERNAME" `
        -LogonType Interactive `
        -RunLevel Limited

    Register-ScheduledTask `
        -TaskName $TaskName `
        -Action $action `
        -Trigger $trigger `
        -Settings $settings `
        -Principal $principal `
        -Force | Out-Null

    Write-Host '    A scheduled task only runs after a user logs in.'
    Write-Host '    Enable automatic logon (netplwiz / Sysinternals Autologon) for a truly unattended Mini PC.'
}
else {
    Write-Step 'Registering the HKCU Run entry (xgview --install-autostart)'
    & $targetExe --install-autostart
    if ($LASTEXITCODE -ne 0) { throw "registering start-on-boot failed with exit code $LASTEXITCODE" }
}

# ---------------------------------------------------------------- summary
Write-Host ''
Write-Step 'Done'
Write-Host "    binary : $targetExe"
Write-Host "    config : $configRoot\config.json"
if (-not $NoAutostart) {
    if ($TaskScheduler) {
        Write-Host "    boot   : scheduled task '$TaskName' (remove with: Unregister-ScheduledTask -TaskName $TaskName)"
    }
    else {
        Write-Host "    boot   : HKCU\Software\Microsoft\Windows\CurrentVersion\Run (remove with: $exeName --remove-autostart)"
    }
}
Write-Host ''
Write-Host "Launch it now with:  & '$targetExe' --fullscreen"

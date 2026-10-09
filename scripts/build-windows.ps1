#requires -Version 5.1
<#
.SYNOPSIS
    Builds a release package of XGView for Windows.

.DESCRIPTION
    Builds the release binary and packs everything it needs to run - the
    executable and the FFmpeg runtime it links against - into a single zip under
    `target`, named after the version, the platform and the architecture it was
    built for:

        target\xgview-<version>-windows-<arch>.zip

    The zip holds the files at its root, with no folder around them, so it can
    be extracted anywhere and `xgview.exe` run from there. The same set is left
    in `target\package` for inspection, or to run without unzipping.

    Nothing is installed and no start-on-boot entry is registered; this script
    only produces the package. To install the build on this machine instead,
    run `install-windows.ps1`.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1

.EXAMPLE
    # Package an existing build without recompiling.
    powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -NoBuild

.EXAMPLE
    # Install the build on this machine, then package it for other machines.
    powershell -ExecutionPolicy Bypass -File scripts\install-windows.ps1
    powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -NoBuild
#>
[CmdletBinding()]
param(
    # Skip `cargo build --release` and package the existing target\release binary.
    [switch]$NoBuild
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'common.ps1')

$repoRoot = Split-Path -Parent $PSScriptRoot
$exeName = 'xgview.exe'
$sourceExe = Join-Path $repoRoot "target\release\$exeName"
# The command line entry point, built as the cargo target `xgview-cli` and
# shipped as `xgview.com`: a shell finds that before `xgview.exe` - `.COM` comes
# first in PATHEXT - and waits for it, being a console application. It looks for
# the viewer above beside itself, so the two travel together.
$cliName = 'xgview.com'
$sourceCli = Join-Path $repoRoot 'target\release\xgview-cli.exe'

function Write-Step([string]$Message) {
    Write-Host "==> $Message" -ForegroundColor Cyan
}

# The desktop decoder links FFmpeg dynamically, so the executable imports
# `avcodec-*.dll` and its neighbours; Windows resolves those from the program's
# own directory or from `PATH`. On the build machine vcpkg puts them on `PATH`,
# on the machine that unpacks this zip there is nothing - hence they travel with
# the executable.
function Get-FfmpegRuntimeDlls {
    param([string]$Exe)

    # What the executable actually imports, read out of its own bytes: a build
    # without the `ffmpeg` feature imports none of these and packs nothing. The
    # names carry the version (`avcodec-63`), which is the linker's answer and
    # not something to hard-code here - the wrong version is a missing dll at
    # launch.
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

# ---------------------------------------------------------------- stage
$version = Get-AppVersion -Manifest (Join-Path $repoRoot 'Cargo.toml')
$stage = Join-Path $repoRoot 'target\package'

# The architecture the package is named for. The build is for the host - there
# is no `--target` here, and the binary lands in target\release - so the host
# triple `rustc` reports is the one to name.
$hostLine = (& rustc -vV) | Where-Object { $_ -like 'host:*' } | Select-Object -First 1
if (-not $hostLine -or $hostLine -notmatch 'host:\s*(\S+)') {
    throw 'cannot read the host target from `rustc -vV`'
}
$arch = switch -Wildcard ($matches[1]) {
    'x86_64*' { 'x86_64' }
    'i686*' { 'i686' }
    'aarch64*' { 'arm64' }
    default { throw "unsupported host architecture: $($matches[1])" }
}
$zip = Join-Path $repoRoot "target\xgview-$version-windows-$arch.zip"

Write-Step "Collecting the runtime into $stage"
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force -Path $stage | Out-Null

Copy-Item -Path $sourceExe -Destination (Join-Path $stage $exeName) -Force
Write-Host "    $exeName"
Copy-Item -Path $sourceCli -Destination (Join-Path $stage $cliName) -Force
Write-Host "    $cliName"
# Language packs: every .ftl in the repo's langs/ directory travels beside the
# executable so a viewer can switch language without rebuilding.
$langsSource = Join-Path $repoRoot 'langs'
if (Test-Path $langsSource) {
    $langsDest = Join-Path $stage 'langs'
    New-Item -ItemType Directory -Force -Path $langsDest | Out-Null
    Copy-Item -Path (Join-Path $langsSource '*.ftl') -Destination $langsDest -Force
    Write-Host "    langs/"
}
foreach ($dll in Get-FfmpegRuntimeDlls -Exe $sourceExe) {
    Copy-Item -Path $dll -Destination $stage -Force
    Write-Host "    $(Split-Path $dll -Leaf)"
}

# ---------------------------------------------------------------- zip
Write-Step "Packing $zip"
# The children of the staging directory, so the zip has the files at its root
# rather than inside a folder named after the version: it is unpacked to run the
# executable, not to browse a tree.
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip -Force

$size = [Math]::Round((Get-Item $zip).Length / 1MB, 1)
Write-Host ''
Write-Step 'Done'
Write-Host "    package : $zip ($size MB)"
Write-Host "    staged  : $stage"

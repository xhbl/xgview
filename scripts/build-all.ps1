#requires -Version 5.1
<#
.SYNOPSIS
    Bumps the build number, then builds the Windows and Android packages.

.DESCRIPTION
    Raises the last component of the version declared in `[workspace.package]`
    (1.1.6 -> 1.1.7), then runs `build-windows.ps1` and `build-android.ps1` in
    turn, so both packages come out under the same, freshly bumped version.

    The bump lives here on purpose: the individual build scripts only read the
    version, they never bump it.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\build-all.ps1
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'common.ps1')

$repoRoot = Split-Path -Parent $PSScriptRoot
$cargoToml = Join-Path $repoRoot 'Cargo.toml'

function Write-Step([string]$Message) {
    Write-Host "==> $Message" -ForegroundColor Cyan
}

# Bump the patch component of `version` inside `[workspace.package]`. The file
# starts with a UTF-8 BOM and uses LF endings, so it is read and written back
# with the same encoding and the same endings: `WriteAllLines` would write CRLF
# on Windows and rewrite every line of the manifest, burying the one line this
# script actually changed.
$lines = [System.IO.File]::ReadAllLines($cargoToml)
$inSection = $false
$bumped = $false
for ($i = 0; $i -lt $lines.Length; $i++) {
    $line = $lines[$i].Trim()
    if ($line -eq '[workspace.package]') { $inSection = $true; continue }
    if ($inSection -and $line.StartsWith('[')) { break }
    if ($inSection -and $line -match '^version\s*=\s*"(\d+)\.(\d+)\.(\d+)"') {
        $newVersion = "$($Matches[1]).$($Matches[2]).$([int]$Matches[3] + 1)"
        $lines[$i] = $lines[$i] -replace '"\d+\.\d+\.\d+"', "`"$newVersion`""
        $bumped = $true
        break
    }
}
if (-not $bumped) {
    throw "no version = `"x.y.z`" under [workspace.package] in $cargoToml"
}

[System.IO.File]::WriteAllText($cargoToml, ($lines -join "`n") + "`n", (New-Object System.Text.UTF8Encoding($true)))

Write-Step "Version bumped to $newVersion"

Write-Step 'Building the Windows package'
& (Join-Path $PSScriptRoot 'build-windows.ps1')

Write-Step 'Building the Android package'
& (Join-Path $PSScriptRoot 'build-android.ps1')

Write-Step "Done - target\xgview-$newVersion-*.zip and target\xgview-$newVersion-*.apk"

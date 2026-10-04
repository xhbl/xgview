# Shared helpers for the Windows build / packaging / deploy scripts.
#
# Dot-source it near the top:
#
#     . (Join-Path $PSScriptRoot 'common.ps1')

<#
.SYNOPSIS
    The application version, read from a manifest.

.DESCRIPTION
    The version is declared once in `[workspace.package]` and inherited by
    every crate, so that is the section that carries the literal; a `[package]`
    that states its own version is honoured too. Read rather than repeated: a
    name that has to be kept in step by hand is a name that eventually
    disagrees with the binary inside it.
#>
function Get-AppVersion {
    param([Parameter(Mandatory)][string]$Manifest)

    $section = ''
    $packageVersion = $null
    foreach ($line in Get-Content -Path $Manifest) {
        if ($line -match '^\s*\[(.+?)\]\s*$') {
            $section = $matches[1]
            continue
        }
        if ($line -match '^\s*version\s*=\s*"([^"]+)"') {
            if ($section -eq 'workspace.package') { return $matches[1] }
            if ($section -eq 'package' -and -not $packageVersion) { $packageVersion = $matches[1] }
        }
    }
    if ($packageVersion) { return $packageVersion }
    throw "no version found in $Manifest ([workspace.package] or [package])"
}

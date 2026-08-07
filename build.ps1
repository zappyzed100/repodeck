#Requires -Version 7
<#
.SYNOPSIS
    Canonical RepoDeck development build entry point.

.DESCRIPTION
    Always invoke this file from the repository root. It delegates to the
    implementation in scripts\build.ps1, which builds into target\release and
    synchronizes the Start Menu shortcut with that exact executable.

    Do not run cargo build directly for the local app: that bypasses the
    shortcut synchronization and makes it easy to run a stale executable.
#>
[CmdletBinding()]
param(
    [switch]$Run,
    [switch]$SkipShortcut
)

$ErrorActionPreference = 'Stop'
$implementation = Join-Path $PSScriptRoot 'scripts\build.ps1'
$arguments = @()
if ($Run) { $arguments += '-Run' }
if ($SkipShortcut) { $arguments += '-SkipShortcut' }

& $implementation @arguments
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

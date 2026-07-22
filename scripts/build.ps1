#Requires -Version 7
<#
.SYNOPSIS
    Dev build for RepoDeck. There is ONE canonical build/run location:

        target\release\repodeck.exe   (repodeck-hook.exe sits beside it)

    Always run RepoDeck from there. `dist\` is only for producing the release
    .zip via scripts\package.ps1 — it is not a place to run the app from.

.PARAMETER Run
    After a successful build, launch target\release\repodeck.exe. The app
    self-elevates, so accept the UAC prompt. If RepoDeck is already running,
    quit it first (tray → 終了) — a running exe can't be overwritten by the build.
#>
param([switch]$Run)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $repoRoot
try {
    if (Get-Process repodeck -ErrorAction SilentlyContinue) {
        throw "RepoDeck is running and locks target\release\repodeck.exe. Quit it (tray → 終了) and re-run."
    }

    Write-Host "Building release (target\release)..."
    cargo build --release
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build --release failed with exit code $LASTEXITCODE"
    }

    $exe = Join-Path $repoRoot "target\release\repodeck.exe"
    Write-Host ""
    Write-Host "Build OK. Canonical exe:"
    Write-Host "  $exe"

    if ($Run) {
        Write-Host "Launching (accept the UAC prompt)..."
        Start-Process -FilePath $exe
    }
}
finally {
    Pop-Location
}

#Requires -Version 7
<#
.SYNOPSIS
    Dev build for RepoDeck. There is ONE canonical build/run location:

        target\release\repodeck.exe   (repodeck-hook.exe sits beside it)

    Always run RepoDeck from there. `dist\` is only for producing the release
    .zip via scripts\package.ps1 — it is not a place to run the app from.

    Every build re-points the Start Menu shortcut at that canonical exe, so
    "RepoDeck" in the Start Menu always launches the build you just made. The
    shortcut is created if it doesn't exist yet.

.PARAMETER Run
    After a successful build, launch target\release\repodeck.exe. The app
    self-elevates, so accept the UAC prompt.

.PARAMETER SkipShortcut
    Build only; leave the Start Menu shortcut alone.
#>
param(
    [switch]$Run,
    [switch]$SkipShortcut
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot

# A running RepoDeck locks its own exe against the linker's write. Testing the
# lock directly (rather than just "is a repodeck.exe running?") keeps the build
# unblocked when the running instance is a *different* copy — a debug build, or
# a packaged one out of dist\.
function Test-FileLocked([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return $false }
    try {
        $stream = [IO.File]::Open($Path, 'Open', 'ReadWrite', 'None')
        $stream.Close()
        return $false
    }
    catch {
        return $true
    }
}

# Points the Start Menu entry at $Exe, creating it when missing. Kept idempotent
# so a build that changes nothing rewrites nothing.
function Sync-StartMenuShortcut([string]$Exe) {
    $startMenu = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs"
    $link = Join-Path $startMenu "RepoDeck.lnk"
    $workingDir = Split-Path -Parent $Exe

    if (-not (Test-Path -LiteralPath $startMenu)) {
        New-Item -ItemType Directory -Path $startMenu -Force | Out-Null
    }

    $shell = New-Object -ComObject WScript.Shell
    try {
        $shortcut = $shell.CreateShortcut($link)
        $isCurrent = (Test-Path -LiteralPath $link) -and
                     $shortcut.TargetPath -eq $Exe -and
                     $shortcut.WorkingDirectory -eq $workingDir
        if ($isCurrent) {
            Write-Host "Start Menu shortcut already points here: $link"
            return
        }

        $shortcut.TargetPath = $Exe
        $shortcut.WorkingDirectory = $workingDir
        $shortcut.IconLocation = "$Exe,0"
        $shortcut.Description = "RepoDeck"
        $shortcut.Save()
        Write-Host "Start Menu shortcut updated: $link"
    }
    finally {
        [Runtime.InteropServices.Marshal]::ReleaseComObject($shell) | Out-Null
    }
}

Push-Location $repoRoot
try {
    $exe = Join-Path $repoRoot "target\release\repodeck.exe"

    if (Test-FileLocked $exe) {
        throw "RepoDeck is running and locks $exe. Quit it (tray -> 終了) and re-run."
    }

    Write-Host "Building release (target\release)..."
    cargo build --release
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build --release failed with exit code $LASTEXITCODE"
    }

    Write-Host ""
    Write-Host "Build OK. Canonical exe:"
    Write-Host "  $exe"

    if (-not $SkipShortcut) {
        Sync-StartMenuShortcut $exe
    }

    if ($Run) {
        Write-Host "Launching (accept the UAC prompt)..."
        Start-Process -FilePath $exe
    }
}
finally {
    Pop-Location
}

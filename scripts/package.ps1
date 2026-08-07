#Requires -Version 7
<#
.SYNOPSIS
    Builds RepoDeck in release mode and assembles the MVP distribution zip
    (development-plan.md §17): repodeck.exe, repodeck-hook.exe, README.txt,
    THIRD_PARTY_NOTICES.md, LICENSE, named RepoDeck-v<version>-windows-x64.zip,
    plus a .sha256 checksum file alongside it.
#>

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $repoRoot
try {
    Write-Host "Building release binaries..."
    & (Join-Path $repoRoot "scripts\build.ps1") -SkipShortcut
    if ($LASTEXITCODE -ne 0) {
        throw "scripts\build.ps1 failed with exit code $LASTEXITCODE"
    }

    $cargoToml = Get-Content (Join-Path $repoRoot "Cargo.toml") -Raw
    if ($cargoToml -notmatch 'version\s*=\s*"([^"]+)"') {
        throw "could not find a version in Cargo.toml"
    }
    $version = $Matches[1]

    $distName = "RepoDeck-v$version-windows-x64"
    $distDir = Join-Path $repoRoot "dist"
    $stagingDir = Join-Path $distDir $distName
    if (Test-Path $stagingDir) {
        Remove-Item $stagingDir -Recurse -Force
    }
    New-Item -ItemType Directory -Force -Path $stagingDir | Out-Null

    $releaseDir = Join-Path $repoRoot "target\release"
    Copy-Item (Join-Path $releaseDir "repodeck.exe") $stagingDir
    Copy-Item (Join-Path $releaseDir "repodeck-hook.exe") $stagingDir
    Copy-Item (Join-Path $repoRoot "THIRD_PARTY_NOTICES.md") $stagingDir
    Copy-Item (Join-Path $repoRoot "LICENSE") $stagingDir

    @"
RepoDeck v$version
====================

RepoDeck is a Windows desktop app for managing multi-repository "worksets"
of application windows, with global-hotkey switching and optional Codex
agent-status integration.

Getting started, full usage instructions, and Codex integration setup:
  https://github.com/zappyzed100/repodeck#readme
  (see README.en.md for English)

Files in this archive:
  repodeck.exe             - the main application
  repodeck-hook.exe        - helper invoked by Codex's own lifecycle hooks
  THIRD_PARTY_NOTICES.md   - third-party license attributions
  LICENSE                  - RepoDeck's own license (MIT)

Just run repodeck.exe - no installer, no admin rights required.
"@ | Set-Content -Path (Join-Path $stagingDir "README.txt") -Encoding utf8NoBOM

    $zipPath = Join-Path $distDir "$distName.zip"
    if (Test-Path $zipPath) {
        Remove-Item $zipPath -Force
    }
    Write-Host "Compressing $stagingDir -> $zipPath"
    Compress-Archive -Path $stagingDir -DestinationPath $zipPath

    $hash = Get-FileHash -Path $zipPath -Algorithm SHA256
    $checksumLine = "$($hash.Hash.ToLowerInvariant())  $distName.zip"
    $checksumPath = "$zipPath.sha256"
    $checksumLine | Set-Content -Path $checksumPath -Encoding utf8NoBOM

    Write-Host ""
    Write-Host "Done:"
    Write-Host "  $zipPath"
    Write-Host "  $checksumPath ($checksumLine)"
}
finally {
    Pop-Location
}

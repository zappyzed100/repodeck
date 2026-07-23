param(
  [int[]]$Sizes = @(3,6,9,12,15),
  [int]$Iters = 300,
  [int]$SettleMs = 1200
)
$ErrorActionPreference = 'Stop'
$repo = 'C:\code\portfolio\repodeck'
$exe = Join-Path $repo 'target\release\repodeck.exe'
$sim = Join-Path $repo 'target\release\repodeck-simtest.exe'
$cfg = Join-Path $env:LOCALAPPDATA 'RepoDeck\config.json'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$master = Join-Path $here '_master_config.json'
$spawnPs = Join-Path $here 'spawn-windows.ps1'
$outDir = Join-Path $here 'soak-results'
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

function Stop-RD {
  Get-Process repodeck -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
  for ($i=0; $i -lt 50; $i++) {
    if (-not (Get-Process repodeck -ErrorAction SilentlyContinue)) { return }
    Start-Sleep -Milliseconds 200
  }
}
function Stop-Spawner($p) {
  if ($p -and -not $p.HasExited) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue }
}

$spawner = $null
$summary = @()
try {
  foreach ($n in $Sizes) {
    Write-Host "`n========== SIZE $n sets ($($n*2) windows) =========="
    Stop-RD
    Stop-Spawner $spawner
    # Kill any stale SET-window owners (previous spawners) so the driver never
    # sees duplicate/unmanaged SET windows from an earlier run.
    & pwsh -NoProfile -File (Join-Path $here 'kill-setwins.ps1') | Write-Host
    Start-Sleep -Milliseconds 500

    Copy-Item $master $cfg -Force
    & python (Join-Path $here 'slice_config.py') $cfg $n | Write-Host

    $spawner = Start-Process pwsh -PassThru -WindowStyle Minimized `
      -ArgumentList '-NoProfile','-File',$spawnPs,'-Sets',$n
    Start-Sleep -Seconds 3

    $env:REPODECK_TEST_CONTROL = '1'
    Start-Process $exe
    Start-Sleep -Seconds 5

    $out = Join-Path $outDir "result_$n.json"
    Write-Host "[soak] running $Iters switches for size $n ..."
    & $sim '--iters' $Iters '--settle-ms' $SettleMs '--out' $out
    $code = $LASTEXITCODE
    Write-Host "[soak] size $n exit=$code"
    $summary += [pscustomobject]@{ size=$n; exit=$code; result=$out }

    Stop-RD
  }
}
finally {
  Stop-RD
  Stop-Spawner $spawner
}

Write-Host "`n========== SOAK SUMMARY =========="
foreach ($s in $summary) {
  if (Test-Path $s.result) {
    $j = Get-Content $s.result -Raw | ConvertFrom-Json
    $fc = @($j.failures).Count
    $verdict = if ($fc -eq 0) { 'PASS' } else { "FAIL ($fc)" }
    Write-Host ("  size {0,2}: switches={1} noops={2} -> {3}" -f $s.size,$j.switches,$j.noops,$verdict)
  } else {
    Write-Host ("  size {0,2}: NO RESULT (exit {1})" -f $s.size,$s.exit)
  }
}

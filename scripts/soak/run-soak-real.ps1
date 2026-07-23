param(
  [int[]]$Sizes = @(3,6,9,12,15),
  [int]$Iters = 150,
  [int]$SettleMs = 1500,
  [int]$ConfirmMs = 8000,
  [int]$Warmup = 12,
  [switch]$SkipLaunch
)
$ErrorActionPreference = 'Stop'
$repo='C:\code\portfolio\repodeck'
$exe = Join-Path $repo 'target\release\repodeck.exe'
$sim = Join-Path $repo 'target\release\repodeck-simtest.exe'
$cfg = Join-Path $env:LOCALAPPDATA 'RepoDeck\config.json'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$master = Join-Path $here '_real_master_config.json'
$outDir = Join-Path $here 'soak-results-real'
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

function Stop-RD {
  Get-Process repodeck -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
  for ($i=0;$i -lt 50;$i++){ if (-not (Get-Process repodeck -EA SilentlyContinue)){return}; Start-Sleep -Milliseconds 200 }
}

if (-not $SkipLaunch) {
  Write-Host "===== launching real app windows (once) ====="
  & pwsh -NoProfile -File (Join-Path $here 'launch-real.ps1') | Write-Host
  Write-Host "[soak] waiting 25s for windows to settle..."
  Start-Sleep -Seconds 25
}

$summary=@()
try {
  foreach ($n in $Sizes) {
    Write-Host "`n========== SIZE $n sets (Codex always incl.) =========="
    Stop-RD
    Copy-Item $master $cfg -Force
    & python (Join-Path $here 'slice_config.py') $cfg $n | Write-Host
    $env:REPODECK_TEST_CONTROL='1'
    Start-Process $exe
    Start-Sleep -Seconds 6
    $out = Join-Path $outDir "result_$n.json"
    Write-Host "[soak] $Iters switches, size $n (settle ${SettleMs}ms confirm ${ConfirmMs}ms)"
    & $sim '--iters' $Iters '--settle-ms' $SettleMs '--confirm-ms' $ConfirmMs '--warmup' $Warmup '--out' $out
    Write-Host "[soak] size $n exit=$LASTEXITCODE"
    $summary += [pscustomobject]@{ size=$n; result=$out }
    Stop-RD
  }
}
finally { Stop-RD }

Write-Host "`n========== REAL-APP SOAK SUMMARY =========="
foreach ($s in $summary) {
  if (Test-Path $s.result) {
    $j = Get-Content $s.result -Raw | ConvertFrom-Json
    $fc=@($j.failures).Count
    $v = if ($fc -eq 0){'PASS'} else {"FAIL ($fc)"}
    Write-Host ("  size {0,2}: switches={1} noops={2} transient={3} -> {4}" -f $s.size,$j.switches,$j.noops,$j.transient,$v)
  } else { Write-Host ("  size {0,2}: NO RESULT" -f $s.size) }
}

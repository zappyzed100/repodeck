$code  = Join-Path $env:LOCALAPPDATA 'Programs\Microsoft VS Code\bin\code.cmd'
$brave = 'C:\Program Files\BraveSoftware\Brave-Browser\Application\brave.exe'
Write-Host "[launch] VSCode windows (repo01-07 folder, repo08-14 workspace)"
for ($r=1; $r -le 14; $r++) {
  $rr = '{0:D2}' -f $r
  $path = if ($r -le 7) { "C:\code\test\repo$rr" } else { "C:\code\test\repo$rr\repo$rr.code-workspace" }
  Start-Process -FilePath $code -ArgumentList '-n','--disable-workspace-trust',$path -WindowStyle Hidden
  Start-Sleep -Milliseconds 1600
}
Write-Host "[launch] Brave windows BSET-01..15"
for ($n=1; $n -le 15; $n++) {
  $nn = '{0:D2}' -f $n
  $url = "file:///C:/code/test/pages/BSET-$nn.html"
  Start-Process -FilePath $brave -ArgumentList '--new-window',$url
  Start-Sleep -Milliseconds 700
}
Write-Host "[launch] done (ChatGPT app assumed already running)"

# Kill every process that owns a top-level window titled "SET-..".
Add-Type -Namespace W -Name U -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern bool EnumWindows(EnumProc cb, System.IntPtr p);
public delegate bool EnumProc(System.IntPtr h, System.IntPtr p);
[System.Runtime.InteropServices.DllImport("user32.dll", CharSet=System.Runtime.InteropServices.CharSet.Unicode)]
public static extern int GetWindowText(System.IntPtr h, System.Text.StringBuilder s, int n);
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern uint GetWindowThreadProcessId(System.IntPtr h, out uint pid);
'@
$pids = New-Object System.Collections.Generic.HashSet[uint32]
$cb = [W.U+EnumProc]{
  param($h,$p)
  $sb = New-Object System.Text.StringBuilder 256
  [void][W.U]::GetWindowText($h,$sb,256)
  if ($sb.ToString() -like 'SET-*') {
    $procId = 0; [void][W.U]::GetWindowThreadProcessId($h,[ref]$procId)
    [void]$pids.Add($procId)
  }
  return $true
}
[void][W.U]::EnumWindows($cb,[System.IntPtr]::Zero)
$me = $PID
foreach ($procId in $pids) {
  if ($procId -ne 0 -and $procId -ne $me) {
    try { Stop-Process -Id $procId -Force -ErrorAction Stop; Write-Host "[kill] pid $procId (SET window owner)" } catch {}
  }
}
if ($pids.Count -eq 0) { Write-Host "[kill] no SET windows found" }

Add-Type -Namespace W -Name U -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern bool EnumWindows(EnumProc cb, System.IntPtr p);
public delegate bool EnumProc(System.IntPtr h, System.IntPtr p);
[System.Runtime.InteropServices.DllImport("user32.dll", CharSet=System.Runtime.InteropServices.CharSet.Unicode)]
public static extern int GetWindowText(System.IntPtr h, System.Text.StringBuilder s, int n);
[System.Runtime.InteropServices.DllImport("user32.dll", CharSet=System.Runtime.InteropServices.CharSet.Unicode)]
public static extern int GetClassName(System.IntPtr h, System.Text.StringBuilder s, int n);
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern uint GetWindowThreadProcessId(System.IntPtr h, out uint pid);
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern bool IsWindowVisible(System.IntPtr h);
'@
$rows=@()
$cb = [W.U+EnumProc]{
  param($h,$p)
  if (-not [W.U]::IsWindowVisible($h)) { return $true }
  $t = New-Object System.Text.StringBuilder 512; [void][W.U]::GetWindowText($h,$t,512)
  $title=$t.ToString(); if ($title.Length -eq 0) { return $true }
  $c = New-Object System.Text.StringBuilder 256; [void][W.U]::GetClassName($h,$c,256)
  $procId=0; [void][W.U]::GetWindowThreadProcessId($h,[ref]$procId)
  $pname=''; $ppath=''
  try { $pr=Get-Process -Id $procId -EA Stop; $pname=$pr.ProcessName; $ppath=$pr.Path } catch {}
  $script:rows += [pscustomobject]@{ title=$title; class=$c.ToString(); pid=$procId; pname=$pname; path=$ppath }
  return $true
}
[void][W.U]::EnumWindows($cb,[System.IntPtr]::Zero)
Write-Host "=== ChatGPT / Codex windows ==="
$rows | Where-Object { $_.pname -like '*ChatGPT*' -or $_.title -like '*ChatGPT*' -or $_.pname -like '*Codex*' } | Format-List
Write-Host "=== Brave windows (sample) ==="
$rows | Where-Object { $_.pname -like '*brave*' } | Select-Object -First 3 title,class,pname,path | Format-List
Write-Host "=== Code windows (sample) ==="
$rows | Where-Object { $_.pname -like '*Code*' } | Select-Object -First 3 title,class,pname,path | Format-List

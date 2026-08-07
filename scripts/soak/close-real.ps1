Add-Type -Namespace W -Name C -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern bool EnumWindows(EnumProc cb, System.IntPtr p);
public delegate bool EnumProc(System.IntPtr h, System.IntPtr p);
[System.Runtime.InteropServices.DllImport("user32.dll", CharSet=System.Runtime.InteropServices.CharSet.Unicode)]
public static extern int GetWindowText(System.IntPtr h, System.Text.StringBuilder s, int n);
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern System.IntPtr PostMessage(System.IntPtr h, uint m, System.IntPtr w, System.IntPtr l);
'@
$targets=@()
$cb=[W.C+EnumProc]{ param($h,$p)
  $sb=New-Object System.Text.StringBuilder 512; [void][W.C]::GetWindowText($h,$sb,512)
  $t=$sb.ToString()
  if ($t -match '^repo\d\d' -or $t -match 'WS repo\d\d' -or $t -match 'BSET-\d\d') { $script:targets += $h }
  return $true }
[void][W.C]::EnumWindows($cb,[System.IntPtr]::Zero)
foreach ($h in $targets) { [void][W.C]::PostMessage($h,0x0010,[System.IntPtr]::Zero,[System.IntPtr]::Zero) }
Write-Host "[close] WM_CLOSE sent to $($targets.Count) test windows"

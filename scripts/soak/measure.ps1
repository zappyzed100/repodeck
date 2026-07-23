Add-Type -Namespace M -Name W -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern bool EnumWindows(EnumProc cb, System.IntPtr p);
public delegate bool EnumProc(System.IntPtr h, System.IntPtr p);
[System.Runtime.InteropServices.DllImport("user32.dll", CharSet=System.Runtime.InteropServices.CharSet.Unicode)]
public static extern int GetWindowText(System.IntPtr h, System.Text.StringBuilder s, int n);
[System.Runtime.InteropServices.DllImport("user32.dll")] public static extern bool IsWindowVisible(System.IntPtr h);
[System.Runtime.InteropServices.DllImport("user32.dll")] public static extern bool IsIconic(System.IntPtr h);
[System.Runtime.InteropServices.DllImport("user32.dll")] public static extern bool GetWindowRect(System.IntPtr h, out RECT r);
[System.Runtime.InteropServices.DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(System.IntPtr h, int a, out RECT r, int s);
public struct RECT { public int L,T,R,B; }
'@
$script:rows=@()
$want = 'ChatGPT','BSET-0','repo0','WS repo'
$cb=[M.W+EnumProc]{ param($h,$p)
  if (-not [M.W]::IsWindowVisible($h)) { return $true }
  $sb=New-Object System.Text.StringBuilder 512; [void][M.W]::GetWindowText($h,$sb,512); $t=$sb.ToString()
  if ($t.Length -eq 0) { return $true }
  foreach ($w in $want) { if ($t -like "*$w*") {
    $fr=New-Object M.W+RECT; [void][M.W]::GetWindowRect($h,[ref]$fr)
    $vb=New-Object M.W+RECT; [void][M.W]::DwmGetWindowAttribute($h,9,[ref]$vb,16)
    $ic=[M.W]::IsIconic($h)
    $script:rows += ("{0,-24} min={1} vis=({2},{3} {4}x{5})" -f $t.Substring(0,[Math]::Min(24,$t.Length)),$ic,$vb.L,$vb.T,($vb.R-$vb.L),($vb.B-$vb.T))
    break } }
  return $true }
[void][M.W]::EnumWindows($cb,[System.IntPtr]::Zero)
$script:rows | Sort-Object | ForEach-Object { Write-Host $_ }

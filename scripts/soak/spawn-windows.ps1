param([int]$Sets = 15)
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$ctx = New-Object System.Windows.Forms.ApplicationContext
$colors = @([System.Drawing.Color]::LightSteelBlue,[System.Drawing.Color]::Khaki,[System.Drawing.Color]::PaleGreen,[System.Drawing.Color]::LightPink,[System.Drawing.Color]::Wheat)
$i = 0
for ($s = 1; $s -le $Sets; $s++) {
  foreach ($ab in @('A','B')) {
    $title = "SET-{0:D2}-{1}" -f $s, $ab
    $f = New-Object System.Windows.Forms.Form
    $f.Text = $title
    $f.Width = 420; $f.Height = 300
    $f.StartPosition = 'Manual'
    $f.Location = New-Object System.Drawing.Point( (60 + ($i % 8) * 90), (60 + [math]::Floor($i / 8) * 70) )
    $f.BackColor = $colors[$s % $colors.Length]
    $f.ShowInTaskbar = $true
    $lbl = New-Object System.Windows.Forms.Label
    $lbl.Text = $title
    $lbl.Font = New-Object System.Drawing.Font('Segoe UI', 28, [System.Drawing.FontStyle]::Bold)
    $lbl.AutoSize = $false
    $lbl.Dock = 'Fill'
    $lbl.TextAlign = 'MiddleCenter'
    $f.Controls.Add($lbl)
    $f.Show()
    $i++
  }
}
Write-Host "[spawn] $i windows for $Sets sets"
[System.Windows.Forms.Application]::Run($ctx)

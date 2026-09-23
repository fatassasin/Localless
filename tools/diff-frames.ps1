# Bounding box of everything that changed between two captured frames.
#
# The film already contains the bug; the open question is *whose* rectangle it
# is. A changed-pixel bbox answers that numerically, where reading the PNG by
# eye does not: the pill window is a known rect, and either the strip lines up
# with its edges or it belongs to something else.
#
# ASCII only: Windows PowerShell reads .ps1 as ANSI.
param([string]$Dir = 'graphify-out\xflash', [string]$Base = 'f000', [string]$Cmp = 'f104',
      [int]$OriginX = 1370, [int]$OriginY = 1894)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

function Load([string]$pat) {
  $f = Get-ChildItem $Dir -Filter "$pat*.png" | Select-Object -First 1
  if (-not $f) { throw "no frame matching $pat in $Dir" }
  return [System.Drawing.Bitmap]::FromFile($f.FullName)
}
$a = Load $Base; $b = Load $Cmp
$w = $a.Width; $h = $a.Height
$ra = New-Object Drawing.Rectangle 0,0,$w,$h
$fmt = [Drawing.Imaging.PixelFormat]::Format32bppArgb
$da = $a.LockBits($ra, 'ReadOnly', $fmt); $db = $b.LockBits($ra, 'ReadOnly', $fmt)
$n = $w * $h * 4
$pa = New-Object byte[] $n; $pb = New-Object byte[] $n
[Runtime.InteropServices.Marshal]::Copy($da.Scan0, $pa, 0, $n)
[Runtime.InteropServices.Marshal]::Copy($db.Scan0, $pb, 0, $n)
$a.UnlockBits($da); $b.UnlockBits($db)

$minx = $w; $miny = $h; $maxx = -1; $maxy = -1; $cnt = 0
for ($y = 0; $y -lt $h; $y++) {
  $row = $y * $da.Stride
  for ($x = 0; $x -lt $w; $x++) {
    $i = $row + $x * 4
    # 24 per channel: well above JPEG-free PNG noise, well below a real repaint.
    if ([Math]::Abs($pa[$i] - $pb[$i]) -gt 24 -or [Math]::Abs($pa[$i+1] - $pb[$i+1]) -gt 24 -or [Math]::Abs($pa[$i+2] - $pb[$i+2]) -gt 24) {
      $cnt++
      if ($x -lt $minx) { $minx = $x }; if ($x -gt $maxx) { $maxx = $x }
      if ($y -lt $miny) { $miny = $y }; if ($y -gt $maxy) { $maxy = $y }
    }
  }
}
if ($maxx -lt 0) { Write-Host "$Base vs ${Cmp}: identical"; exit }
Write-Host ("{0} vs {1}: {2} px changed" -f $Base, $Cmp, $cnt)
Write-Host ("  image bbox  {0},{1} .. {2},{3}  ({4}x{5})" -f $minx,$miny,$maxx,$maxy,($maxx-$minx+1),($maxy-$miny+1))
Write-Host ("  SCREEN bbox {0},{1} .. {2},{3}  ({4}x{5})" -f ($minx+$OriginX),($miny+$OriginY),($maxx+$OriginX),($maxy+$OriginY),($maxx-$minx+1),($maxy-$miny+1))

# Per-row change counts around the top of the strip: a caption bar is a solid
# run of full-width rows, an animation marquee is two thin rows.
Write-Host '  rows with >200 changed px:'
for ($y = 0; $y -lt $h; $y++) {
  $row = $y * $da.Stride; $c = 0
  for ($x = 0; $x -lt $w; $x++) {
    $i = $row + $x * 4
    if ([Math]::Abs($pa[$i] - $pb[$i]) -gt 24 -or [Math]::Abs($pa[$i+1] - $pb[$i+1]) -gt 24 -or [Math]::Abs($pa[$i+2] - $pb[$i+2]) -gt 24) { $c++ }
  }
  if ($c -gt 200) { Write-Host ("    y={0} (screen {1})  {2}" -f $y, ($y+$OriginY), $c) }
}

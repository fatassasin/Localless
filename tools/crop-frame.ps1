# Crop and upscale a region of a captured frame, so the artefact can be read
# instead of guessed at. The diff bbox is where to point it.
# ASCII only: Windows PowerShell reads .ps1 as ANSI.
param([string]$Dir='graphify-out\xflash', [string]$Frame='f104',
      [int]$X=30, [int]$Y=35, [int]$W=740, [int]$H=110, [int]$Zoom=2, [string]$Out='crop.png')
$ErrorActionPreference='Stop'
Add-Type -AssemblyName System.Drawing
$f = Get-ChildItem $Dir -Filter "$Frame*.png" | Select-Object -First 1
if (-not $f) { throw "no frame $Frame" }
$src = [Drawing.Bitmap]::FromFile($f.FullName)
$dst = New-Object Drawing.Bitmap (($W*$Zoom), ($H*$Zoom))
$g = [Drawing.Graphics]::FromImage($dst)
$g.InterpolationMode = 'NearestNeighbor'; $g.PixelOffsetMode = 'Half'
$g.DrawImage($src, (New-Object Drawing.Rectangle 0,0,($W*$Zoom),($H*$Zoom)),
             (New-Object Drawing.Rectangle $X,$Y,$W,$H), 'Pixel')
$g.Dispose()
$p = Join-Path (Resolve-Path $Dir).Path $Out
$dst.Save($p, [Drawing.Imaging.ImageFormat]::Png)
Write-Host "wrote $p ($($dst.Width)x$($dst.Height)) from $($f.Name)"

# Grab a rectangle of the composited desktop. The window-level PrintWindow in
# shot-window.ps1 cannot answer the question this one is for: whether the pill's
# SetWindowRgn cuts a hard edge out of its box-shadow, and whether the area
# outside the region really shows what is underneath. Both are only visible in
# the composited result.
#
# ASCII only: Windows PowerShell reads .ps1 as ANSI.
param([int]$X = 0, [int]$Y = 0, [int]$W = 0, [int]$H = 0, [string]$Out = "$env:TEMP\localless-desk.png")
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Dk {
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
}
"@
[void][Dk]::SetProcessDPIAware()
if ($W -le 0) { $W = [Dk]::GetSystemMetrics(0) }
if ($H -le 0) { $H = [Dk]::GetSystemMetrics(1) }
$bmp = New-Object System.Drawing.Bitmap($W, $H)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($X, $Y, 0, 0, (New-Object System.Drawing.Size($W, $H)))
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()
Write-Host ("saved {0} ({1}x{2} at {3},{4})" -f $Out, $W, $H, $X, $Y)

# Screenshot one of localless.exe's windows to a PNG so the look can actually be
# checked instead of described. Picks the window whose width is closest to -Width.
#
# PrintWindow with PW_RENDERFULLCONTENT (2) is required: WebView2 renders through
# a child compositor surface, and plain BitBlt of the desktop would capture
# whatever is on top of it instead.
#
# ASCII only: Windows PowerShell reads .ps1 as ANSI.
param([int]$Width = 1440, [string]$Out = "$env:TEMP\localless-shot.png")
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class S {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  // Without this the process is DPI-virtualized: GetWindowRect hands back logical
  // pixels while WebView2 renders at physical ones, so the bitmap comes out the
  // right aspect but holds only the top-left 1/scale^2 of the window. That looks
  // like "the settings page is zoomed in", not like a measurement bug.
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  public struct RECT { public int left, top, right, bottom; }
  public static List<IntPtr> ForPid(uint want) {
    var o = new List<IntPtr>();
    EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == want) o.Add(h); return true; }, IntPtr.Zero);
    return o;
  }
}
"@
$pid0 = (Get-Process localless -ErrorAction Stop | Select-Object -First 1).Id
[void][S]::SetProcessDPIAware()
$best = [IntPtr]::Zero; $bestDiff = [int]::MaxValue; $bw = 0; $bh = 0
foreach ($h in [S]::ForPid([uint32]$pid0)) {
  $r = New-Object S+RECT
  [void][S]::GetWindowRect($h, [ref]$r)
  $w = $r.right - $r.left; $hh = $r.bottom - $r.top
  if (-not [S]::IsWindowVisible($h) -or $w -lt 200 -or $hh -lt 200) { continue }
  $d = [Math]::Abs($w - $Width)
  if ($d -lt $bestDiff) { $bestDiff = $d; $best = $h; $bw = $w; $bh = $hh }
}
if ($best -eq [IntPtr]::Zero) { Write-Host 'no candidate window'; exit 1 }
[void][S]::SetForegroundWindow($best)
Start-Sleep -Milliseconds 600
$bmp = New-Object System.Drawing.Bitmap $bw, $bh
$g = [System.Drawing.Graphics]::FromImage($bmp)
$dc = $g.GetHdc()
$ok = [S]::PrintWindow($best, $dc, 2)
$g.ReleaseHdc($dc); $g.Dispose()
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
Write-Host ("hwnd=0x{0:X} {1}x{2} PrintWindow={3} -> {4}" -f [int64]$best, $bw, $bh, $ok, $Out)

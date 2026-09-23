# List every top-level window of the running localless process with the three
# numbers that decide whether the pill can be seen and whether it eats clicks:
#
#   rect   -- GetWindowRect, physical pixels. The pill window must be only as
#             big as the pill itself. Anything the size of the screen here is
#             the "whole screen stops taking clicks" bug coming back.
#   rgn    -- GetWindowRgnBox: 0=no region (whole window), 1=empty (occupies
#             nothing), 2=simple, 3=complex.
#   ex     -- WS_EX_TRANSPARENT (0x20) / WS_EX_NOACTIVATE (0x8000000) /
#             WS_EX_TOPMOST (0x8).
#
# ASCII only: Windows PowerShell reads .ps1 as ANSI.
#
# SetProcessDPIAware first, or every number below is a lie: a DPI-unaware
# process gets a virtualized desktop and GetWindowRect comes back divided by
# the scale factor. On this machine (175%) that turns a correct bottom-center
# placement into something that looks a thousand pixels off.
$ErrorActionPreference = 'Stop'

Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class W {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern int GetWindowLongW(IntPtr h, int i);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern int GetWindowRgnBox(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
  [DllImport("user32.dll")] public static extern bool SystemParametersInfoW(uint a, uint b, out RECT r, uint c);
  public struct RECT { public int left, top, right, bottom; }
  public struct POINT { public int x, y; }
  public static List<IntPtr> ForPid(uint want) {
    var o = new List<IntPtr>();
    EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == want) o.Add(h); return true; }, IntPtr.Zero);
    return o;
  }
}
"@

$procId = (Get-Process localless -ErrorAction Stop | Select-Object -First 1).Id
$rgnName = @{ 0 = 'none(whole)'; 1 = 'empty'; 2 = 'simple'; 3 = 'complex' }

[void][W]::SetProcessDPIAware()
$wa = New-Object W+RECT
[void][W]::SystemParametersInfoW(0x30, 0, [ref]$wa, 0)
"screen={0}x{1}  workArea={2},{3}..{4},{5}" -f [W]::GetSystemMetrics(0), [W]::GetSystemMetrics(1), $wa.left, $wa.top, $wa.right, $wa.bottom

foreach ($h in [W]::ForPid([uint32]$procId)) {
  $r = New-Object W+RECT
  [void][W]::GetWindowRect($h, [ref]$r)
  $b = New-Object W+RECT
  $kind = [W]::GetWindowRgnBox($h, [ref]$b)
  $ex = [W]::GetWindowLongW($h, -20)
  $tag = @()
  if ($ex -band 0x20) { $tag += 'TRANSPARENT' }
  if ($ex -band 0x8000000) { $tag += 'NOACTIVATE' }
  if ($ex -band 0x8) { $tag += 'TOPMOST' }
  $w = $r.right - $r.left; $hh = $r.bottom - $r.top
  $rgn = $rgnName[$kind]
  if ($kind -ge 2) { $rgn += " $($b.left),$($b.top)..$($b.right),$($b.bottom)" }
  '{0,-10} vis={1,-5} rect={2},{3} {4}x{5}  rgn={6}  ex={7}' -f `
    ('0x' + $h.ToString('X')), [W]::IsWindowVisible($h), $r.left, $r.top, $w, $hh, $rgn, ($tag -join '+')
}

# Who answers a click in the middle of the screen? If this ever comes back as
# one of the windows above while the pill is showing, the pill is stealing the
# whole screen again.
$p = New-Object W+POINT
$p.x = 700; $p.y = 400
$hit = [W]::WindowFromPoint($p)
$hitPid = 0
[void][W]::GetWindowThreadProcessId($hit, [ref]$hitPid)
$owner = try { (Get-Process -Id $hitPid).ProcessName } catch { '?' }
"click(700,400) -> 0x$($hit.ToString('X')) pid=$hitPid $owner"

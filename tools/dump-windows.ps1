# Dump the live state of every top-level window owned by localless.exe.
#
# The pill is a full-workArea transparent always-on-top window that is only
# harmless because WS_EX_TRANSPARENT is set. If that bit is off while the page
# is showing nothing, the whole screen stops taking clicks and there is nothing
# on screen to explain why -- which is exactly the reported symptom. So read the
# bit rather than reasoning about the code that is supposed to set it.
#
# ASCII only: Windows PowerShell reads .ps1 as ANSI.
$ErrorActionPreference = 'Stop'

Add-Type @"
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class W {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern int GetWindowLongW(IntPtr h, int i);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
  [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
  public struct RECT { public int left, top, right, bottom; }
  public struct POINT { public int x, y; }
  public static List<IntPtr> ForPid(uint want) {
    var outp = new List<IntPtr>();
    EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == want) outp.Add(h); return true; }, IntPtr.Zero);
    return outp;
  }
}
"@

$procs = @(Get-Process localless -ErrorAction SilentlyContinue)
Write-Host "localless.exe instances: $($procs.Count)  pids=$($procs.Id -join ',')"
if ($procs.Count -eq 0) { Write-Host 'not running'; exit 0 }

$GWL_EXSTYLE = -20
$WS_EX_TRANSPARENT = 0x20
$WS_EX_LAYERED     = 0x80000
$WS_EX_TOPMOST     = 0x8
$WS_EX_NOACTIVATE  = 0x08000000

foreach ($p in $procs) {
  Write-Host ""
  Write-Host "=== pid $($p.Id) ==="
  foreach ($h in [W]::ForPid([uint32]$p.Id)) {
    $t = New-Object System.Text.StringBuilder 256
    $c = New-Object System.Text.StringBuilder 256
    [void][W]::GetWindowTextW($h, $t, 256)
    [void][W]::GetClassNameW($h, $c, 256)
    $ex = [W]::GetWindowLongW($h, $GWL_EXSTYLE)
    $vis = [W]::IsWindowVisible($h)
    $r = New-Object W+RECT
    [void][W]::GetWindowRect($h, [ref]$r)
    $flags = @()
    if ($ex -band $WS_EX_TRANSPARENT) { $flags += 'TRANSPARENT(click-through)' } else { $flags += '** NOT transparent -> EATS CLICKS **' }
    if ($ex -band $WS_EX_LAYERED)    { $flags += 'LAYERED' }
    if ($ex -band $WS_EX_TOPMOST)    { $flags += 'TOPMOST' }
    if ($ex -band $WS_EX_NOACTIVATE) { $flags += 'NOACTIVATE' }
    Write-Host ("  hwnd=0x{0:X} vis={1} rect={2},{3} {4}x{5}" -f [int64]$h, $vis, $r.left, $r.top, ($r.right-$r.left), ($r.bottom-$r.top))
    Write-Host ("     class='$($c.ToString())' title='$($t.ToString())'")
    Write-Host ("     exstyle=0x{0:X}  {1}" -f $ex, ($flags -join ' | '))
  }
}

# What actually receives a click at the cursor right now.
$pt = New-Object W+POINT
[void][W]::GetCursorPos([ref]$pt)
$under = [W]::WindowFromPoint($pt)
$uc = New-Object System.Text.StringBuilder 256
[void][W]::GetClassNameW($under, $uc, 256)
$upid = 0
[void][W]::GetWindowThreadProcessId($under, [ref]$upid)
$uproc = (Get-Process -Id $upid -ErrorAction SilentlyContinue).ProcessName
Write-Host ""
Write-Host "cursor at $($pt.x),$($pt.y) -> hwnd=0x$('{0:X}' -f [int64]$under) class='$($uc.ToString())' pid=$upid proc=$uproc"

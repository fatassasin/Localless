# Watch EVERY top-level window of localless.exe while clicking the result pill's X.
#
# Why not probe-pillstyle.ps1: that one locks onto the pill hwnd and only reads
# the pill. A 26s demo run showed the pill's GWL_STYLE never leaves 0x94000000
# (POPUP|VISIBLE|CLIPSIBLINGS, no WS_CAPTION), so the reported "a little title
# bar flashes when I click the X" is NOT the pill painting a caption. It has to
# be some other window -- one that may be created and destroyed inside that same
# frame, which is why this enumerates on every sample instead of resolving a
# handle once.
#
# Also drives the click itself. Waiting for a human to hit a 26px target during
# a 4.2s demo window is not reproducible; this fires SendInput at the X the
# moment the pill goes wide (result state), so the before/after samples bracket
# the exact click.
#
# Prints one line per change only. ASCII only: Windows PowerShell reads .ps1 as ANSI.
param([int]$Seconds = 40, [switch]$NoClick)
$ErrorActionPreference = 'Stop'

Add-Type @"
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class XW {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern IntPtr GetWindowLongPtrW(IntPtr h, int i);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern int GetWindowRgnBox(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint x, uint y, uint d, IntPtr e);
  public struct RECT { public int left, top, right, bottom; }
  public static List<IntPtr> ForPid(uint want) {
    var o = new List<IntPtr>();
    EnumWindows((h,l) => { uint p; GetWindowThreadProcessId(h, out p); if (p==want) o.Add(h); return true; }, IntPtr.Zero);
    return o;
  }
}
"@

$p = @(Get-Process localless -ErrorAction SilentlyContinue)[0]
if (-not $p) { Write-Host 'localless not running'; exit 1 }
$pid_ = [uint32]$p.Id
Write-Host "watching every top-level window of pid $pid_ for ${Seconds}s"

$GWL_STYLE = -16; $GWL_EXSTYLE = -20

function StyleTag([int64]$s) {
  $f = @()
  if ($s -band 0x00C00000) { $f += 'CAPTION' }
  if ($s -band 0x00040000) { $f += 'THICKFRAME' }
  if ($s -band 0x00080000) { $f += 'SYSMENU' }
  if ($s -band 0x00800000) { $f += 'BORDER' }
  if ($s -band 0x00400000) { $f += 'DLGFRAME' }
  if (-not ($s -band 0x80000000)) { $f += 'no-POPUP' }
  if ($f.Count) { return ($f -join '+') } else { return 'clean' }
}

# One line of state per window. Everything that could make a strip appear on
# screen is in here: whether it is shown at all, whether it has a frame, how big
# it is, and how much of it the region lets through.
function Snap([IntPtr]$h) {
  $st = [int64][XW]::GetWindowLongPtrW($h, $GWL_STYLE)
  $ex = [int64][XW]::GetWindowLongPtrW($h, $GWL_EXSTYLE)
  $r = New-Object XW+RECT; [void][XW]::GetWindowRect($h, [ref]$r)
  $g = New-Object XW+RECT; $rc = [XW]::GetWindowRgnBox($h, [ref]$g)
  $rgn = if ($rc -eq 1) { 'EMPTY' } elseif ($rc -eq 0) { 'NONE(full)' } else { "$($g.right-$g.left)x$($g.bottom-$g.top)" }
  $c = New-Object System.Text.StringBuilder 64; [void][XW]::GetClassNameW($h, $c, 64)
  return ("cls={0} vis={1} style=0x{2:X8} {3} ex=0x{4:X8} at={5},{6} {7}x{8} rgn={9}" -f `
    $c.ToString(), [XW]::IsWindowVisible($h), $st, (StyleTag $st), $ex, `
    $r.left, $r.top, ($r.right-$r.left), ($r.bottom-$r.top), $rgn)
}

# The X sits at the head of the pill's flex row. Window-local offset, derived
# from pill.html: --ll-pad 28 + (padding-left 7 + half of the 26px button) * .75
# horizontally, 28 + half of 47 * .75 vertically. The window is sized 1:1 with
# CSS px (window h 92 == 28*2 + 47*.75), so no DPI conversion here.
$XOFF = 43; $YOFF = 46
$clicked = $false

$seen = @{}
$sw = [Diagnostics.Stopwatch]::StartNew()
while ($sw.Elapsed.TotalSeconds -lt $Seconds) {
  $now = @{}
  foreach ($h in [XW]::ForPid($pid_)) {
    $k = [string]([int64]$h)
    $s = Snap $h
    $now[$k] = $s
    if (-not $seen.ContainsKey($k)) {
      Write-Host ("{0}  +NEW  0x{1:X}  {2}" -f (Get-Date -Format HH:mm:ss.fff), [int64]$h, $s)
    } elseif ($seen[$k] -ne $s) {
      Write-Host ("{0}        0x{1:X}  {2}" -f (Get-Date -Format HH:mm:ss.fff), [int64]$h, $s)
    }

    # Fire the click once, when the pill is wide and actually showing: that is
    # the result state, the one the report is about.
    if (-not $NoClick -and -not $clicked -and $s -match 'rgn=(\d+)x' -and [int]$Matches[1] -gt 300) {
      $r = New-Object XW+RECT; [void][XW]::GetWindowRect($h, [ref]$r)
      $cx = $r.left + $XOFF; $cy = $r.top + $YOFF
      Write-Host ("{0}  CLICK X at {1},{2}" -f (Get-Date -Format HH:mm:ss.fff), $cx, $cy)
      [void][XW]::SetCursorPos($cx, $cy)
      [XW]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
      [XW]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
      $clicked = $true
    }
  }
  foreach ($k in @($seen.Keys)) {
    if (-not $now.ContainsKey($k)) {
      Write-Host ("{0}  -GONE 0x{1:X}" -f (Get-Date -Format HH:mm:ss.fff), [int64]$k)
    }
  }
  $seen = $now
  Start-Sleep -Milliseconds 2
}
Write-Host 'done'

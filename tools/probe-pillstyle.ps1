# Watch the pill window's GWL_STYLE and window region for changes.
#
# Why this exists: pill.rs strips WS_CAPTION at startup (fit -> no_frame), but
# tao rewrites GWL_STYLE from its own cached WindowFlags on every set_size /
# set_position. place() runs on EVERY pill_rect, while apply_ex (the only thing
# that strips the caption back off) runs only on the hidden<->shown transition.
# So a mid-show resize can leave WS_CAPTION on with nothing to clean it up.
# That is a claim about live window state, so read the state instead of the code.
#
# Prints one line per change only. Ctrl+C or -Seconds to stop.
# ASCII only: Windows PowerShell reads .ps1 as ANSI.
param([int]$Seconds = 60)
$ErrorActionPreference = 'Stop'

Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class PS_ {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern IntPtr GetWindowLongPtrW(IntPtr h, int i);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern int GetWindowRgnBox(IntPtr h, out RECT r);
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

# The pill is the widest of the process's top-level windows that is not the
# 40x40 mic overlay and not a 0x0 message-only window. Pick by size, not by
# title: GetWindowTextW on these comes back as a single character.
$best = $null; $bestArea = 0
foreach ($h in [PS_]::ForPid([uint32]$p.Id)) {
  $r = New-Object PS_+RECT
  [void][PS_]::GetWindowRect($h, [ref]$r)
  $w = $r.right - $r.left; $ht = $r.bottom - $r.top
  if ($w -ge 60 -and $ht -ge 40 -and $ht -le 400 -and ($w*$ht) -gt $bestArea) { $bestArea = $w*$ht; $best = $h }
}
if (-not $best) { Write-Host 'pill window not found'; exit 1 }
Write-Host ("watching hwnd=0x{0:X} for {1}s" -f [int64]$best, $Seconds)

$CAPTION=0x00C00000; $THICK=0x00040000; $SYSMENU=0x00080000; $POPUP=0x80000000

$last = ''
$stop = (Get-Date).AddSeconds($Seconds)
while ((Get-Date) -lt $stop) {
  $s = [int64][PS_]::GetWindowLongPtrW($best, -16)
  $r = New-Object PS_+RECT; [void][PS_]::GetWindowRect($best, [ref]$r)
  $g = New-Object PS_+RECT; $rc = [PS_]::GetWindowRgnBox($best, [ref]$g)
  $rgn = if ($rc -eq 0) { 'ERROR' } elseif ($rc -eq 1) { 'EMPTY' } else { "$($g.right-$g.left)x$($g.bottom-$g.top)" }
  $bits = @()
  if ($s -band $CAPTION) { $bits += 'CAPTION' }
  if ($s -band $THICK)   { $bits += 'THICKFRAME' }
  if ($s -band $SYSMENU) { $bits += 'SYSMENU' }
  if (-not ($s -band $POPUP)) { $bits += 'no-POPUP' }
  $tag = if ($bits.Count) { $bits -join '|' } else { 'clean' }
  $key = "$('{0:X8}' -f $s) rgn=$rgn win=$($r.right-$r.left)x$($r.bottom-$r.top)"
  if ($key -ne $last) {
    $last = $key
    Write-Host ("{0:HH:mm:ss.fff}  style=0x{1}  {2}" -f (Get-Date), ('{0:X8}' -f $s), "$tag  rgn=$rgn  win=$($r.right-$r.left)x$($r.bottom-$r.top)")
  }
  Start-Sleep -Milliseconds 2
}
Write-Host 'done'

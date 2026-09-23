# Click the floating mic once, for real.
# Real mouse, not PostMessage: WebView2 pointer events ignore synthesized
# messages, and this window carries WS_EX_NOACTIVATE -- a real click is
# exactly what we need to exercise.
#
# Windows are matched by owning PID + size, never by title: this file is
# read as ANSI by Windows PowerShell, so any non-ASCII literal compared
# against a real window title silently never matches. That cost one run.
param([int]$Pid_ = 0)
$ErrorActionPreference = 'Stop'
Add-Type @"
using System;using System.Runtime.InteropServices;using System.Text;
public class U {
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc f, IntPtr l);
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint p);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x,int y);
  [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f,uint x,uint y,uint d,UIntPtr e);
  public struct RECT { public int left,top,right,bottom; }
  public struct POINT { public int x,y; }
}
"@

function Find-Mic {
  $global:micH = [IntPtr]::Zero
  $cb = [U+EnumProc]{
    param($h,$l)
    if ([U]::IsWindowVisible($h)) {
      $p = 0
      [void][U]::GetWindowThreadProcessId($h, [ref]$p)
      if ($p -eq $script:want) {
        $r = New-Object U+RECT
        [void][U]::GetWindowRect($h, [ref]$r)
        $w = $r.right - $r.left; $ht = $r.bottom - $r.top
        # The pill covers the whole work area; the mic is the small square one.
        if ($w -gt 0 -and $w -lt 400 -and $ht -lt 400) { $global:micH = $h; return $false }
      }
    }
    return $true
  }
  [void][U]::EnumWindows($cb, [IntPtr]::Zero)
  return $global:micH
}

$script:want = $Pid_
for ($i = 0; $i -lt 40; $i++) {
  $h = Find-Mic
  if ($h -ne [IntPtr]::Zero) { break }
  Start-Sleep -Milliseconds 500
}
if ($h -eq [IntPtr]::Zero) { Write-Host 'no floating-mic window found'; exit 1 }

$r = New-Object U+RECT
[void][U]::GetWindowRect($h, [ref]$r)
$cx = [int](($r.left + $r.right) / 2)
$cy = [int](($r.top + $r.bottom) / 2)
Write-Host "hwnd=$h rect=$($r.left),$($r.top),$($r.right),$($r.bottom) center=$cx,$cy"

$old = New-Object U+POINT
[void][U]::GetCursorPos([ref]$old)
[void][U]::SetCursorPos($cx, $cy)
Start-Sleep -Milliseconds 250
[U]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero)   # LEFTDOWN
Start-Sleep -Milliseconds 90
[U]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero)   # LEFTUP
Start-Sleep -Milliseconds 1200
[void][U]::SetCursorPos($old.x, $old.y)
Write-Host 'clicked'

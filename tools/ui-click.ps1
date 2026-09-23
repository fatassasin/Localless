# Drive one of localless.exe's own windows: click at a CSS-pixel coordinate, and
# optionally tap a virtual key afterwards. This exists because the settings window
# has no scriptable surface at all -- it is opened from a tray icon that nothing
# can click, and every control on it is a DOM node with no UI Automation name we
# can rely on (the WebView2 tree comes back with one-character strings through
# ANSI marshalling, see dump-windows.ps1).
#
# Coordinates are CSS pixels measured from the window's CLIENT top-left, i.e. the
# same numbers you read off the page in a screenshot. The scale factor is read
# from GetDpiForWindow, so the caller never deals with physical pixels.
#
# SetProcessDPIAware is mandatory: without it SetCursorPos lands at 1/scale of
# where you asked, which on a 200% display means every click misses low and left
# and looks like "the button does not respond".
#
# ASCII only: Windows PowerShell reads .ps1 as ANSI.
param(
  [int]$Width = 1314,     # physical width used to pick the window
  [double]$X = -1,        # CSS px from client left; negative = do not click
  [double]$Y = -1,
  [int]$Key = 0,          # virtual-key code to tap after the click; 0 = none
  [int]$KeyDelayMs = 700, # wait between the click and the key tap
  [int]$Wheel = 0,        # wheel notches at X,Y before clicking; negative = scroll down
  [switch]$NoClick        # move/scroll only, do not press the button
)
$ErrorActionPreference = 'Stop'
Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class U {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  // dwData is int, not uint: a wheel-down notch is -120, and casting that to
  // uint32 in PowerShell throws instead of wrapping.
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint x, uint y, int d, IntPtr e);
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte sc, uint f, IntPtr e);
  [DllImport("user32.dll")] public static extern uint MapVirtualKey(uint code, uint type);
  public struct RECT { public int left, top, right, bottom; }
  public struct POINT { public int x, y; }
  public static List<IntPtr> ForPid(uint want) {
    var o = new List<IntPtr>();
    EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == want) o.Add(h); return true; }, IntPtr.Zero);
    return o;
  }
}
"@
[void][U]::SetProcessDPIAware()
$pid0 = (Get-Process localless -ErrorAction Stop | Select-Object -First 1).Id
$best = [IntPtr]::Zero; $bestDiff = [int]::MaxValue; $bw = 0; $bh = 0
foreach ($h in [U]::ForPid([uint32]$pid0)) {
  $r = New-Object U+RECT
  [void][U]::GetWindowRect($h, [ref]$r)
  $w = $r.right - $r.left; $hh = $r.bottom - $r.top
  if (-not [U]::IsWindowVisible($h) -or $w -lt 200 -or $hh -lt 200) { continue }
  $d = [Math]::Abs($w - $Width)
  if ($d -lt $bestDiff) { $bestDiff = $d; $best = $h; $bw = $w; $bh = $hh }
}
if ($best -eq [IntPtr]::Zero) { Write-Host 'no candidate window'; exit 1 }
$dpi = [U]::GetDpiForWindow($best)
if ($dpi -le 0) { $dpi = 96 }
$scale = $dpi / 96.0
[void][U]::SetForegroundWindow($best)
Start-Sleep -Milliseconds 400
Write-Host ("hwnd=0x{0:X} {1}x{2} scale={3}" -f [int64]$best, $bw, $bh, $scale)

if ($X -ge 0 -and $Y -ge 0) {
  $o = New-Object U+POINT
  [void][U]::ClientToScreen($best, [ref]$o)
  $px = $o.x + [int][Math]::Round($X * $scale)
  $py = $o.y + [int][Math]::Round($Y * $scale)
  [void][U]::SetCursorPos($px, $py)
  Start-Sleep -Milliseconds 120
  # Wheel first, then click: scrolling to a control and clicking it is one gesture,
  # and doing it the other way round would click whatever happened to be there before.
  if ($Wheel -ne 0) {
    for ($i = 0; $i -lt [Math]::Abs($Wheel); $i++) {
      $delta = if ($Wheel -lt 0) { -120 } else { 120 }
      [U]::mouse_event(0x0800, 0, 0, [int]$delta, [IntPtr]::Zero)  # MOUSEEVENTF_WHEEL
      Start-Sleep -Milliseconds 60
    }
    Write-Host ("wheeled {0} notches at CSS {1},{2}" -f $Wheel, $X, $Y)
    Start-Sleep -Milliseconds 300
  }
  if (-not $NoClick) {
    [U]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)   # LEFTDOWN
    Start-Sleep -Milliseconds 60
    [U]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)   # LEFTUP
    Write-Host ("clicked CSS {0},{1} -> screen {2},{3}" -f $X, $Y, $px, $py)
  }
}

if ($Key -ne 0) {
  Start-Sleep -Milliseconds $KeyDelayMs
  # The right-hand modifiers are EXTENDED keys. Send one without KEYEVENTF_EXTENDEDKEY
  # (0x0001) and a real scancode and the input stack hands the page a LEFT Alt / Ctrl
  # instead, so a capture box waiting for RightAlt just keeps waiting -- which looks
  # exactly like "the capture box is broken".
  $sc = [byte][U]::MapVirtualKey([uint32]$Key, 0)
  $ext = if ($Key -eq 0xA5 -or $Key -eq 0xA3 -or $Key -eq 0x5B -or $Key -eq 0x5C) { 1 } else { 0 }
  [U]::keybd_event([byte]$Key, $sc, [uint32]$ext, [IntPtr]::Zero)          # down
  Start-Sleep -Milliseconds 80
  [U]::keybd_event([byte]$Key, $sc, [uint32]($ext -bor 2), [IntPtr]::Zero) # up (KEYEVENTF_KEYUP)
  Write-Host ("tapped vk=0x{0:X} sc=0x{1:X} ext={2}" -f $Key, $sc, $ext)
}

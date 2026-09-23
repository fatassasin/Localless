# Watch localless.exe's windows appear and record, for each one, the rectangle it
# had at the FIRST moment it became visible.
#
# This is the only way to prove "it no longer flashes at the top-left". A window
# that is built visible and then moved reports its default rect (near 0,0) on the
# first visible sample and the final rect a few samples later; one that is built
# hidden, placed, and only then shown reports the final rect immediately. Polling
# GetWindowRect alone cannot tell those apart -- the visibility flag is the whole
# point.
#
# ASCII only: Windows PowerShell reads .ps1 as ANSI.
param([int]$Seconds = 20, [int]$IntervalMs = 15)
$ErrorActionPreference = 'Stop'
Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class P {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  public struct RECT { public int left, top, right, bottom; }
  public static List<IntPtr> ForPid(uint want) {
    var o = new List<IntPtr>();
    EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == want) o.Add(h); return true; }, IntPtr.Zero);
    return o;
  }
}
"@
[void][P]::SetProcessDPIAware()
$seen = @{}
$deadline = (Get-Date).AddSeconds($Seconds)
while ((Get-Date) -lt $deadline) {
  $proc = Get-Process localless -ErrorAction SilentlyContinue | Select-Object -First 1
  if ($proc) {
    foreach ($h in [P]::ForPid([uint32]$proc.Id)) {
      if (-not [P]::IsWindowVisible($h)) { continue }
      $r = New-Object P+RECT
      [void][P]::GetWindowRect($h, [ref]$r)
      $w = $r.right - $r.left; $hh = $r.bottom - $r.top
      if ($w -lt 200 -or $hh -lt 200) { continue }
      $k = [int64]$h
      if ($seen.ContainsKey($k)) { continue }
      $seen[$k] = $true
      Write-Host ("{0:HH:mm:ss.fff} first-visible hwnd=0x{1:X} {2}x{3} @{4},{5}" -f (Get-Date), $k, $w, $hh, $r.left, $r.top)
    }
  }
  Start-Sleep -Milliseconds $IntervalMs
}
Write-Host ("done, {0} window(s)" -f $seen.Count)

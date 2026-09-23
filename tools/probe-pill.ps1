# Prove the pill window stays click-through WHILE recording.
#
# Bug being guarded: the pill window covers the whole work area and is
# invisible, so if WS_EX_TRANSPARENT is missing the entire screen silently
# stops taking clicks with nothing on screen to explain it. That is what
# "I cannot click anything while dictating" was. Recording can stay on
# indefinitely, so this is the state that matters, not the idle one.
#
# ASCII only: Windows PowerShell reads .ps1 as ANSI.
$ErrorActionPreference = 'Stop'

Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class P {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern int GetWindowLongW(IntPtr h, int i);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
  public struct RECT { public int left, top, right, bottom; }
  public static List<IntPtr> ForPid(uint want) {
    var o = new List<IntPtr>();
    EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == want) o.Add(h); return true; }, IntPtr.Zero);
    return o;
  }
}
"@

$pid0 = (Get-Process localless -ErrorAction Stop | Select-Object -First 1).Id

# The full-work-area window is the pill: the only one that is visible and wider
# than 1000px. Matching on title/class is not an option -- the wide-char P/Invoke
# marshals as ANSI here and returns a single character.
function Get-Pill {
  foreach ($h in [P]::ForPid([uint32]$pid0)) {
    $r = New-Object P+RECT
    [void][P]::GetWindowRect($h, [ref]$r)
    if ([P]::IsWindowVisible($h) -and ($r.right - $r.left) -gt 1000) { return $h }
  }
  return [IntPtr]::Zero
}

function Show($tag) {
  $h = Get-Pill
  if ($h -eq [IntPtr]::Zero) { Write-Host "$tag : no pill window"; return }
  $ex = [P]::GetWindowLongW($h, -20)
  $t = if ($ex -band 0x20) { 'click-through OK' } else { '*** EATS CLICKS ***' }
  Write-Host ("$tag : exstyle=0x{0:X} {1}" -f $ex, $t)
}

$RIGHT_ALT = 0xA5
function Tap { [P]::keybd_event($RIGHT_ALT, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 60; [P]::keybd_event($RIGHT_ALT, 0, 2, [UIntPtr]::Zero) }

Show 'idle      '
Tap
Start-Sleep -Milliseconds 700
foreach ($i in 1..6) { Show ("recording $i"); Start-Sleep -Milliseconds 500 }
Tap
Start-Sleep -Seconds 2
Show 'after tap '
Start-Sleep -Seconds 6
Show 'settled   '

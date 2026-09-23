# Catch ANY window that appears on screen, system-wide, at the moment the result
# pill's X is clicked.
#
# Why system-wide: probe-xclick.ps1 filters by localless.exe's pid and saw
# nothing -- no caption, no new visible window. But the things that can paint a
# Windows-looking strip over the pill are not necessarily owned by our process:
# a WebView2 native tooltip belongs to msedgewebview2.exe, and shell/IME popups
# belong to someone else again. Filtering by pid is exactly how that class of
# cause stays invisible.
#
# Why the loop is in C#: a one-frame flash needs a sample interval measured in
# single-digit milliseconds, and enumerating ~400 top-level windows per sample
# from PowerShell is far slower than that. Everything runs inside Watch() and
# only the diffs come back.
#
# ASCII only: Windows PowerShell reads .ps1 as ANSI.
param([int]$Seconds = 26, [switch]$NoClick)
$ErrorActionPreference = 'Stop'

Add-Type @"
using System;
using System.Text;
using System.Diagnostics;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class AW {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern int GetWindowRgnBox(IntPtr h, out RECT r);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern IntPtr GetWindowLongPtrW(IntPtr h, int i);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint x, uint y, uint d, IntPtr e);
  public struct RECT { public int left, top, right, bottom; }

  static string Cls(IntPtr h) { var b = new StringBuilder(128); GetClassNameW(h, b, 128); return b.ToString(); }
  static string Txt(IntPtr h) { var b = new StringBuilder(128); GetWindowTextW(h, b, 128); return b.ToString(); }

  // One screen-visible window, described the way the question needs it: is it
  // on screen, where, how big, and does it carry a frame that would draw a bar.
  static string Line(IntPtr h) {
    RECT r; GetWindowRect(h, out r);
    long st = (long)GetWindowLongPtrW(h, -16);
    uint pid; GetWindowThreadProcessId(h, out pid);
    string proc;
    try { proc = Process.GetProcessById((int)pid).ProcessName; } catch { proc = "?"; }
    string cap = ((st & 0x00C00000L) != 0) ? " CAPTION" : "";
    return string.Format("{0} [{1}] '{2}' at={3},{4} {5}x{6}{7}",
      Cls(h), proc, Txt(h), r.left, r.top, r.right-r.left, r.bottom-r.top, cap);
  }

  public static List<string> Watch(uint pillPid, int ms, bool doClick) {
    var log = new List<string>();
    var prev = new Dictionary<IntPtr,string>();
    bool first = true, clicked = false;
    var sw = Stopwatch.StartNew();
    while (sw.ElapsedMilliseconds < ms) {
      var now = new Dictionary<IntPtr,string>();
      EnumWindows((h,l) => {
        if (!IsWindowVisible(h)) return true;
        RECT r; GetWindowRect(h, out r);
        if (r.right-r.left <= 0 || r.bottom-r.top <= 0) return true;
        now[h] = Line(h);
        return true;
      }, IntPtr.Zero);

      if (!first) {
        foreach (var kv in now)
          if (!prev.ContainsKey(kv.Key))
            log.Add(string.Format("{0}  APPEAR 0x{1:X}  {2}", DateTime.Now.ToString("HH:mm:ss.fff"), (long)kv.Key, kv.Value));
        foreach (var kv in prev)
          if (!now.ContainsKey(kv.Key))
            log.Add(string.Format("{0}  VANISH 0x{1:X}  {2}", DateTime.Now.ToString("HH:mm:ss.fff"), (long)kv.Key, kv.Value));
      }
      prev = now; first = false;

      // Click the X once the pill goes wide -- that is the result state, the
      // one the report is about. Offsets come from pill.html's layout; the
      // window is sized 1:1 with CSS px (h 92 == 28*2 + 47*.75).
      if (doClick && !clicked) {
        foreach (var kv in now) {
          uint p; GetWindowThreadProcessId(kv.Key, out p);
          if (p != pillPid) continue;
          RECT g; if (GetWindowRgnBox(kv.Key, out g) != 2) continue;
          if (g.right-g.left <= 300) continue;
          RECT r; GetWindowRect(kv.Key, out r);
          log.Add(string.Format("{0}  CLICK  at {1},{2}", DateTime.Now.ToString("HH:mm:ss.fff"), r.left+43, r.top+46));
          SetCursorPos(r.left+43, r.top+46);
          mouse_event(0x0002, 0, 0, 0, IntPtr.Zero);
          mouse_event(0x0004, 0, 0, 0, IntPtr.Zero);
          clicked = true;
          break;
        }
      }
    }
    return log;
  }
}
"@

$p = @(Get-Process localless -ErrorAction SilentlyContinue)[0]
if (-not $p) { Write-Host 'localless not running'; exit 1 }
Write-Host "system-wide window watch for ${Seconds}s (pill pid $($p.Id))"
[AW]::Watch([uint32]$p.Id, $Seconds * 1000, -not $NoClick) | ForEach-Object { Write-Host $_ }
Write-Host 'done'

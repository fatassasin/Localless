# Watch the whole desktop for whatever paints the "Localless" strip.
#
# What every earlier probe missed
# -------------------------------
# probe-xclick   : top-level windows of localless.exe only, and only its own pid.
# probe-anywin   : system-wide, but keyed on APPEAR/VANISH -- a window that is
#                  already visible and merely MOVES or RESIZES produced no line.
# probe-tree     : full parent/child tree, but a single snapshot, not a watch.
#
# The strip has now been filmed twice and is definitely on screen, while all
# three probes reported nothing. The union of their blind spots is exactly:
# "an already-visible window belonging to someone else that moves into place".
# An IME composition window (class IME / MSCTFIME UI, one per thread, created
# once and reused) behaves precisely like that, and so does a tooltip that is
# shown by SetWindowPos rather than being created on demand.
#
# So this one records CHANGE, not just existence: any visible top-level window
# anywhere on the desktop whose class, title, style, exstyle, rect or region
# differs from the previous sample gets a line. Plus the full child tree of
# localless.exe, because WebView2 keeps its widgets there.
#
# The loop lives in C# so the sample interval stays in the low tens of ms; a
# strip that fades in over ~300ms cannot hide from that.
#
# ASCII only: Windows PowerShell reads .ps1 as ANSI.
param([int]$Seconds = 90, [string]$Out = 'graphify-out\strip-watch.txt')
$ErrorActionPreference = 'Stop'

Add-Type @"
using System;
using System.Text;
using System.Diagnostics;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class SW {
  // 200% display: without this every rect below is a virtualized lie and will
  // not line up with the pill's real coordinates.
  [DllImport("user32.dll")] static extern bool SetProcessDpiAwarenessContext(IntPtr c);
  [DllImport("user32.dll")] static extern bool SetProcessDPIAware();
  public static void GoDpiAware() { if (!SetProcessDpiAwarenessContext(new IntPtr(-4))) SetProcessDPIAware(); }

  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr p, EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern int GetWindowRgnBox(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern IntPtr GetWindowLongPtrW(IntPtr h, int i);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  public struct RECT { public int left, top, right, bottom; }

  static string Line(IntPtr h) {
    var c = new StringBuilder(96); GetClassNameW(h, c, 96);
    var t = new StringBuilder(160); GetWindowTextW(h, t, 160);
    RECT r; GetWindowRect(h, out r);
    RECT g; int rc = GetWindowRgnBox(h, out g);
    string rgn = rc == 1 ? "EMPTY" : rc == 0 ? "NONE" : (g.right-g.left) + "x" + (g.bottom-g.top);
    long st = (long)GetWindowLongPtrW(h, -16);
    uint pid; GetWindowThreadProcessId(h, out pid);
    string proc; try { proc = Process.GetProcessById((int)pid).ProcessName; } catch { proc = "?"; }
    string cap = ((st & 0x00C00000L) == 0x00C00000L) ? " CAPTION" : "";
    return string.Format("{0} [{1}] '{2}' vis={3} style=0x{4:X8}{5} ex=0x{6:X8} at={7},{8} {9}x{10} rgn={11}",
      c, proc, t, IsWindowVisible(h) ? 1 : 0, st, cap, (long)GetWindowLongPtrW(h, -20),
      r.left, r.top, r.right-r.left, r.bottom-r.top, rgn);
  }

  public static List<string> Watch(uint ourPid, int ms) {
    var log = new List<string>();
    var prev = new Dictionary<IntPtr,string>();
    bool first = true;
    var sw = Stopwatch.StartNew();
    while (sw.ElapsedMilliseconds < ms) {
      var now = new Dictionary<IntPtr,string>();
      EnumWindows((h,l) => {
        uint p; GetWindowThreadProcessId(h, out p);
        bool ours = (p == ourPid);
        if (IsWindowVisible(h)) {
          RECT r; GetWindowRect(h, out r);
          // Zero-size windows are message-only sinks; they cannot paint a strip.
          if (r.right-r.left > 0 && r.bottom-r.top > 0) now[h] = Line(h);
        }
        // Our own children get watched whether visible or not: the strip's
        // width never matched the pill window, so a hidden child coming up is
        // still a live hypothesis.
        if (ours) EnumChildWindows(h, (c,l2) => { now[c] = "  child " + Line(c); return true; }, IntPtr.Zero);
        return true;
      }, IntPtr.Zero);

      if (!first) {
        foreach (var kv in now) {
          string was;
          if (!prev.TryGetValue(kv.Key, out was))
            log.Add(string.Format("{0} NEW    0x{1:X} {2}", DateTime.Now.ToString("HH:mm:ss.fff"), (long)kv.Key, kv.Value));
          else if (was != kv.Value)
            log.Add(string.Format("{0} CHANGE 0x{1:X} {2}", DateTime.Now.ToString("HH:mm:ss.fff"), (long)kv.Key, kv.Value));
        }
        foreach (var kv in prev)
          if (!now.ContainsKey(kv.Key))
            log.Add(string.Format("{0} GONE   0x{1:X} {2}", DateTime.Now.ToString("HH:mm:ss.fff"), (long)kv.Key, kv.Value));
      }
      prev = now; first = false;
    }
    return log;
  }
}
"@

[SW]::GoDpiAware()
$p = @(Get-Process localless -ErrorAction SilentlyContinue)[0]
if (-not $p) { Write-Host 'localless not running'; exit 1 }
New-Item -ItemType Directory -Force -Path (Split-Path $Out) | Out-Null
Write-Host "watching the whole desktop for ${Seconds}s -- reproduce now"
$lines = [SW]::Watch([uint32]$p.Id, $Seconds * 1000)
$lines | Set-Content -Path $Out -Encoding UTF8
Write-Host "$($lines.Count) change lines -> $Out"

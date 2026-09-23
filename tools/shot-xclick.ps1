# Film the pill at ~10ms per frame across a click on the result pill's X.
#
# Why pixels and not window state: three state probes now agree that nothing
# explains the reported strip. The pill's GWL_STYLE never gains WS_CAPTION, no
# window in localless.exe grows a frame, and a system-wide watch saw no window
# appear or vanish at all. Either the flash is not a window, or it is the pill's
# own WebView2 painting something for one frame. Both are settled by looking.
#
# The capture starts before the click and runs past it, so the frames bracket
# the event; frame files are numbered so the flash can be found by eye and the
# frame index converted back to a time offset.
#
# ASCII only: Windows PowerShell reads .ps1 as ANSI.
param([int]$Frames = 120, [int]$ClickAtFrame = 60, [string]$OutDir = 'graphify-out\xflash')
$ErrorActionPreference = 'Stop'

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Drawing;
using System.Diagnostics;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class Shot {
  // Must run before anything reads a coordinate. This display is at 200%, and a
  // DPI-unaware process gets two different lies at once: GetWindowRect hands
  // back virtualized coords while CopyFromScreen blits physical ones. The first
  // run of this script filmed a completely different part of the desktop and
  // reported diff=0 on all 90 frames, which reads exactly like "nothing
  // happened" -- the most expensive kind of wrong answer.
  [DllImport("user32.dll")] static extern bool SetProcessDpiAwarenessContext(IntPtr c);
  [DllImport("user32.dll")] static extern bool SetProcessDPIAware();
  public static void GoDpiAware() {
    // -4 == PER_MONITOR_AWARE_V2; fall back for older builds.
    if (!SetProcessDpiAwarenessContext(new IntPtr(-4))) SetProcessDPIAware();
  }

  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern int GetWindowRgnBox(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern IntPtr GetWindowLongPtrW(IntPtr h, int i);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint x, uint y, uint d, IntPtr e);
  public struct RECT { public int left, top, right, bottom; }

  // Style/rect/region on one line, cheap enough to sample every frame.
  static string WinLine(IntPtr h) {
    RECT r; GetWindowRect(h, out r);
    RECT g; int rc = GetWindowRgnBox(h, out g);
    string rgn = rc == 1 ? "EMPTY" : rc == 0 ? "NONE" : (g.right-g.left) + "x" + (g.bottom-g.top);
    return string.Format("style=0x{0:X8} ex=0x{1:X8} at={2},{3} {4}x{5} rgn={6}",
      (long)GetWindowLongPtrW(h, -16), (long)GetWindowLongPtrW(h, -20),
      r.left, r.top, r.right-r.left, r.bottom-r.top, rgn);
  }

  // The pill window in its result state: our process, non-empty region, wide.
  public static IntPtr WaitWide(uint pid, int timeoutMs) {
    var sw = Stopwatch.StartNew();
    while (sw.ElapsedMilliseconds < timeoutMs) {
      IntPtr found = IntPtr.Zero;
      EnumWindows((h,l) => {
        uint p; GetWindowThreadProcessId(h, out p);
        if (p != pid) return true;
        RECT g; if (GetWindowRgnBox(h, out g) != 2) return true;
        if (g.right-g.left <= 300) return true;
        found = h; return false;
      }, IntPtr.Zero);
      if (found != IntPtr.Zero) return found;
    }
    return IntPtr.Zero;
  }

  public static List<string> Film(IntPtr pill, int frames, int clickAt, string dir) {
    var log = new List<string>();
    RECT r; GetWindowRect(pill, out r);
    // The X sits at the head of the pill's flex row. pill.html puts it at
    // (--ll-pad 28 + (7 + 13) * .75, 28 + 23.5 * .75) in CSS px, and the window
    // is 28*2 + 47*.75 == 91.25 CSS px tall, so one ratio converts both.
    double k = (r.bottom - r.top) / 91.25;
    int cx = r.left + (int)(43 * k), cy = r.top + (int)(46 * k);
    // Pad generously: whatever is flashing may be bigger than the pill window.
    int x = r.left - 80, y = r.top - 80;
    int w = (r.right-r.left) + 160, h = (r.bottom-r.top) + 160;
    log.Add(string.Format("pill at={0},{1} {2}x{3}; k={4:0.00}; capturing {5},{6} {7}x{8}",
      r.left, r.top, r.right-r.left, r.bottom-r.top, k, x, y, w, h));

    var shots = new List<Bitmap>();
    var stamps = new List<string>();
    var state = new List<string>();
    var sw = Stopwatch.StartNew();
    for (int i = 0; i < frames; i++) {
      // Park the cursor on the X first and only press much later. The report is
      // "sometimes, when I click the X" -- and the one mechanism that behaves
      // that way is dwell-triggered: it needs the pointer to sit still on the
      // button for a few hundred ms before the press. Clicking the instant the
      // cursor arrives is a different gesture and would test the wrong thing.
      // Moving early also wakes a dimmed display, whose first input event would
      // otherwise be swallowed instead of reaching the button.
      if (i == 0) {
        SetCursorPos(cx, cy);
        log.Add(string.Format("frame 0: HOVER at {0},{1}", cx, cy));
      }
      if (i == clickAt) {
        mouse_event(0x0002, 0, 0, 0, IntPtr.Zero);
        mouse_event(0x0004, 0, 0, 0, IntPtr.Zero);
        log.Add(string.Format("frame {0}: CLICK", i));
      }
      var bmp = new Bitmap(w, h);
      using (var g = Graphics.FromImage(bmp)) g.CopyFromScreen(x, y, 0, 0, new Size(w, h));
      shots.Add(bmp);
      stamps.Add(sw.Elapsed.TotalMilliseconds.ToString("0.0"));
      // Per-frame window state, recorded next to the pixels. The grey
      // "Localless" bar showed up in the film while three separate state probes
      // reported nothing, so neither kind of evidence can identify the culprit
      // on its own -- they have to share a clock.
      state.Add(string.Format("{0} f{1:D3} {2}", DateTime.Now.ToString("HH:mm:ss.fff"), i, WinLine(pill)));
      while (sw.Elapsed.TotalMilliseconds < (i+1) * 10.0) { }
    }
    // Write after filming: disk IO inside the loop would stretch the interval.
    System.IO.File.WriteAllLines(dir + "\\state.txt", state);
    for (int i = 0; i < shots.Count; i++) {
      shots[i].Save(string.Format("{0}\\f{1:D3}_{2}ms.png", dir, i, stamps[i]),
        System.Drawing.Imaging.ImageFormat.Png);
      shots[i].Dispose();
    }
    log.Add(string.Format("wrote {0} frames to {1}", shots.Count, dir));
    return log;
  }
}
"@ -ReferencedAssemblies System.Drawing

[Shot]::GoDpiAware()

$p = @(Get-Process localless -ErrorAction SilentlyContinue)[0]
if (-not $p) { Write-Host 'localless not running'; exit 1 }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
Get-ChildItem $OutDir -Filter *.png -ErrorAction SilentlyContinue | Remove-Item -Force

Write-Host 'waiting for the result pill (wide, non-empty region)...'
$pill = [Shot]::WaitWide([uint32]$p.Id, 40000)
if ($pill -eq [IntPtr]::Zero) { Write-Host 'result pill never appeared within 40s'; exit 1 }
[Shot]::Film($pill, $Frames, $ClickAtFrame, (Resolve-Path $OutDir).Path) | ForEach-Object { Write-Host $_ }

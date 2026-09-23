# Prove the settings window is never on screen while it carries WS_CAPTION.
#
# The fix in settingswin::show_when_ready does not stop tao from putting
# WS_CAPTION back -- it cannot; tao rewrites the whole GWL_STYLE from its own
# cached WindowFlags inside set_visible. What it does is make sure the window
# occupies zero pixels for as long as the caption bits are on: an empty
# SetWindowRgn goes on before show() and comes off after keep_frameless().
#
# So the check is not "does the style ever show CAPTION" (it still will) but
# "is there any sample where CAPTION is on AND the window is visible AND the
# region is not empty". One such sample is a frame the user can see. Zero of
# them closes it without needing to eyeball pixels.
#
# Locks on by title before the window is shown -- it is built with
# visible(false), so the HWND exists long before the dangerous moment. A probe
# that waits for IsWindowVisible would start sampling after the flash.
#
# ASCII only: Windows PowerShell reads .ps1 as ANSI.
param([int]$Seconds = 25, [string]$Out = 'graphify-out\settings-open.txt')
$ErrorActionPreference = 'Stop'

Add-Type @"
using System;
using System.Text;
using System.Diagnostics;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class SO {
  [DllImport("user32.dll")] static extern bool SetProcessDpiAwarenessContext(IntPtr c);
  [DllImport("user32.dll")] static extern bool SetProcessDPIAware();
  public static void GoDpiAware() { if (!SetProcessDpiAwarenessContext(new IntPtr(-4))) SetProcessDPIAware(); }

  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern IntPtr GetWindowLongPtrW(IntPtr h, int i);
  [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern int GetWindowRgnBox(IntPtr h, out RECT r);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  public struct RECT { public int left, top, right, bottom; }

  // Poll for the HWND itself, visible or not.
  public static IntPtr WaitFor(string title, int timeoutMs) {
    var sw = Stopwatch.StartNew();
    while (sw.ElapsedMilliseconds < timeoutMs) {
      IntPtr found = IntPtr.Zero;
      EnumWindows((h,l) => {
        var t = new StringBuilder(160); GetWindowTextW(h, t, 160);
        if (t.ToString() == title) { found = h; return false; }
        return true;
      }, IntPtr.Zero);
      if (found != IntPtr.Zero) return found;
      System.Threading.Thread.Sleep(5);
    }
    return IntPtr.Zero;
  }

  public static List<string> Watch(IntPtr h, int ms, out int exposed) {
    var log = new List<string>();
    exposed = 0;
    string prev = null;
    var sw = Stopwatch.StartNew();
    while (sw.ElapsedMilliseconds < ms) {
      if (!IsWindow(h)) { log.Add(DateTime.Now.ToString("HH:mm:ss.fff") + " WINDOW GONE"); break; }
      long st = (long)GetWindowLongPtrW(h, -16);
      RECT g; int rc = GetWindowRgnBox(h, out g);
      string rgn = rc == 1 ? "EMPTY" : rc == 0 ? "NONE" : (g.right-g.left) + "x" + (g.bottom-g.top);
      bool vis = IsWindowVisible(h);
      bool cap = (st & 0x00C00000L) == 0x00C00000L;
      // The one thing being measured. NONE means "no region at all", i.e. the
      // whole window is on screen -- that is the exposed case, not EMPTY.
      bool bad = cap && vis && rgn != "EMPTY";
      if (bad) exposed++;
      RECT r; GetWindowRect(h, out r);
      string cur = string.Format("style=0x{0:X8}{1} vis={2} rgn={3} at={4},{5} {6}x{7}{8}",
        st, cap ? " CAPTION" : "", vis ? 1 : 0, rgn,
        r.left, r.top, r.right-r.left, r.bottom-r.top,
        bad ? "   <<< EXPOSED CAPTION" : "");
      if (cur != prev) { log.Add(DateTime.Now.ToString("HH:mm:ss.fff") + " " + cur); prev = cur; }
      System.Threading.Thread.Sleep(2);
    }
    return log;
  }
}
"@

[SO]::GoDpiAware()
New-Item -ItemType Directory -Force -Path (Split-Path $Out) | Out-Null
Write-Host 'waiting for the settings HWND to be created...'
$h = [SO]::WaitFor('Localless 设置', 40000)
if ($h -eq [IntPtr]::Zero) { Write-Host 'settings window never got created within 40s'; exit 1 }
Write-Host ("locked on 0x{0:X} before it was shown" -f [int64]$h)
$exposed = 0
$lines = [SO]::Watch($h, $Seconds * 1000, [ref]$exposed)
$lines | Set-Content -Path $Out -Encoding UTF8
$lines | ForEach-Object { Write-Host $_ }
Write-Host ''
Write-Host "samples with a visible, unclipped caption: $exposed"
if ($exposed -eq 0) { Write-Host 'PASS -- the caption was never on screen' }
else { Write-Host 'FAIL -- the caption reached the screen' }

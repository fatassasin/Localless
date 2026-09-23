# Watch the settings window's GWL_STYLE at ~2ms while the user hovers and clicks it.
#
# What the desktop-wide watch (watch-strip.ps1) already settled
# -------------------------------------------------------------
# The pill never carries WS_CAPTION -- it is 0x94000000 for its whole life and
# shows/hides by SetWindowRgn, which never goes through tao's set_visible. The
# only localless window ever measured with a real caption is the settings
# window, and only for one sample right after show():
#
#   NEW    'Localless 设置' style=0x14CF0000 CAPTION   (02:34:18.089)
#   CHANGE 'Localless 设置' style=0x94040000           (02:34:18.255)
#
# settingswin.rs show_when_ready() does show() -> set_focus() -> keep_frameless(),
# so that is tao restoring its cached WindowFlags on set_visible and our code
# stripping them back a moment later.
#
# What this probe decides
# -----------------------
# Whether that was a one-shot flash at open, or whether tao keeps writing the
# caption bits back afterwards. The second reading is the one that would also
# explain "the top bar jumps whenever I hover or click after opening Localless"
# -- and the two reports would then be a single bug, not two.
#
# watch-strip.ps1 cannot answer this: enumerating every window on the desktop
# costs ~100ms per sample, so a caption that comes back for one frame between
# samples leaves no line. This locks onto one HWND and does nothing else, which
# buys a ~2ms interval.
#
# Foreground/active are logged next to the style: if the caption bits return
# exactly when activation moves, the trigger is WM_NCACTIVATE, not a timer.
#
# ASCII only: Windows PowerShell reads .ps1 as ANSI.
param([int]$Seconds = 60, [string]$Out = 'graphify-out\settings-style.txt')
$ErrorActionPreference = 'Stop'

Add-Type @"
using System;
using System.Text;
using System.Diagnostics;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class SS {
  [DllImport("user32.dll")] static extern bool SetProcessDpiAwarenessContext(IntPtr c);
  [DllImport("user32.dll")] static extern bool SetProcessDPIAware();
  public static void GoDpiAware() { if (!SetProcessDpiAwarenessContext(new IntPtr(-4))) SetProcessDPIAware(); }

  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern IntPtr GetWindowLongPtrW(IntPtr h, int i);
  [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
  [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  public struct RECT { public int left, top, right, bottom; }
  public struct POINT { public int x, y; }

  public static IntPtr WaitFor(string title, int timeoutMs) {
    var sw = Stopwatch.StartNew();
    while (sw.ElapsedMilliseconds < timeoutMs) {
      IntPtr found = IntPtr.Zero;
      EnumWindows((h,l) => {
        var t = new StringBuilder(160); GetWindowTextW(h, t, 160);
        if (t.ToString() == title && IsWindowVisible(h)) { found = h; return false; }
        return true;
      }, IntPtr.Zero);
      if (found != IntPtr.Zero) return found;
      System.Threading.Thread.Sleep(15);
    }
    return IntPtr.Zero;
  }

  // Only the bits that can put a bar on screen, spelled out -- reading
  // 0x94040000 against 0x14CF0000 by eye is how a flip gets missed.
  static string Bits(long s) {
    var f = new List<string>();
    if ((s & 0x00C00000L) == 0x00C00000L) f.Add("CAPTION");
    else if ((s & 0x00800000L) != 0) f.Add("BORDER");
    else if ((s & 0x00400000L) != 0) f.Add("DLGFRAME");
    if ((s & 0x00040000L) != 0) f.Add("THICKFRAME");
    if ((s & 0x00080000L) != 0) f.Add("SYSMENU");
    if ((s & 0x00020000L) != 0) f.Add("MINBOX");
    if ((s & 0x00010000L) != 0) f.Add("MAXBOX");
    if ((s & 0x80000000L) == 0) f.Add("no-POPUP");
    return f.Count == 0 ? "clean" : string.Join("+", f);
  }

  public static List<string> Watch(IntPtr h, int ms) {
    var log = new List<string>();
    string prev = null;
    var sw = Stopwatch.StartNew();
    while (sw.ElapsedMilliseconds < ms) {
      if (!IsWindow(h)) { log.Add(DateTime.Now.ToString("HH:mm:ss.fff") + " WINDOW GONE"); break; }
      long st = (long)GetWindowLongPtrW(h, -16);
      RECT r; GetWindowRect(h, out r);
      POINT p; GetCursorPos(out p);
      IntPtr under = WindowFromPoint(p);
      bool over = (p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom);
      string cur = string.Format("style=0x{0:X8} {1} at={2},{3} {4}x{5} fg={6} cursorOver={7} underCursor=0x{8:X}",
        st, Bits(st), r.left, r.top, r.right-r.left, r.bottom-r.top,
        GetForegroundWindow() == h ? "self" : "other", over ? 1 : 0, (long)under);
      if (cur != prev) {
        log.Add(DateTime.Now.ToString("HH:mm:ss.fff") + " " + cur);
        prev = cur;
      }
      System.Threading.Thread.Sleep(2);
    }
    return log;
  }
}
"@

[SS]::GoDpiAware()
New-Item -ItemType Directory -Force -Path (Split-Path $Out) | Out-Null
Write-Host 'waiting for the settings window (open it from the tray)...'
$h = [SS]::WaitFor('Localless 设置', 60000)
if ($h -eq [IntPtr]::Zero) { Write-Host 'settings window never appeared within 60s'; exit 1 }
Write-Host ("locked on 0x{0:X} -- now hover and click it for ${Seconds}s" -f [int64]$h)
$lines = [SS]::Watch($h, $Seconds * 1000)
$lines | Set-Content -Path $Out -Encoding UTF8
Write-Host "$($lines.Count) style changes -> $Out"
$lines | ForEach-Object { Write-Host $_ }

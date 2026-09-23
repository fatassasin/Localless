# Reproduce "open settings for the first time, then click the middle of the
# recording pill -> a caption strip flashes", and film the thinking text at
# the same time to see whether its ripple animates.
#
# Meant to run against an instance started with LOCALLESS_SETTINGS=1 and
# LOCALLESS_PILL_DEMO=1: the settings window opens by itself and the pill walks
# loading -> rec -> refine -> result -> learned -> done on a fixed clock.
#
# Two areas are filmed on one clock: a fixed box around the pill's home at the
# bottom centre of the work area, and the top strip of the settings window.
# Frames are change-detected (a frame identical to the previous one is dropped)
# and PNG-encoded on a background thread, so a 25s run stays small and the
# capture interval is not stretched by disk IO.
#
# Per-sample window state goes to state.txt next to the frames: pill and
# settings GWL_STYLE, pill region, foreground window. Same clock as the frames.
#
# ASCII only: Windows PowerShell reads .ps1 as ANSI.
param([int]$Seconds = 25, [int]$ClickAfterMs = 3200, [string]$OutDir = 'graphify-out\firstclick')
$ErrorActionPreference = 'Stop'

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Text;
using System.Drawing;
using System.Drawing.Imaging;
using System.Diagnostics;
using System.Collections.Generic;
using System.Collections.Concurrent;
using System.Threading;
using System.Runtime.InteropServices;
public class FC {
  [DllImport("user32.dll")] static extern bool SetProcessDpiAwarenessContext(IntPtr c);
  [DllImport("user32.dll")] static extern bool SetProcessDPIAware();
  public static void GoDpiAware() { if (!SetProcessDpiAwarenessContext(new IntPtr(-4))) SetProcessDPIAware(); }

  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] static extern int GetWindowRgnBox(IntPtr h, out RECT r);
  [DllImport("user32.dll")] static extern IntPtr GetWindowLongPtrW(IntPtr h, int i);
  [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] static extern void mouse_event(uint f, uint x, uint y, uint d, IntPtr e);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern bool SystemParametersInfoW(uint a, uint b, ref RECT r, uint c);
  public struct RECT { public int left, top, right, bottom; }

  static string Title(IntPtr h) { var t = new StringBuilder(160); GetWindowTextW(h, t, 160); return t.ToString(); }

  public static IntPtr Find(uint pid, string title, bool mustBeVisible, int timeoutMs) {
    var sw = Stopwatch.StartNew();
    while (sw.ElapsedMilliseconds < timeoutMs) {
      IntPtr found = IntPtr.Zero;
      EnumWindows((h,l) => {
        uint p; GetWindowThreadProcessId(h, out p);
        if (p != pid || Title(h) != title) return true;
        if (mustBeVisible && !IsWindowVisible(h)) return true;
        found = h; return false;
      }, IntPtr.Zero);
      if (found != IntPtr.Zero) return found;
      Thread.Sleep(10);
    }
    return IntPtr.Zero;
  }

  static string Rgn(IntPtr h) {
    RECT g; int rc = GetWindowRgnBox(h, out g);
    return rc == 1 ? "EMPTY" : rc == 0 ? "NONE" : (g.right-g.left) + "x" + (g.bottom-g.top);
  }

  // Cheap equality on raw bytes: identical frames are dropped before encoding.
  static byte[] Bytes(Bitmap b) {
    var d = b.LockBits(new Rectangle(0, 0, b.Width, b.Height), ImageLockMode.ReadOnly, PixelFormat.Format32bppArgb);
    var a = new byte[d.Stride * d.Height];
    Marshal.Copy(d.Scan0, a, 0, a.Length);
    b.UnlockBits(d);
    return a;
  }
  static bool Same(byte[] x, byte[] y) {
    if (x == null || y == null || x.Length != y.Length) return false;
    for (int i = 0; i < x.Length; i++) if (x[i] != y[i]) return false;
    return true;
  }

  public static List<string> Run(uint pid, IntPtr pill, IntPtr settings, int ms, int clickAfter, string dir) {
    var log = new List<string>();
    RECT wa = new RECT(); SystemParametersInfoW(0x0030, 0, ref wa, 0);
    // Pill lives at the bottom centre of the work area. A fixed box, because the
    // pill window resizes on every state change and a box that follows it would
    // make every frame "different".
    int pw = 1100, ph = 420;
    int px = (wa.left + wa.right) / 2 - pw / 2, py = wa.bottom - ph;
    RECT sr; GetWindowRect(settings, out sr);
    int sx = sr.left - 40, sy = sr.top - 120, sw_ = (sr.right - sr.left) + 80, sh = 280;
    log.Add(string.Format("pill box {0},{1} {2}x{3}; settings box {4},{5} {6}x{7}", px, py, pw, ph, sx, sy, sw_, sh));

    var q = new BlockingCollection<Tuple<Bitmap,string>>(64);
    var writer = new Thread(() => {
      foreach (var t in q.GetConsumingEnumerable()) { t.Item1.Save(t.Item2, ImageFormat.Png); t.Item1.Dispose(); }
    });
    writer.Start();

    var state = new List<string>();
    byte[] lastP = null, lastS = null;
    string prevState = null;
    bool clicked = false;
    int n = 0, keptP = 0, keptS = 0;
    var clock = Stopwatch.StartNew();
    while (clock.ElapsedMilliseconds < ms) {
      double t = clock.Elapsed.TotalMilliseconds;
      if (!clicked && t >= clickAfter) {
        // Middle of the pill, not a button: that is the reported gesture.
        RECT r; GetWindowRect(pill, out r);
        int cx = (r.left + r.right) / 2, cy = (r.top + r.bottom) / 2;
        SetCursorPos(cx, cy);
        Thread.Sleep(15);
        mouse_event(0x0002, 0, 0, 0, IntPtr.Zero);
        mouse_event(0x0004, 0, 0, 0, IntPtr.Zero);
        clicked = true;
        state.Add(string.Format("{0,8:0.0} CLICK at {1},{2} (pill rgn={3})", t, cx, cy, Rgn(pill)));
      }
      var bp = new Bitmap(pw, ph, PixelFormat.Format32bppArgb);
      using (var g = Graphics.FromImage(bp)) g.CopyFromScreen(px, py, 0, 0, new Size(pw, ph));
      var bs = new Bitmap(sw_, sh, PixelFormat.Format32bppArgb);
      using (var g = Graphics.FromImage(bs)) g.CopyFromScreen(sx, sy, 0, 0, new Size(sw_, sh));

      IntPtr fg = GetForegroundWindow();
      string fgs = fg == pill ? "pill" : fg == settings ? "settings" : ("0x" + ((long)fg).ToString("X") + " '" + Title(fg) + "'");
      string cur = string.Format("pill style=0x{0:X8} ex=0x{1:X8} rgn={2} | set style=0x{3:X8} | fg={4}",
        (long)GetWindowLongPtrW(pill, -16), (long)GetWindowLongPtrW(pill, -20), Rgn(pill),
        (long)GetWindowLongPtrW(settings, -16), fgs);
      if (cur != prevState) { state.Add(string.Format("{0,8:0.0} {1}", t, cur)); prevState = cur; }

      var ap = Bytes(bp);
      if (!Same(ap, lastP)) { q.Add(Tuple.Create(bp, string.Format("{0}\\p{1:D4}_{2:00000}ms.png", dir, n, t))); keptP++; } else bp.Dispose();
      lastP = ap;
      var as_ = Bytes(bs);
      if (!Same(as_, lastS)) { q.Add(Tuple.Create(bs, string.Format("{0}\\s{1:D4}_{2:00000}ms.png", dir, n, t))); keptS++; } else bs.Dispose();
      lastS = as_;
      n++;
      while (clock.Elapsed.TotalMilliseconds < t + 25) { }
    }
    q.CompleteAdding();
    writer.Join();
    System.IO.File.WriteAllLines(dir + "\\state.txt", state);
    log.Add(string.Format("{0} samples, kept {1} pill frames and {2} settings frames", n, keptP, keptS));
    return log;
  }
}
"@ -ReferencedAssemblies System.Drawing

[FC]::GoDpiAware()
$p = @(Get-Process localless -ErrorAction SilentlyContinue)[0]
if (-not $p) { Write-Host 'localless not running'; exit 1 }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
Get-ChildItem $OutDir -File -ErrorAction SilentlyContinue | Remove-Item -Force

$pill = [FC]::Find([uint32]$p.Id, 'Localless', $false, 20000)
if ($pill -eq [IntPtr]::Zero) { Write-Host 'pill window not found'; exit 1 }
$set = [FC]::Find([uint32]$p.Id, 'Localless 设置', $true, 20000)
if ($set -eq [IntPtr]::Zero) { Write-Host 'settings window never became visible'; exit 1 }
Write-Host ("pill 0x{0:X}  settings 0x{1:X}" -f [int64]$pill, [int64]$set)
[FC]::Run([uint32]$p.Id, $pill, $set, $Seconds * 1000, $ClickAfterMs, (Resolve-Path $OutDir).Path) | ForEach-Object { Write-Host $_ }

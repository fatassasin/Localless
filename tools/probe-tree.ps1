# Dump the FULL window tree (top-level AND children) for localless.exe and the
# msedgewebview2.exe processes it owns.
#
# Why this exists: probe-xclick / probe-anywin both walk EnumWindows, which only
# ever returns top-level windows. WebView2 puts its render widgets in CHILD
# windows of the Tauri window, and no_frame() in pill.rs only ever touches the
# top-level HWND. A child carrying WS_CAPTION would paint a caption bar inside
# its parent's client area -- at the very top, which is exactly where the strip
# appears -- while every probe so far reported "nothing changed, no new window".
# That blind spot is consistent with all three null results.
#
# ASCII only: Windows PowerShell reads .ps1 as ANSI.
$ErrorActionPreference = 'Stop'
Add-Type @"
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class WT {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr p, EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern IntPtr GetWindowLongPtrW(IntPtr h, int i);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  public struct RECT { public int left, top, right, bottom; }

  public static List<string> Dump(List<uint> pids) {
    var o = new List<string>();
    EnumWindows((h,l) => {
      uint p; GetWindowThreadProcessId(h, out p);
      if (!pids.Contains(p)) return true;
      Walk(h, 0, o);
      return true;
    }, IntPtr.Zero);
    return o;
  }

  static void Walk(IntPtr h, int depth, List<string> o) {
    var cls = new StringBuilder(128); GetClassNameW(h, cls, 128);
    var txt = new StringBuilder(256); GetWindowTextW(h, txt, 256);
    RECT r; GetWindowRect(h, out r);
    long st = (long)GetWindowLongPtrW(h, -16);
    // WS_CAPTION is WS_BORDER|WS_DLGFRAME; report the halves separately so a
    // partial strip (border only, no dlgframe) is still visible in the dump.
    string flags = "";
    if ((st & 0x00C00000L) == 0x00C00000L) flags += " CAPTION";
    else { if ((st & 0x00800000L) != 0) flags += " BORDER"; if ((st & 0x00400000L) != 0) flags += " DLGFRAME"; }
    if ((st & 0x80000000L) == 0) flags += " no-POPUP";
    o.Add(string.Format("{0}0x{1:X} {2} '{3}' vis={4} style=0x{5:X8}{6} at={7},{8} {9}x{10}",
      new string(' ', depth*2), (long)h, cls, txt, IsWindowVisible(h), st, flags,
      r.left, r.top, r.right-r.left, r.bottom-r.top));
    if (depth > 4) return;
    var kids = new List<IntPtr>();
    EnumChildWindows(h, (c,l) => { kids.Add(c); return true; }, IntPtr.Zero);
    foreach (var c in kids) {
      // EnumChildWindows is recursive already; only walk direct children here.
      IntPtr par = GetParent(c);
      if (par == h) Walk(c, depth+1, o);
    }
  }
  [DllImport("user32.dll")] static extern IntPtr GetParent(IntPtr h);
}
"@

$pids = New-Object 'System.Collections.Generic.List[uint32]'
foreach ($n in 'localless','msedgewebview2') {
  foreach ($p in @(Get-Process $n -ErrorAction SilentlyContinue)) { $pids.Add([uint32]$p.Id) }
}
if ($pids.Count -eq 0) { Write-Host 'nothing running'; exit 1 }
Write-Host "window tree for $($pids.Count) processes"
[WT]::Dump($pids) | ForEach-Object { Write-Host $_ }

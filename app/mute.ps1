param([int]$SelfPid = 0)
# 录音时静音「其它应用」，录完还原。
#
# 以前这里动的是默认播放设备的 IAudioEndpointVolume，也就是系统总音量。
# 名字写的是静音其它声音，做出来却是把整台机器的声音一刀切——音量图标变成
# 打叉的喇叭，还顺手把用户自己调的总音量状态搅进来。现在改成逐个应用：
# IAudioSessionManager2 枚举当前播放设备上的会话，对每个会话的
# ISimpleAudioVolume 单独 SetMute，总音量一根手指都不碰。
#
# 两条自保规则：
#   1) 本来就静音的会话一概跳过，还原时也不碰它——那是用户自己按的静音，
#      不是我们按的，收工时替他打开就是擅自改设置。
#   2) 只还原这一次真正动过的那几个会话，按会话实例 id 记名字。
#
# 常驻模式：这个脚本起一次就一直在，stdin 一行一条命令（1=静音 0=还原 q=退出）。
# 以前是开录 spawn 一个、停录再 spawn 一个，每个都要现编译 Add-Type 约一秒：
# 静音真正落地是在开录一秒之后，短句说完了才静下来，看着就是「停录才静音」；
# 而且那两个进程互不相干，谁先编完谁后应用没有保证，偶尔还会留着静音不还原。
# 现在编译只发生一次，之后每条命令都是毫秒级，且严格按先后顺序执行。

$ErrorActionPreference = 'Stop'

Add-Type -ErrorAction Stop @'
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Runtime.InteropServices;

[Guid("D666063F-1587-4E43-81F1-B948E807363F"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IMMDevice {
  int Activate(ref Guid iid, int ctx, IntPtr p, [MarshalAs(UnmanagedType.IUnknown)] out object o);
}
[Guid("A95664D2-9614-4F35-A746-DE8DB63617E6"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IMMDeviceEnumerator { int _a(); int GetDefaultAudioEndpoint(int flow, int role, out IMMDevice ep); }
[ComImport, Guid("BCDE0395-E52F-467C-8E3D-C4579291692E")] class MMDeviceEnumeratorComObject { }

[Guid("77AA99A0-1BD6-484F-8BC7-2C654C9A9B6F"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IAudioSessionManager2 {
  int _a(); int _b();
  int GetSessionEnumerator(out IAudioSessionEnumerator e);
}
[Guid("E2F5BB11-0570-40CA-ACDD-3AA01277DEE8"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IAudioSessionEnumerator {
  int GetCount(out int c);
  int GetSession(int i, [MarshalAs(UnmanagedType.IUnknown)] out object s);
}
[Guid("BFB7FF88-7239-4FC9-8FA2-07C950BE9C6D"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IAudioSessionControl2 {
  int GetState(out int s);
  int GetDisplayName([MarshalAs(UnmanagedType.LPWStr)] out string n);
  int SetDisplayName([MarshalAs(UnmanagedType.LPWStr)] string n, ref Guid c);
  int GetIconPath([MarshalAs(UnmanagedType.LPWStr)] out string p);
  int SetIconPath([MarshalAs(UnmanagedType.LPWStr)] string p, ref Guid c);
  int GetGroupingParam(out Guid g);
  int SetGroupingParam(ref Guid g, ref Guid c);
  int RegisterAudioSessionNotification(IntPtr n);
  int UnregisterAudioSessionNotification(IntPtr n);
  int GetSessionIdentifier([MarshalAs(UnmanagedType.LPWStr)] out string id);
  int GetSessionInstanceIdentifier([MarshalAs(UnmanagedType.LPWStr)] out string id);
  int GetProcessId(out uint pid);
  int IsSystemSoundsSession();
  int SetDuckingPreference([MarshalAs(UnmanagedType.Bool)] bool opt);
}
[Guid("87CE5498-68D6-44E5-9215-6DA47EF883D8"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface ISimpleAudioVolume {
  int SetMasterVolume(float l, ref Guid c);
  int GetMasterVolume(out float l);
  int SetMute([MarshalAs(UnmanagedType.Bool)] bool m, ref Guid c);
  int GetMute([MarshalAs(UnmanagedType.Bool)] out bool m);
}

public class LlAudio {
  [StructLayout(LayoutKind.Sequential)]
  struct PROCESSENTRY32 {
    public uint dwSize; public uint cntUsage; public uint th32ProcessID;
    public IntPtr th32DefaultHeapID; public uint th32ModuleID; public uint cntThreads;
    public uint th32ParentProcessID; public int pcPriClassBase; public uint dwFlags;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 260)] public string szExeFile;
  }
  [DllImport("kernel32.dll", SetLastError = true)] static extern IntPtr CreateToolhelp32Snapshot(uint flags, uint pid);
  [DllImport("kernel32.dll", SetLastError = true)] static extern bool Process32First(IntPtr h, ref PROCESSENTRY32 e);
  [DllImport("kernel32.dll", SetLastError = true)] static extern bool Process32Next(IntPtr h, ref PROCESSENTRY32 e);
  [DllImport("kernel32.dll", SetLastError = true)] static extern bool CloseHandle(IntPtr h);

  // pid → 父 pid。一次快照建一张表，别在循环里一个个问。
  static Dictionary<uint, uint> Parents() {
    Dictionary<uint, uint> map = new Dictionary<uint, uint>();
    IntPtr snap = CreateToolhelp32Snapshot(2 /*TH32CS_SNAPPROCESS*/, 0);
    if (snap == IntPtr.Zero || snap == new IntPtr(-1)) return map;
    try {
      PROCESSENTRY32 e = new PROCESSENTRY32();
      e.dwSize = (uint)Marshal.SizeOf(typeof(PROCESSENTRY32));
      if (Process32First(snap, ref e)) {
        do { map[e.th32ProcessID] = e.th32ParentProcessID; } while (Process32Next(snap, ref e));
      }
    } finally { CloseHandle(snap); }
    return map;
  }

  static IAudioSessionEnumerator Sessions() {
    IMMDevice dev; object mgr;
    Guid iid = typeof(IAudioSessionManager2).GUID;
    ((IMMDeviceEnumerator)(new MMDeviceEnumeratorComObject())).GetDefaultAudioEndpoint(0, 1, out dev);
    dev.Activate(ref iid, 23, IntPtr.Zero, out mgr);
    IAudioSessionEnumerator e;
    ((IAudioSessionManager2)mgr).GetSessionEnumerator(out e);
    return e;
  }

  // 自家的会话要放过。两版的发声进程都不是主进程本身：Electron 是它的 audio
  // service 子进程，Tauri 是 WebView2 的 msedgewebview2.exe——后者连可执行文件
  // 都在 Edge 运行时目录里，跟我们毫无关系。所以认的是**祖先链**：顺着父 pid
  // 往上走，走到 selfPid 就是自家的。
  //
  // 以前认的是可执行文件路径。那条在 Electron 上能跑，但换到 Tauri 就整个失效
  // （路径永远对不上 → 开录把自己也静音了，回放历史时没声音）；而且它在
  // Electron 上也过宽——机器上别的 Electron 应用共用同一份 electron.exe 时会被
  // 一起放过，该静的音静不掉。
  //
  // 走不动就停（父进程已经退了，或者 pid 被回收成了环）。深度封顶 16 层：
  // pid 回收出现自环时这是唯一的出口。
  static bool IsSelf(uint pid, uint selfPid, Dictionary<uint, uint> parents) {
    if (pid == 0 || selfPid == 0) return false;
    uint cur = pid;
    for (int hop = 0; hop < 16; hop++) {
      if (cur == selfPid) return true;
      uint up;
      if (!parents.TryGetValue(cur, out up) || up == 0 || up == cur) return false;
      cur = up;
    }
    return false;
  }

  // mute=true ：把自家以外、当前没被静音的会话全静音，返回动过的会话 id。
  // mute=false：只把 only 里点名的那几个还原。
  public static string[] Apply(bool mute, string[] only, uint selfPid) {
    List<string> touched = new List<string>();
    HashSet<string> wanted = new HashSet<string>(only ?? new string[0], StringComparer.OrdinalIgnoreCase);
    Dictionary<uint, uint> parents = mute ? Parents() : null;
    IAudioSessionEnumerator en = Sessions();
    int n; en.GetCount(out n);
    for (int i = 0; i < n; i++) {
      object raw;
      if (en.GetSession(i, out raw) != 0 || raw == null) continue;
      IAudioSessionControl2 ctl = raw as IAudioSessionControl2;
      ISimpleAudioVolume vol = raw as ISimpleAudioVolume;
      if (ctl == null || vol == null) continue;
      string id;
      if (ctl.GetSessionInstanceIdentifier(out id) != 0 || id == null) continue;
      Guid ctx = Guid.Empty;
      if (mute) {
        uint pid;
        if (ctl.GetProcessId(out pid) != 0) pid = 0;
        if (IsSelf(pid, selfPid, parents)) continue;
        bool already;
        if (vol.GetMute(out already) != 0 || already) continue;
        if (vol.SetMute(true, ref ctx) == 0) touched.Add(id);
      } else if (wanted.Contains(id)) {
        vol.SetMute(false, ref ctx);
      }
    }
    return touched.ToArray();
  }
}
'@

$selfPid = [uint32]([Math]::Max(0, $SelfPid))

$held = New-Object string[] 0

function Restore {
  if ($script:held.Length -gt 0) {
    try { [void][LlAudio]::Apply($false, $script:held, $script:selfPid) } catch { }
    $script:held = New-Object string[] 0
  }
}

# 就绪信号。主进程拿它确认 Add-Type 已经编完，这之后的命令才是毫秒级的。
[Console]::Out.WriteLine('ready')
[Console]::Out.Flush()

try {
  while ($true) {
    $line = [Console]::In.ReadLine()
    # 读到结尾＝主进程没了（管道关闭）。别人的声音还按在我们手里，必须还回去。
    if ($null -eq $line) { break }
    $line = $line.Trim()
    if ($line -eq 'q') { break }
    try {
      if ($line -eq '1') {
        # 已经在静音里就别再来一遍：第二遍会把第一遍静下去的会话读成
        # 「本来就静音」而跳过，名单反而空了，收工时谁也还原不了。
        if ($held.Length -eq 0) { $held = [LlAudio]::Apply($true, $null, $selfPid) }
        [Console]::Out.WriteLine('on ' + $held.Length)
      } elseif ($line -eq '0') {
        Restore
        [Console]::Out.WriteLine('off')
      }
    } catch {
      [Console]::Out.WriteLine('err ' + $_.Exception.Message)
    }
    [Console]::Out.Flush()
  }
} finally {
  Restore
}

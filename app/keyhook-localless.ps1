# keyhook.ps1 — low-level keyboard hook forwarding ALL key events to stdout as
# JSON lines (the renderer's shortcut engine does its own matching/filtering).
# -SwallowVk <vk>：该键的按下/抬起被吞掉（不传给系统），0 = 不吞。
param([int]$SwallowVk = 0xA5)
$src = @'
using System;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Windows.Forms;

public static class KeyHook {
    private delegate IntPtr LowLevelKeyboardProc(int nCode, IntPtr wParam, IntPtr lParam);
    private static LowLevelKeyboardProc proc = HookCallback;
    private static IntPtr hookId = IntPtr.Zero;

    [DllImport("user32.dll", SetLastError = true)]
    private static extern IntPtr SetWindowsHookEx(int idHook, LowLevelKeyboardProc lpfn, IntPtr hMod, uint dwThreadId);
    [DllImport("user32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool UnhookWindowsHookEx(IntPtr hhk);
    [DllImport("user32.dll")]
    private static extern IntPtr CallNextHookEx(IntPtr hhk, int nCode, IntPtr wParam, IntPtr lParam);
    [DllImport("kernel32.dll")]
    private static extern IntPtr GetModuleHandle(string lpModuleName);

    public static int SwallowVk = 0;

    private const int WH_KEYBOARD_LL = 13;
    private const int WM_KEYDOWN = 0x0100, WM_KEYUP = 0x0101, WM_SYSKEYDOWN = 0x0104, WM_SYSKEYUP = 0x0105;
    // KBDLLHOOKSTRUCT: vkCode(0) scanCode(4) flags(8) time(12) dwExtraInfo(16)。
    // paste.ps1 给自己注入的按键盖了这个戳。
    private const int EXTRAINFO_OFFSET = 16;
    private static readonly IntPtr LL_TAG = (IntPtr)0x4C4C5354;   // 'LLST'

    public static void Run() {
        using (Process cur = Process.GetCurrentProcess())
        using (ProcessModule mod = cur.MainModule) {
            hookId = SetWindowsHookEx(WH_KEYBOARD_LL, proc, GetModuleHandle(mod.ModuleName), 0);
        }
        Application.Run();
        UnhookWindowsHookEx(hookId);
    }

    private static string Name(int vk) {
        switch (vk) {
            case 0xA0: return "LeftShift";
            case 0xA1: return "RightShift";
            case 0xA2: return "LeftCtrl";
            case 0xA3: return "RightCtrl";
            case 0xA4: return "LeftAlt";
            case 0xA5: return "RightAlt";
            case 0x5B: return "LeftCmd";
            case 0x5C: return "RightCmd";
            case 0x20: return "Space";
            case 0x1B: return "Escape";
            case 0x0D: return "Enter";
            case 0x09: return "Tab";
            case 0x08: return "Backspace";
            case 0x2E: return "Delete";
        }
        if (vk >= 0x41 && vk <= 0x5A) return ((char)('A' + (vk - 0x41))).ToString();
        if (vk >= 0x30 && vk <= 0x39) return ((char)('0' + (vk - 0x30))).ToString();
        if (vk >= 0x70 && vk <= 0x7B) return "F" + (vk - 0x6F).ToString();
        return null; // ignore everything else
    }

    private static IntPtr HookCallback(int nCode, IntPtr wParam, IntPtr lParam) {
        if (nCode >= 0) {
            // 自家注入的按键（粘贴的 Shift+Insert、实时听写的退格）原路流回这里。
            // 它们不是用户敲的，快捷键引擎不该看见，更不该为它们做 I/O：这个回调
            // 是全系统输入的串行瓶颈，链上任何一处慢下来，整台机器的按键都跟着卡。
            //
            // 认 dwExtraInfo 戳而不是 LLKHF_INJECTED 标志位：远程控制软件（手机
            // 遥控这台机器）注入的按键同样带 INJECTED，按标志位过滤会把用户的
            // 远程热键一起废掉。私有戳只挡我们自己发的。
            if (Marshal.ReadIntPtr(lParam, EXTRAINFO_OFFSET) == LL_TAG) {
                return CallNextHookEx(hookId, nCode, wParam, lParam);
            }
            int vk = Marshal.ReadInt32(lParam);
            int msg = wParam.ToInt32();
            bool down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            bool up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
            if (down || up) {
                string n = Name(vk);
                // ponytail: 按住 RightAlt 时系统会把 Alt 状态喂给鼠标左键（Alt+左键=拖拽/无动作），
                // 表现就是“左键失灵”。Click 在钩子线程里直接吞掉右 Alt 的按下/抬起（不调用
                // CallNextHookEx），系统永远看不见 Alt，左键恢复正常；录音事件照发。
                if (SwallowVk != 0 && vk == SwallowVk && n != null) {
                    Console.Out.WriteLine("{\"key\":\"" + n + "\",\"down\":" + (down ? "true" : "false") + "}");
                    Console.Out.Flush();
                    return (IntPtr)1;
                }
                if (n != null) {
                    Console.Out.WriteLine("{\"key\":\"" + n + "\",\"down\":" + (down ? "true" : "false") + "}");
                    Console.Out.Flush();
                }
            }
        }
        return CallNextHookEx(hookId, nCode, wParam, lParam);
    }
}
'@
$log = [System.IO.Path]::Combine($env:TEMP, 'keyhook-debug.log')
function Dbg($m) { [System.IO.File]::AppendAllText($log, "$(Get-Date -Format o) $m`n") }
Add-Type -TypeDefinition $src -ReferencedAssemblies System.Windows.Forms
[KeyHook]::SwallowVk = $SwallowVk
Dbg("hook starting (swallow=$SwallowVk)")
[KeyHook]::Run()


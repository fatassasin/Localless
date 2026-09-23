// 自家注入按键的签名。PowerShell 版把这个值散在三个文件里（paste.ps1、
// type-helper.ps1、keyhook-localless.ps1），改一处忘两处就会让钩子把我们自己
// 发的键当成用户敲的，在回调里做阻塞 I/O——那是全系统输入的串行瓶颈。
// 收进一个常量之后这类事故没有发生的余地。
//
// 认 dwExtraInfo 戳而不是 LLKHF_INJECTED 标志位：远程控制软件（手机遥控这台
// 机器）注入的按键同样带 INJECTED，按标志位过滤会把用户的远程热键一起废掉。
pub const LL_TAG: usize = 0x4C4C_5354; // 'LLST'

use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC, VIRTUAL_KEY,
};

/// 一条按键事件。真键盘会同时给出扫描码，wScan 留 0 是 SendInput 的经典坑：
/// 部分输入法和按扫描码判键的程序会认错键。vk 仍然是权威（没有置
/// KEYEVENTF_SCANCODE），补上扫描码只是让这几下看起来更像真键盘敲的。
pub fn key(vk: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: LL_TAG,
            },
        },
    }
}

/// Unicode 通道：wVk 必须是 0，字符放在 wScan。不走键盘布局，也不经输入法翻译。
pub fn unicode_key(unit: u16, up: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE;
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: LL_TAG,
            },
        },
    }
}

pub fn send(buf: &[INPUT]) -> u32 {
    if buf.is_empty() {
        return 0;
    }
    unsafe {
        windows::Win32::UI::Input::KeyboardAndMouse::SendInput(
            buf,
            std::mem::size_of::<INPUT>() as i32,
        )
    }
}

/// UTF-16 宽字符串，末尾带 NUL。Win32 的字符串参数几乎都要这个。
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

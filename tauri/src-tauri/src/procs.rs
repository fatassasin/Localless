// 「仅在这些程序显示」那个下拉列表的候选：机器上现在跑着哪些程序。
//
// 只给设置页用，不参与判据。真正判「前台是谁」的是 micwatch::read_foreground，
// 它问的是前台窗口那一个 pid，不需要整张进程表——这里是另一件事：用户要从一份
// 名单里挑，而不是凭记忆把进程名拼对。
//
// CreateToolhelp32Snapshot 而不是起 powershell 去 Get-Process：这条路是用户点一下
// 输入框就走一遍，起子进程要付 ~800ms 的编译钱还会闪黑框，而快照是 ~3ms。
// 搬自 QuotaSidebar 的 procwatch.rs，那边同样的位置同样这么换过。
//
// 名字按 file_stem 归一（notepad.exe → notepad），和 micwatch 那边的 proc_name
// 一字不差——两边对不上的话，下拉里挑出来的名字会匹配不上前台窗口。

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};

/// 去掉 .exe、转小写。micwatch 比对时两边都 to_lowercase，所以这里先小写掉，
/// 用户在下拉里看到的就是最终会被存进设置的那个字。
fn normalize(n: &str) -> String {
    let n = n.trim().to_lowercase();
    n.strip_suffix(".exe").unwrap_or(&n).to_string()
}

/// PROCESSENTRY32W.szExeFile 是定长数组，后面填的是 0，不是 Rust 的字符串。
fn wsz(v: &[u16]) -> String {
    let n = v.iter().position(|&c| c == 0).unwrap_or(v.len());
    String::from_utf16_lossy(&v[..n])
}

/// 当前所有进程名，排好序去过重。
///
/// 拿不到就返回空表。调用方（设置页）把空表画成「列不出进程」而不是「没有进程」
/// ——这一栏是给人挑的，挑不出来时还能自己把名字打进去。
pub fn running_names() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Ok(h) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        eprintln!("[进程] 取快照失败，下拉列表这次是空的");
        return out;
    };
    let mut e = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    if unsafe { Process32FirstW(h, &mut e) }.is_ok() {
        loop {
            let n = normalize(&wsz(&e.szExeFile));
            if !n.is_empty() {
                out.push(n);
            }
            if unsafe { Process32NextW(h, &mut e) }.is_err() {
                break;
            }
        }
    }
    let _ = unsafe { CloseHandle(h) };
    out.sort();
    out.dedup();
    out
}

/// 设置页用：点一下输入框就要这份名单。
///
/// spawn_blocking 而不是同步命令：快照本身是几毫秒，但机器上进程多、页面又是
/// 每次聚焦都问一次，别把这几毫秒压在主线程的事件循环上。
#[tauri::command]
pub async fn procs_list() -> Vec<String> {
    tauri::async_runtime::spawn_blocking(running_names).await.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 名字归一化() {
        assert_eq!(normalize("Notepad.exe"), "notepad");
        assert_eq!(normalize(" CHROME.EXE "), "chrome");
        assert_eq!(normalize("chrome"), "chrome");
    }

    #[test]
    fn 定长数组按零截断() {
        let mut v = [0u16; 8];
        for (i, c) in "ab".encode_utf16().enumerate() {
            v[i] = c;
        }
        assert_eq!(wsz(&v), "ab");
    }

    /// 真机对照：这张表里至少得有自己，而且不带 .exe。
    #[test]
    fn 列得出自己() {
        let names = running_names();
        assert!(!names.is_empty(), "进程表不该是空的");
        assert!(names.iter().all(|n| !n.ends_with(".exe")), "名字应该已经去过后缀");
        assert!(names.windows(2).all(|w| w[0] < w[1]), "应该排好序且不重复");
    }
}

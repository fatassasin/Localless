// 一次听写的观测线。
//
//   %TEMP%\localless-main-debug.log
//
// 和 Electron 版写的是同一个文件（preload 的 dictLog、主进程的 __llToggle /
// hook spawned 都往这儿写），所以两版的记录能按时间混在一起读。
//
// 为什么必须有这个文件：页面那侧的 console 在这台机器上看不见——WebView2 的
// devtools 得手动开，而听写是在别的程序有焦点的时候发生的，一开 devtools 焦点
// 就跑了，现象本身就没了。真正难查的几种故障（药丸呼不出来、粘贴没落地、
// 引擎连不上）都只能靠事后读这个文件。
//
// 不做轮转。一次听写写几行，一天几百行，量级是几十 KB；加轮转反而会让
// 「昨天那次」在用户来问的时候已经被转走了。

use std::io::Write;
use std::path::PathBuf;

fn path() -> PathBuf {
    let base = std::env::var_os("TEMP")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("localless-main-debug.log")
}

/// 写一行。写不进去就算了——这是观测线，不能因为它失败拖垮听写本身。
pub fn log(line: &str) {
    let stamp = crate::history::now_iso();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path()) {
        let _ = writeln!(f, "{stamp} dict {line}");
    }
}

/// 页面那侧的观测线。录音状态机每走一步都会调它。
#[tauri::command]
pub fn dict_log(line: String) {
    log(&line);
}

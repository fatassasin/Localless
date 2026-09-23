// Localless —— Tauri 版入口。
//
// Electron 版把本地能力摊在 main.js 和 8 个 PowerShell sidecar 里：每读一次焦点
// 控件、每粘一次、每静一次音，都要现起一个 powershell.exe。这一版全部收进进程内，
// 直接调 Win32/COM。
//
// 唯一还在进程外的是 engine.py：那是 PyTorch/CTranslate2 的语音识别管线，
// 换语言没有收益，继续当 sidecar 跑。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    localless_lib::run();
}

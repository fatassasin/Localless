// 别在别人的 MSIX 包里跑。
//
// 从一个 MSIX 打包应用（Claude 桌面版就是）里起的进程，写 %APPDATA% 会被系统
// 悄悄改道到那个包自己的 `%LOCALAPPDATA%\Packages\<包名>\LocalCache\Roaming\` 下面：
// 新文件直接落那边，旧文件第一次被写时复制一份过去，之后各改各的。读是合并视图，
// 所以进程自己看不出任何异常。
//
// 对 Localless 这意味着 history.db、localless-settings.json、recordings 各有两份：
// 开机自启的那个进程写真的，从 Claude 里重启的那个写包里的副本。2026-09-30 查出来
// 真库 415 行、副本 309 行、交集 0——用户看到的是「每次更新后今天的历史就没了」。
//
// 这类进程拿不到包身份（GetCurrentPackageFullName 返回 APPMODEL_ERROR_NO_PACKAGE），
// 只能看结果：在 %APPDATA%\localless 里新建一个文件，问它最终落在哪。目录本身问不出来——
// 合并视图下目录解析回真路径，只有真被改道的文件才露馅。
//
// 发现被改道就交给 WMI 重起一个自己：Win32_Process.Create 的进程是 WmiPrvSE 生的，
// 不在任何包里。

use std::os::windows::process::CommandExt;
use std::path::PathBuf;

/// 被改道了就安排在包外重起，返回 true，调用方应当立刻退出。
pub fn relaunch_if_redirected() -> bool {
    // 重起出来的那个还是被改道（WMI 那条路不灵了）就别再起，免得无限重起。
    let escaped = std::env::args().any(|a| a == "--outside");
    let Some(real) = redirected_to() else { return false };
    if escaped {
        crate::debug::log(&format!("AppData 仍被改道到 {real}，照常启动（历史会写进那份副本）"));
        return false;
    }
    let Ok(exe) = std::env::current_exe() else { return false };
    let dir = exe.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
    let q = |s: &str| s.replace('\'', "''");
    // 等本进程退了再起，免得两个 Localless 同时抢键盘钩子。
    let script = format!(
        "Wait-Process -Id {pid} -Timeout 15 -ErrorAction SilentlyContinue; \
         Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{{\
         CommandLine=([char]34+'{exe}'+[char]34+' --outside'); CurrentDirectory='{dir}'}} | Out-Null",
        pid = std::process::id(),
        exe = q(&exe.to_string_lossy()),
        dir = q(&dir),
    );
    let ok = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .spawn()
        .is_ok();
    crate::debug::log(&format!("AppData 被改道到 {real}，交给 WMI 在包外重起：{}", if ok { "已安排" } else { "powershell 起不来" }));
    ok
}

/// 被改道时返回实际落点，没改道返回 None。
fn redirected_to() -> Option<String> {
    let dir = PathBuf::from(std::env::var_os("APPDATA")?).join("localless");
    let _ = std::fs::create_dir_all(&dir);
    let probe = dir.join(format!(".where-{}", std::process::id()));
    std::fs::write(&probe, b"").ok()?;
    let real = std::fs::canonicalize(&probe);
    let _ = std::fs::remove_file(&probe);
    let real = real.ok()?.to_string_lossy().into_owned();
    let low = real.to_lowercase();
    (low.contains(r"\packages\") && low.contains(r"\localcache\")).then_some(real)
}

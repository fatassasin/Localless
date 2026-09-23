// 录音时静音其它应用。搬自 main.js:1406-1429（muteHelper + localless:mute）和
// 1486（收摊时的 '0\nq\n'）。
//
// 干活的还是 app/mute.ps1，常驻：起一次，stdin 一行一条命令（1=静音 0=还原
// q=退出）。以前是开录 spawn 一个、停录再 spawn 一个，每个都要现编译 Add-Type
// 约一秒——静音真正落地是在开录一秒之后，短句说完了才静下来，看着就是「停录
// 才静音」；而且那两个进程互不相干，谁先编完谁后应用没有保证，偶尔还会留着
// 静音不还原。常驻之后每条命令 20ms 上下，且严格按先后顺序执行。
//
// ── 这一版改了脚本里的一处 ──────────────────────────────────────
//
// 「哪些会话算自家的」原来按可执行文件路径认（Electron 的 audio service 和主
// 进程同一份 electron.exe）。Tauri 的声音来自 WebView2 的 msedgewebview2.exe，
// 连可执行文件都在 Edge 运行时目录里，路径永远对不上——照搬过来就是开录时把
// 自己也静音了，表现成「录音时回放历史没声音」。脚本那侧改成了走进程祖先链，
// 理由写在 mute.ps1 的 IsSelf 上面。
//
// ── 不需要清孤儿 ────────────────────────────────────────────────
//
// 钩子那边要 kill_orphan_scripts，这边不用：mute.ps1 是 stdin 驱动的，我们一死
// 管道就关，它 ReadLine 读到 null 就 break，然后 finally 里把静音还回去。被
// taskkill /F 也一样——这是它和钩子唯一的结构性区别。

use parking_lot::Mutex;
use std::io::{BufRead, Write};

static CHILD: Mutex<Option<std::process::Child>> = Mutex::new(None);

/// 起一个常驻的 mute.ps1，已经有了就复用。
///
/// stdout 必须有人读。不读的话管道填满，脚本就卡在写 'ready' 上——再往后所有
/// 命令它都收不到，静音整个失效而且一声不吭。
fn spawn() -> bool {
    let mut slot = CHILD.lock();
    if let Some(c) = slot.as_mut() {
        // 已经退了（脚本崩了 / 被人杀了）就重起一个。
        match c.try_wait() {
            Ok(None) => return true,
            _ => {
                let _ = c.wait();
                *slot = None;
            }
        }
    }

    let script = crate::models::root().join("app").join("mute.ps1");
    if !script.is_file() {
        crate::debug::log(&format!("mute 脚本不在：{}", script.display()));
        return false;
    }

    use std::os::windows::process::CommandExt;
    let child = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            &script.to_string_lossy(),
            "-SelfPid",
            &std::process::id().to_string(),
        ])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            crate::debug::log(&format!("mute spawn 失败 {e}"));
            return false;
        }
    };
    if let Some(out) = child.stdout.take() {
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                // 只有 err 值得记。'ready'/'on N'/'off' 每次听写都来，记了就是刷屏。
                if line.starts_with("err") {
                    crate::debug::log(&format!("mute {line}"));
                }
            }
        });
    }
    *slot = Some(child);
    true
}

/// 提前把 Add-Type 那一秒付掉。启动时「录音时静音」开着才叫——关着的话这一秒
/// 白付，而且多挂一个常驻 PowerShell。对应 Electron 版 whenReady 里那一句。
pub fn warm_up() {
    if crate::settings::read()
        .get("muteWhileRecording")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        spawn();
    }
}

/// 开录 / 停录。
///
/// 写完就走，不等回音：静音失败不该拖住录音——用户已经在说话了，为了一个
/// 「别人的声音没静下去」卡住采集是把小问题换成大问题。
#[tauri::command]
pub fn mute_set(on: bool) {
    if !spawn() {
        return;
    }
    let mut slot = CHILD.lock();
    let Some(c) = slot.as_mut() else { return };
    let Some(stdin) = c.stdin.as_mut() else { return };
    let line: &[u8] = if on { b"1\n" } else { b"0\n" };
    if stdin.write_all(line).and_then(|_| stdin.flush()).is_err() {
        // 管道断了。丢掉这个孩子，下次 spawn 会起新的——它那侧读到 EOF 会自己
        // 把静音还回去，所以这里不用额外补一次还原。
        crate::debug::log("mute 管道断了");
        if let Some(mut c) = slot.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// 收摊。'0' 是还原，'q' 是退出，然后关掉 stdin。
///
/// 必须等它退完再走：别人的声音还按在我们手里，进程先死的话要等操作系统关管道
/// 才轮到它还原，这中间用户看到的就是「退出之后别的程序还是没声音」。
/// 1.5 秒封顶——还原本身是毫秒级的，超过这个数说明它已经不正常了，不能为它
/// 把退出卡住。
pub fn stop() {
    let Some(mut c) = CHILD.lock().take() else { return };
    if let Some(mut stdin) = c.stdin.take() {
        let _ = stdin.write_all(b"0\nq\n");
        let _ = stdin.flush();
        drop(stdin); // 关掉写端，它那侧的 ReadLine 才会收到 EOF
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    loop {
        match c.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            _ => break,
        }
    }
    crate::debug::log("mute 没按时退，强杀");
    let _ = c.kill();
    let _ = c.wait();
}

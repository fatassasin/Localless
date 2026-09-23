// 投递。把转写结果送进用户正在打字的那个输入框。
//
// 搬自 Electron 版 main.js:1270-1379（剪贴板守卫 + paste-begin/again/release +
// read-focused）。
//
// ── 为什么借剪贴板 ────────────────────────────────────────────────
//
// 逐字符注入（SendInput 打 Unicode）在中文输入法开着的时候会被吃掉，在 Electron
// 和浏览器里还会触发一堆 keydown 处理。粘贴是唯一一条到哪儿都一样的路。代价是
// 得借用剪贴板，而剪贴板是用户的东西——借了必须还。
//
// ── 为什么拆成三步 ────────────────────────────────────────────────
//
// 一次 invoke 做完「借—粘—还」的话，「没落地，再补一枪」就得把整套重来：再写一次
// 剪贴板，Win+V 里就再多一条听写残渣。拆开之后剪贴板全程只被碰一次，补枪纯粹是
// 再发一遍按键。
//
// 代价是调用方必须自己保证 release 一定会跑到（try/finally）——漏一次，用户
// 复制的东西就再也回不来了。
//
// ── 和 Electron 版的差别 ──────────────────────────────────────────
//
// 那边写「不留痕」的剪贴板要另起一个 PowerShell helper（Electron 的 clipboard
// 挂不上 CanIncludeInClipboardHistory），于是有一整套临时文件、退出码 2 的重试、
// 以及 ~820ms 的 C# 现编译。这边 paste.rs 直接做 Win32，一次调用就完事，所以
// 那些全没了：没有临时文件，没有退出码，没有「写没写成说不准」的中间态。
// 留下来的只有借还账本本身。

use crate::paste::{Snapshot, Trace};
use parking_lot::Mutex;
use serde_json::{json, Value};

/// 借条。previous 是用户原来的剪贴板（每一种格式都在），written 是我们塞进去的
/// 那段听写结果。
///
/// previous 是 None = 借之前没抄下来（剪贴板被别人占着、或者大得离谱），那就
/// 不还——还一份残缺的回去比不还更糟。
///
/// listed 是原内容在 Win+V 里那一条的 Id，只在留痕模式下记，见 cliphist.rs。
struct Guard {
    previous: Option<Snapshot>,
    written: String,
    listed: Option<windows::core::HSTRING>,
}

static GUARD: Mutex<Option<Guard>> = Mutex::new(None);

/// 幂等地写一次剪贴板。内容已经就是它了就不写——省掉一次剪贴板通知风暴
/// （每次写都会惊动所有剪贴板监听程序），也顺手压掉了投递失败路径上天生的
/// 重复写：补枪要放一次，摆药丸时又要放一次。
fn write_once(text: &str, trace: Trace) -> bool {
    if crate::paste::read_text().as_deref() == Some(text) {
        return false;
    }
    crate::paste::set_text(text, trace);
    true
}

/// 只记账，不写剪贴板。
///
/// previous 只认最早那一份：两段结果先后借用剪贴板时，拿第二次的当前值去还原，
/// 还回去的正是上一段的听写结果，等于没还。
fn stash(text: &str, record: bool) {
    let mut g = GUARD.lock();
    let (previous, listed) = match g.as_ref() {
        Some(old) => (old.previous.clone(), old.listed.clone()),
        None => {
            let snap = crate::paste::snapshot();
            let listed = if record { snap.as_ref().and_then(crate::cliphist::top_if_same) } else { None };
            (snap, listed)
        }
    };
    *g = Some(Guard { previous, written: text.to_string(), listed });
}

/// 「自动清理剪贴板」开着 = 这次听写不许留痕。
///
/// 默认值是「开」：键不在、或者设置文件读不出来时也要不留痕。反过来的话，一次
/// 读设置失败就等于把用户关掉的留痕行为悄悄打开，而这种失败是不报错的。
pub fn hide_from_history() -> bool {
    crate::settings::read()
        .get("clipboardAutoCleanEnabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

// ── 给页面的命令 ──────────────────────────────────────────────────

/// 药丸上那颗「复制」，以及任何一次「只放进剪贴板、不粘贴」。
///
/// 先把借条撕掉：这是用户主动要的一次复制，结果应该一直留在剪贴板里，不能被
/// 稍后那次 release 还原掉。
#[tauri::command]
pub async fn clipboard_write(text: String, no_history: bool) -> bool {
    tauri::async_runtime::spawn_blocking(move || {
        *GUARD.lock() = None;
        write_once(&text, if no_history { Trace::NoHistory } else { Trace::Record });
        true
    })
    .await
    .unwrap_or(false)
}

/// 第一步：借剪贴板 + 发粘贴键。
///
/// 写不进剪贴板就绝不发键——那一下会把别人的剪贴板内容捅进用户的文档，比什么都
/// 不粘糟得多。返回 false 就是这种情况，调用方该直接去摆药丸。
///
/// 自动清理开着时这一下是 Trace::Hidden：连远控同步也别让它看见，理由见那边。
/// 关着时听写本来就要进 Win+V，只能照常写。
#[tauri::command]
pub async fn paste_begin(text: String, no_history: bool) -> bool {
    tauri::async_runtime::spawn_blocking(move || {
        stash(&text, !no_history);
        let trace = if no_history { Trace::Hidden } else { Trace::Record };
        // 剪贴板已经是这段文字就只发键。注意这里仍然要记借条：上面 stash 过了。
        if write_once(&text, trace)
            && crate::paste::read_text().as_deref() != Some(text.as_str())
        {
            // 写完读回来对不上 = 没写进去（剪贴板被别的程序抢着占住）。
            crate::debug::log("paste-begin 剪贴板写不进去，没有发键");
            return false;
        }
        // 让目标程序的剪贴板监听跑完再发键。
        std::thread::sleep(std::time::Duration::from_millis(40));
        crate::paste::press_paste();
        true
    })
    .await
    .unwrap_or(false)
}

/// 第二步：确认没落地，补一枪。
///
/// 补枪前先确认剪贴板还是我们那份。用户在这一两秒里自己复制了别的东西的话，
/// 直接补枪等于把他刚复制的内容捅进目标程序——比不补枪糟得多。重新写一次我们的
/// 文字，previous 仍然记着最早那份原内容，不会多留一条残渣。
///
/// 这次重写一律不留痕，不看开关：留痕那份 paste_begin 已经记过一条了，这里再记
/// 就是同一段听写在 Win+V 里出现两遍。
#[tauri::command]
pub async fn paste_again() -> bool {
    tauri::async_runtime::spawn_blocking(|| {
        let Some(value) = GUARD.lock().as_ref().map(|g| g.written.clone()) else {
            return false;
        };
        if write_once(&value, Trace::Hidden)
            && crate::paste::read_text().as_deref() != Some(value.as_str())
        {
            crate::debug::log("paste-again 剪贴板写不进去，没有补枪");
            return false;
        }
        crate::paste::press_paste();
        true
    })
    .await
    .unwrap_or(false)
}

/// 第三步：还剪贴板。两种模式都还——用户原来复制的东西永远是 Ctrl+V 的那一个。
///
/// - `hide`（自动清理开着）：原样还回去，不留痕。它早就在 Win+V 里了，再记一条
///   就是白白多一份重复。
/// - 关着：听写已经在 Win+V 顶上留了一条。原内容要是认得出是历史里哪一条，就
///   留痕写回去、再删掉下面那条旧的，Win+V 变成「原内容、听写、……」；认不出就
///   只不留痕地还，Ctrl+V 照样是原内容，只是 Win+V 里听写排在它上面。
#[tauri::command]
pub async fn paste_release(hide: bool) -> Value {
    tauri::async_runtime::spawn_blocking(move || {
        // 目标程序处理 WM_PASTE 需要一点时间，还原得排在它后面。早还的话
        // 粘进去的会是用户的旧内容。
        std::thread::sleep(std::time::Duration::from_millis(250));
        give_back(hide)
    })
    .await
    .unwrap_or_else(|_| json!({ "restored": false }))
}

/// 「自动清理剪贴板」关着、而这段听写没地方粘（光标不在输入框里）时的收尾：
/// 在 Win+V 里留下记录，Ctrl+V 仍然是用户原来复制的东西。要这段文字就点药丸
/// 上的「复制」，或者去 Win+V 第二条拿。
#[tauri::command]
pub async fn clipboard_record(text: String) -> Value {
    tauri::async_runtime::spawn_blocking(move || {
        stash(&text, true);
        write_once(&text, Trace::Record);
        give_back(false)
    })
    .await
    .unwrap_or_else(|_| json!({ "restored": false }))
}

fn give_back(hide: bool) -> Value {
    let Some(g) = GUARD.lock().take() else {
        return json!({ "restored": false });
    };
    let Some(previous) = g.previous else {
        crate::debug::log("剪贴板借之前没抄下原内容，不还原");
        return json!({ "restored": false });
    };
    if !hide {
        crate::cliphist::wait_recorded(&g.written);
    }
    // 剪贴板已经不是我们写的那份了 = 用户中途自己复制过别的东西。还原会把
    // 他刚复制的东西顶掉，一概不动。
    if crate::paste::read_text().as_deref() != Some(g.written.as_str()) {
        return json!({ "restored": false });
    }
    let listed = if hide { None } else { g.listed };
    let Some(old) = listed else {
        crate::paste::restore(&previous, Trace::Hidden);
        return json!({ "restored": true, "reordered": false });
    };
    crate::paste::restore(&previous, Trace::Record);
    let reordered = crate::cliphist::drop_older(&previous, &old);
    if !reordered {
        crate::debug::log("原剪贴板内容没能挪回 Win+V 第一位");
    }
    json!({ "restored": true, "reordered": reordered })
}

// ── 读输入框 ──────────────────────────────────────────────────────

/// 常驻的 uia-helper。一次 `powershell -File uia-helper.ps1` 冷启动实测 335ms，
/// 其中真正查 UIA 的部分只有几十毫秒，剩下全是进程启动 + Add-Type。
///
/// 而这条路一次听写要走四遍：两枪粘贴 × 每枪两次确认。四次冷启动就是 1.3 秒，
/// 占了「粘贴落空之后复制药丸迟迟不出来」那 2.4 秒的一多半——用户报的就是这个。
/// 改成一个长住的 `-Serve` 进程之后，每次问答降到几十毫秒。
///
/// 这里只存管道：写一行进去，等一行出来。读那侧另起一个线程往 channel 里塞，
/// 于是「等回答」可以带超时——直接在管道上读是没法超时的，而 UIA 卡死是真会
/// 发生的事（旧版那个 1500ms 的 kill 就是为它准备的）。
struct Uia {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    rx: std::sync::mpsc::Receiver<String>,
}

static UIA: Mutex<Option<Uia>> = Mutex::new(None);

/// 起一个常驻 helper。起不来返回 None，调用方当作「这次什么都没读到」。
fn spawn_uia() -> Option<Uia> {
    let helper = crate::models::root().join("app").join("uia-helper.ps1");
    if !helper.is_file() {
        return None;
    }
    let mut cmd = std::process::Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        &helper.to_string_lossy(),
        "-Serve",
    ])
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().ok()?;
    let stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    // 读线程。helper 一行一个答案，读到什么塞什么；管道一断（进程没了）线程
    // 自己结束。收不下了（这一侧已经把连接扔了）也结束，别赖着不走。
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout).lines() {
            match line {
                Ok(l) => {
                    if tx.send(l).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });
    Some(Uia { child, stdin, rx })
}

/// 问一次。超时或者管道断了返回 None——调用方会把这条连接整个扔掉重开，
/// 而不是留着一个可能还欠着一行回答的进程：下次问的时候先收到的会是上一次
/// 迟到的那一行，答非所问比没答案更坏。
fn ask(u: &mut Uia) -> Option<Value> {
    use std::io::Write;
    u.stdin.write_all(b"read\n").ok()?;
    u.stdin.flush().ok()?;
    let line = u
        .rx
        .recv_timeout(std::time::Duration::from_millis(1500))
        .ok()?;
    Some(serde_json::from_str(line.trim()).unwrap_or_else(|_| json!({})))
}

/// 把冷启动那 500ms 提前付掉。放在启动流程里，别让它落在第一次听写结束的那
/// 一刻——正是最不该多花时间的地方。起不来什么都不做，第一次读的时候会再试。
pub fn warm_up() {
    std::thread::spawn(|| {
        let mut slot = UIA.lock();
        if slot.is_none() {
            *slot = spawn_uia();
        }
    });
}

/// 当前焦点控件里已经有的文字，连同「凭什么判断它可写」的证据。
///
/// uia-helper.ps1 一个字没改地搬过来了。那份脚本里有两处是踩出来的，重写一遍
/// 很容易丢：一是 IsPassword 直接返回空（密码框的内容绝不能读出来送进模型），
/// 二是两个 pattern 都没拿到时等 180ms 重读一次（Chromium 的无障碍树是被第一次
/// UIA 请求踢起来的，建的过程是异步的）。
///
/// 为什么不在 Rust 里直接调 UIAutomationClient：能调，但那份脚本是这台机器上
/// 一年多试出来的，而它的失败模式（浏览器里第一次听写蹦药丸）只有在真实应用里
/// 才复现得出来。慢的那部分不是它，是进程启动——所以改成常驻而不是重写。
///
/// 1500ms 超时之后当作「什么都没读到」，返回 `{}`。调用方靠 hasValue/hasText
/// 两个字段分辨「确实只读」和「UIA 这次什么都没拿到」——分不清的话后者也会被
/// 当成不可写，于是浏览器里第一次听写就蹦药丸。
#[tauri::command]
pub async fn read_focused() -> Value {
    tauri::async_runtime::spawn_blocking(|| {
        let mut slot = UIA.lock();
        if slot.is_none() {
            *slot = spawn_uia();
        }
        if let Some(u) = slot.as_mut() {
            if let Some(v) = ask(u) {
                return v;
            }
            // 答不上来的连接不留：超时的那一行可能过会儿才到，留着就会被下一次
            // 提问收走。杀掉，下次现开一个——冷启动的代价只在出过错之后付。
            if let Some(mut dead) = slot.take() {
                let _ = dead.child.kill();
                let _ = dead.child.wait();
            }
            crate::debug::log("read-focused 没答上来，helper 已重置");
        }
        json!({})
    })
    .await
    .unwrap_or_else(|_| json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 借条只认最早那一份原内容。第二次 stash 不能把 previous 换成第一段的
    /// 听写结果——那样「还」回去的正是上一段听写，用户的东西还是没了。
    #[test]
    fn 并行借用只记最早那份原内容() {
        let mine = Some(Snapshot::of_text("用户自己复制的"));
        *GUARD.lock() = Some(Guard {
            previous: mine.clone(),
            written: "第一段听写".into(),
            listed: None,
        });
        stash("第二段听写", false);
        let g = GUARD.lock();
        let g = g.as_ref().unwrap();
        assert_eq!(g.previous, mine);
        assert_eq!(g.written, "第二段听写");
    }

    /// 在真剪贴板和真 Win+V 上走一遍借还（不发粘贴键）。会动用户的剪贴板，所以
    /// 默认不跑：`cargo test --release 真剪贴板 -- --ignored --test-threads=1`。
    /// 跑完删掉测试条目、把原剪贴板原样还回去。
    #[test]
    #[ignore]
    fn 真剪贴板借还() {
        use crate::cliphist::tests::{purge, texts};
        let wait = || std::thread::sleep(std::time::Duration::from_millis(800));
        let mine = crate::paste::snapshot().expect("抄不下当前剪贴板");
        *GUARD.lock() = None;

        let result = std::panic::catch_unwind(|| {
            crate::paste::set_text("LLTEST-旧复制", Trace::Record);
            wait();
            assert_eq!(texts(1), ["LLTEST-旧复制"]);

            // 关着：听写留一条记录，原内容回到第一位，且不重复。
            stash("LLTEST-听写一", true);
            write_once("LLTEST-听写一", Trace::Record);
            let r = give_back(false);
            wait();
            assert_eq!(r["reordered"], true, "{r}");
            assert_eq!(crate::paste::read_text().as_deref(), Some("LLTEST-旧复制"));
            assert_eq!(texts(3)[..2], ["LLTEST-旧复制", "LLTEST-听写一"]);
            assert_ne!(texts(3)[2], "LLTEST-旧复制", "旧条目没删掉");

            // 开着：Win+V 一条都不多，Ctrl+V 还是原内容。
            stash("LLTEST-听写二", false);
            write_once("LLTEST-听写二", Trace::Hidden);
            let r = give_back(true);
            wait();
            assert_eq!(r["restored"], true, "{r}");
            assert_eq!(crate::paste::read_text().as_deref(), Some("LLTEST-旧复制"));
            assert_eq!(texts(2), ["LLTEST-旧复制", "LLTEST-听写一"]);
        });

        purge("LLTEST-");
        crate::paste::restore(&mine, Trace::Hidden);
        assert_eq!(crate::paste::snapshot().as_ref().map(|s| s.text()), Some(mine.text()));
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }
}

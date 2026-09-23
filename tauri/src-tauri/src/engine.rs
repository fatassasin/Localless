// 引擎监护。搬自 Electron 版 main.js:57-136（startEngine / restartEngine /
// watchEngineHeartbeat）。
//
// engine.py 一行没改：它本来就是个独立进程，只认 ws://127.0.0.1:8765 和
// %APPDATA% 里那份设置文件，搜不到任何 electron 字样。这边要做的只是「起它、
// 盯它、它死了再起一遍」。
//
// 三道监护，缺一道都会留下一种查不出来的死法：
//
//   1. 进程退了 → 退避重启。反复崩就拉长间隔：显卡被别的程序占满时，3 秒一次
//      地重载模型只会让情况更糟——每次都要再抢一遍显存，每次都卡到看门狗超时。
//   2. 心跳停了 → 强制重启。engine.py 的事件循环每 5 秒摸一次
//      engine-heartbeat。卡在 GPU 里的原生调用会连 GIL 一起攥着，那种时候
//      engine.py 自己的看门狗线程也醒不过来，只有进程外看得见。
//   3. 日志单独落一份。引擎卡死或被强杀时这一侧是唯一还醒着的，而整条监护链路
//      只能事后复盘。Electron 那边的理由是 console.log 接不到活文件；这边
//      stderr 虽然能重定向，但那是调试时才挂的，打包跑起来照样是黑洞。
//
// 和 Electron 版共用同一份 engine-supervisor.log 和 engine-heartbeat：两版不会
// 同时跑（都要抢 8765），共用一份日志才能把「上一次是怎么死的」连起来看。

use parking_lot::Mutex;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// engine.py 的看门狗判定 GPU 卡死时用这个退出码，跟普通崩溃区分开。
const WATCHDOG_EXIT: i32 = 3;
const EXIT_WINDOW: Duration = Duration::from_secs(10 * 60);
const HEARTBEAT_DEAD: Duration = Duration::from_secs(45);
/// 刚起来的那段不算：python 要 import torch，到写下第一次心跳之前有十几秒。
const HEARTBEAT_GRACE: Duration = Duration::from_secs(60);

/// 一代引擎。job 必须跟 child 同生共死：它一 drop，KILL_ON_JOB_CLOSE 就把整棵
/// 树收掉——python 自己崩了、waiter 把这一代清出 PROC 时，下模型的子进程也跟着
/// 走，不会攥着显存等下一代来抢。
struct Engine {
    child: Child,
    /// 建 job 或塞进 job 失败时是 None，杀树退回 taskkill，见 kill_tree。
    job: Option<crate::job::Job>,
}

static PROC: Mutex<Option<Engine>> = Mutex::new(None);
/// 代次。restartEngine 靠它作废在途的重启，不然「手动重启」和「崩溃自动重启」
/// 会同时起两个引擎，两个都去抢 8765，表现成「引擎时好时坏」。
static GENERATION: AtomicU64 = AtomicU64::new(0);
static EXITS: Mutex<Vec<Instant>> = Mutex::new(Vec::new());
static STARTED_AT: Mutex<Option<Instant>> = Mutex::new(None);
static QUITTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn script() -> PathBuf {
    crate::models::root().join("app").join("engine.py")
}
fn python() -> PathBuf {
    crate::models::root()
        .join("app")
        .join(".venv")
        .join("Scripts")
        .join("python.exe")
}
fn heartbeat() -> PathBuf {
    crate::models::root().join("engine-heartbeat")
}

pub fn log(line: &str) {
    let stamp = crate::history::now_iso();
    eprintln!("[引擎] {line}");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::models::root().join("engine-supervisor.log"))
    {
        let _ = writeln!(f, "{stamp} {line}");
    }
}

// ── 起 ────────────────────────────────────────────────────────────

pub fn start() {
    let (py, sc) = (python(), script());
    // Electron 那边只查 engine.py 在不在。python 也要查：venv 没建时
    // spawn 失败的报错是「系统找不到指定的文件」，没人会想到是虚拟环境没装。
    if !sc.is_file() {
        log(&format!("engine.py 不在 {} — 不起引擎", sc.display()));
        return;
    }
    if !py.is_file() {
        log(&format!("venv 的 python 不在 {} — 不起引擎", py.display()));
        return;
    }
    if PROC.lock().is_some() {
        return;
    }

    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let mut cmd = Command::new(&py);
    cmd.arg(&sc)
        .current_dir(sc.parent().unwrap_or(&py))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            log(&format!("起不来：{e}"));
            return;
        }
    };
    let pid = child.id();
    log(&format!("engine pid={pid} 起来了（第 {generation} 代）"));
    let job = match crate::job::Job::create().and_then(|j| j.assign(&child).map(|()| j)) {
        Ok(j) => Some(j),
        Err(e) => {
            log(&format!("engine 没能放进 Job Object：{e} — 停止时退回 taskkill"));
            None
        }
    };

    // 引擎自己的日志已经写在 engine.log 里了，这两条流只是转出来方便实时看。
    for (tag, stream) in [
        ("engine", child.stdout.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>)),
        ("engine!", child.stderr.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>)),
    ] {
        if let Some(s) = stream {
            std::thread::spawn(move || {
                for line in BufReader::new(s).lines().map_while(Result::ok) {
                    let t = line.trim();
                    if !t.is_empty() {
                        eprintln!("[{tag}] {t}");
                    }
                }
            });
        }
    }

    *STARTED_AT.lock() = Some(Instant::now());
    *PROC.lock() = Some(Engine { child, job });
    watch_heartbeat();

    // 等它退。Rust 没有 'exit' 事件，只能占一个线程 wait——比轮询 try_wait
    // 准，退出码也只有 wait 拿得到。
    std::thread::spawn(move || {
        // 把 Child 借出来 wait：锁不能一直握着，那会把 restart 卡死。
        let code = loop {
            let taken = {
                let mut g = PROC.lock();
                match g.as_mut() {
                    Some(e) if e.child.id() == pid => e.child.try_wait().ok().flatten(),
                    // 被 restart 换掉了：这一代的使命结束，不要再报退出。
                    _ => return,
                }
            };
            match taken {
                Some(st) => break st.code().unwrap_or(-1),
                None => std::thread::sleep(Duration::from_millis(250)),
            }
        };
        {
            let mut g = PROC.lock();
            if g.as_ref().map(|e| e.child.id()) == Some(pid) {
                *g = None;
            }
        }
        if QUITTING.load(Ordering::SeqCst) || GENERATION.load(Ordering::SeqCst) != generation {
            log(&format!("engine pid={pid} 退了 code={code}（不重启）"));
            return;
        }
        let now = Instant::now();
        let n = {
            let mut xs = EXITS.lock();
            xs.retain(|t| now.duration_since(*t) < EXIT_WINDOW);
            xs.push(now);
            xs.len() as u32
        };
        let delay = (3000u64).saturating_mul(1u64 << (n - 1).min(10)).min(60000);
        log(&format!(
            "engine pid={pid} 退了 code={code}{} — {n} 次/10 分钟，{delay}ms 后重启",
            if code == WATCHDOG_EXIT { "（看门狗判定 GPU 卡死，重载模型）" } else { "" }
        ));
        std::thread::sleep(Duration::from_millis(delay));
        start();
    });
}

pub fn restart() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    let engine = PROC.lock().take();
    if let Some(e) = engine {
        kill_tree(e);
        std::thread::sleep(Duration::from_millis(150));
    } else {
        std::thread::sleep(Duration::from_millis(50));
    }
    start();
}

/// 退出时收摊（托盘 Quit、关机）。Localless 被 taskkill /F 强杀时不会走到这里，
/// 但那种情况 job 句柄跟着进程关，KILL_ON_JOB_CLOSE 照样把引擎树收掉。
pub fn stop() {
    QUITTING.store(true, Ordering::SeqCst);
    if let Some(e) = PROC.lock().take() {
        kill_tree(e);
    }
}

/// 杀整棵引擎树。`Child::kill()` 只杀 python 自己，而引擎会 spawn 出下模型的
/// 子进程，留着就是孤儿，还攥着显存——下一代引擎起来照样抢不到。
fn kill_tree(mut e: Engine) {
    if let Some(job) = e.job.take() {
        // terminate 万一失败也不要紧：job 在这里 drop，KILL_ON_JOB_CLOSE 照样收树。
        if let Err(err) = job.terminate() {
            log(&format!("TerminateJobObject 失败：{err} — 靠关句柄收树"));
        }
        return;
    }
    // 没有 job 时只能靠外部 taskkill /t 遍历子树。但关机时不能起新进程：会话拆到
    // 一半再 CreateProcess 会弹 0xc0000142 的错误框，把「正在关闭应用」那一屏卡住
    // （见 job.rs 文件头）。那时只杀 python 自己，剩下的系统马上也会收走。
    if shutting_down() {
        let _ = e.child.kill();
        return;
    }
    use std::os::windows::process::CommandExt;
    let _ = Command::new("taskkill")
        .args(["/pid", &e.child.id().to_string(), "/t", "/f"])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn shutting_down() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SHUTTINGDOWN};
    // SAFETY: 纯查询，无指针参数。
    unsafe { GetSystemMetrics(SM_SHUTTINGDOWN) != 0 }
}

// ── 心跳 ──────────────────────────────────────────────────────────

static WATCHING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn watch_heartbeat() {
    if WATCHING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_secs(10));
        if QUITTING.load(Ordering::SeqCst) || PROC.lock().is_none() {
            continue;
        }
        let started = *STARTED_AT.lock();
        match started {
            Some(t) if t.elapsed() < HEARTBEAT_GRACE => continue,
            None => continue,
            _ => {}
        }
        // 读不到就当没这回事：宁可漏判，也不能因为一次 stat 失败去杀一个正在
        // 好好干活的引擎。
        let Ok(age) = std::fs::metadata(heartbeat())
            .and_then(|m| m.modified())
            .and_then(|t| SystemTime::now().duration_since(t).map_err(std::io::Error::other))
        else {
            continue;
        };
        if age < HEARTBEAT_DEAD {
            continue;
        }
        let pid = PROC.lock().as_ref().map(|e| e.child.id()).unwrap_or(0);
        log(&format!(
            "engine pid={pid} 心跳停了 {} 秒 — 事件循环已经死掉（多半是卡在 GPU 里连 GIL 一起占着），强制重载模型",
            age.as_secs()
        ));
        restart();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 退避是「3 秒起，每多崩一次翻倍，封顶 60 秒」。第一次崩就等 60 秒是个
    /// bug（引擎偶发崩一次的话，用户得干等一分钟才能再说话）；不封顶也是
    /// （崩十几次之后就再也不重启了，表现成「引擎彻底没了」）。
    fn backoff(n: u32) -> u64 {
        (3000u64).saturating_mul(1u64 << (n - 1).min(10)).min(60000)
    }

    #[test]
    fn 退避从三秒起翻倍封顶一分钟() {
        assert_eq!(backoff(1), 3000);
        assert_eq!(backoff(2), 6000);
        assert_eq!(backoff(3), 12000);
        assert_eq!(backoff(4), 24000);
        assert_eq!(backoff(5), 48000);
        assert_eq!(backoff(6), 60000);
        // 崩到第 30 次也还是 60 秒，不会溢出成 0——移位溢出在 release 下
        // 是静默回绕，那会让「崩得最狠的时候重启得最快」。
        assert_eq!(backoff(30), 60000);
        assert_eq!(backoff(64), 60000);
    }
}

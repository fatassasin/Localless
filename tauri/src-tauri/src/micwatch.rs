// 悬浮麦克风什么时候该出现。搬自 Electron 版 main.js 的六段：
// 379-385（matchesMicApps）、386-516（UU 远程会话探测）、517-566（客户端是不是
// 触屏设备）、568-600（startRemoteWatcher）、625-650（applyMicVisibility）、
// 876-935（前台窗口监听）。
//
// 判据一条没改——那些数字和倒向都是在真机上一次次试错试出来的，注释里写着为什么。
// 换掉的只有「前台窗口怎么拿」：
//
// 那边是 spawn 一个 PowerShell，让它 Add-Type 现编一段 C# 轮询循环，再配上孤儿
// 自杀心跳（父进程被 taskkill /F 时不走 will-quit）、连挂三次就放弃、3 秒重启、
// 启动前先杀同名孤儿——整整四套脚手架，全是为「监听器住在另一个进程里」付的钱。
// 那段 C# 干的事就是每 450ms 调四个 Win32 函数。这边直接调同样那四个，于是
// 四套脚手架一起不必存在：没有子进程，就没有孤儿，也没有起不来这回事。
//
// 两条判据的倒向必须记住：**测不出来一律放行**。主机上多一个图标只是碍眼，
// 远程时图标没了却是功能缺失，而那时候用户恰恰没有键盘可以补救。

use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

// ── 常量。和 Electron 版一一对应 ──────────────────────────────────

const FG_POLL_MS: u64 = 450;
const REMOTE_POLL_MS: u64 = 3000;
const REMOTE_WINDOW_MS: u128 = 45_000;
/// 窗口没铺够就先不下结论，返回 None（放行）。
const REMOTE_MIN_SPAN_MS: u128 = 20_000;
/// 退路只剩旧版 GameViewer 的明文 .txt 日志：那时空闲每 5 分钟才写一行心跳
/// （不到 1 B/s），推流每秒写一行码率监控（约 200 B/s），差三个数量级。
/// .slog 新格式上这条判据整个作废（推流 162.8 B/s 比空闲的 5 秒窗口峰值 279 B/s
/// 还慢），所以只对 .txt 生效。
const REMOTE_MIN_RATE_TXT: f64 = 40.0;
/// 引擎每 4 秒报一次编码会话。超过这个岁数就当没读到——引擎挂了要退回「测不出来」
/// （放行），而不是抱着最后一份读数一直当真：那份读数说的是它挂掉之前那一刻的事。
const ENCODER_TTL_MS: u128 = 15_000;

/// 比 1080p 大 = 电脑（手边有键盘），不大于 = 平板/手机。判据是**虚拟显示器的
/// 尺寸**：推流时 GameViewer 会按客户端造一块虚拟屏，主机自己那几路输出全部置为
/// inactive。同一台主机上实测——iPad（面板 2420×1668）→ 虚拟屏 1920×1080 @175%；
/// 手机（面板 2796×1290）→ 1920×1080；这台电脑 → 3840×2160 @144Hz。
///
/// 已知误判：1366×768 那类低分辨率笔记本连过来，虚拟屏同样是 1920×1080，会被
/// 当成平板，图标多出现一个。这是有意选的方向，和这一段其余地方一致。
const CLIENT_DESKTOP_W: u32 = 1920;
const CLIENT_DESKTOP_H: u32 = 1080;
/// 逻辑尺寸乘回缩放会差零点几个像素（1097 × 1.75 = 1919.75），留一点余量。
/// 余量给小了就会把 iPad 判成电脑 = 图标消失，那是错得最狠的方向。
const CLIENT_SIZE_SLACK: u32 = 16;

fn log_dir() -> std::path::PathBuf {
    let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
    std::path::Path::new(&pf)
        .join("Netease")
        .join("GameViewer")
        .join("log")
        .join("server")
        .join("log")
}

// ── 状态 ──────────────────────────────────────────────────────────

#[derive(Clone, Default, PartialEq)]
struct Foreground {
    pid: u32,
    proc: String,
    title: String,
}

#[derive(Clone, Copy)]
struct Sample {
    at: Instant,
    size: u64,
}

#[derive(Default)]
struct State {
    /// 「仅在这几个程序显示」的关键词，已小写去重。空 = 不过滤。
    mic_apps: Vec<String>,
    remote_only: bool,
    /// None = 用户此刻在哪个窗口还没测出来（监听没跑，或者刚停）。
    last_foreground: Option<Foreground>,
    /// None = 没测出来。这三个 None 都朝「显示」倒。
    remote_active: Option<bool>,
    client_touch: Option<bool>,
    /// 药丸那条「此刻该录哪支麦克风」推过没有。None = 没推过：药丸重载后要从头
    /// 再推一次，所以不能拿 false 当初值。
    remote_pushed: Option<bool>,
    visible: bool,
    samples: Vec<Sample>,
    sample_file: Option<String>,
    /// 只用来给 remote-nvenc / remote-rate 那行日志去重，不参与任何判定。
    last_active_logged: Option<bool>,
    /// 显卡上谁在用 NVENC 编码视频，由引擎经药丸那条 ws 转进来。
    /// None = 问不出来（这台机器没有 N 卡、或者引擎没连上），**不是**「没人在编码」。
    encoder_pids: Option<(Vec<u32>, Instant)>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static FG_RUNNING: AtomicBool = AtomicBool::new(false);
static REMOTE_RUNNING: AtomicBool = AtomicBool::new(false);

fn state() -> parking_lot::MappedMutexGuard<'static, State> {
    parking_lot::MutexGuard::map(STATE.lock(), |s| s.get_or_insert_with(State::default))
}

// ── 关键词 ────────────────────────────────────────────────────────

/// 逐字对应 Electron 的 normalizeMicApps：逗号（中英）、顿号、换行都算分隔，
/// 去空白、转小写、丢掉超过 64 个字的，去重后最多 32 条。
fn normalize_mic_apps(v: Option<&serde_json::Value>) -> Vec<String> {
    let raw = match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .map(|x| x.as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>()
            .join(","),
        // 那边 typeof 'object' 直接返回 []，数字/布尔走 String(value)。
        Some(serde_json::Value::Object(_)) => return Vec::new(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    };
    let mut out: Vec<String> = Vec::new();
    for part in raw.split(|c| matches!(c, ',' | '，' | '、' | '\n' | '\r')) {
        let p = part.trim().to_lowercase();
        if p.is_empty() || p.chars().count() > 64 {
            continue;
        }
        if !out.contains(&p) {
            out.push(p);
        }
        if out.len() >= 32 {
            break;
        }
    }
    out
}

/// 关键词命中进程名或窗口标题就算数。没设关键词、或者还没测出前台是谁，一律放行。
fn matches_mic_apps(apps: &[String], fg: Option<&Foreground>) -> bool {
    if apps.is_empty() {
        return true;
    }
    let Some(fg) = fg else { return true };
    let proc = fg.proc.to_lowercase();
    let title = fg.title.to_lowercase();
    apps.iter()
        .any(|k| (!proc.is_empty() && proc.contains(k)) || (!title.is_empty() && title.contains(k)))
}

// ── 前台窗口 ──────────────────────────────────────────────────────

/// 那段 C# 里真正干活的四个调用。进程名只在 hwnd/pid 真变了的时候才解析——
/// 它是这一圈里唯一一次真正的系统调用，450ms 一次扫三个窗口字段是 0 开销。
fn read_foreground(cache: &mut (isize, u32, String)) -> Option<Foreground> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
    };

    unsafe {
        let h = GetForegroundWindow();
        let mut pid: u32 = 0;
        if !h.is_invalid() {
            GetWindowThreadProcessId(h, Some(&mut pid));
        }

        let raw = h.0 as isize;
        let proc = if raw == cache.0 && pid == cache.1 {
            cache.2.clone()
        } else {
            proc_name(pid)
        };

        // GetWindowText 读别家窗口时不会把 WM_GETTEXT 派到对方线程，所以卡死的
        // 程序也阻塞不了我们。
        let title = if h.is_invalid() {
            String::new()
        } else {
            let n = GetWindowTextLengthW(h);
            if n <= 0 {
                String::new()
            } else {
                let mut buf = vec![0u16; n as usize + 1];
                let got = GetWindowTextW(h, &mut buf);
                String::from_utf16_lossy(&buf[..got.max(0) as usize])
            }
        };

        *cache = (raw, pid, proc.clone());


        // 自家窗口报的就是自己的 pid。直接忽略、不记录：last_foreground 保持指向
        // 用户打开设置之前所在的程序，改完关键词才能立刻按那个程序判断。
        // 药丸和悬浮麦是 WS_EX_NOACTIVATE，压根不会进前台。
        if pid == std::process::id() {
            return None;
        }

        fn proc_name(pid: u32) -> String {
            if pid == 0 {
                return String::new();
            }
            unsafe {
                let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                    return String::new();
                };
                let mut buf = vec![0u16; 1024];
                let mut n = buf.len() as u32;
                let ok =
                    QueryFullProcessImageNameW(h, PROCESS_NAME_FORMAT(0), windows::core::PWSTR(buf.as_mut_ptr()), &mut n)
                        .is_ok();
                let _ = CloseHandle(h);
                if !ok {
                    return String::new();
                }
                let full = String::from_utf16_lossy(&buf[..n as usize]);
                // 那边是 Path.GetFileNameWithoutExtension，所以 notepad.exe → notepad。
                std::path::Path::new(&full)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            }
        }

        Some(Foreground { pid, proc, title })
    }
}

fn start_foreground(app: &AppHandle) {
    // 幂等：设置页 180ms 防抖会连着调好几次。
    if FG_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let mut cache = (0isize, 0u32, String::new());
        let mut last: Option<Foreground> = None;
        while FG_RUNNING.load(Ordering::SeqCst) {
            let now = read_foreground(&mut cache);
            // 只在真变了的时候才动状态，稳态下这一圈什么都不做。
            if now.is_some() && now != last {
                last = now.clone();
                state().last_foreground = now;
                apply(&app);
            }
            std::thread::sleep(Duration::from_millis(FG_POLL_MS));
        }
    });
}

fn stop_foreground() {
    FG_RUNNING.store(false, Ordering::SeqCst);
    state().last_foreground = None;
}

// ── UU 远程（网易 GameViewer）会话探测 ────────────────────────────
//
// Windows 的 SM_REMOTESESSION 对它无效：GameViewer 推的是控制台会话，那个标志只
// 认终端服务（RDP），实测连着的时候也是 0。进程和服务常驻，连不连都在；它到网易
// 服务器的几条 443 长连接同样常驻——这些都不能当判据。

struct LogFile {
    name: String,
    size: u64,
    mtime: std::time::SystemTime,
}

/// GameViewer 2026-09-09 把日志从明文 .txt 换成了二进制 .slog，文件名也从
/// streamer_log_* 变成 streamer_log_controlled_*。只认 .txt 的话，探测器看到的是
/// 09-08 就冻结不动的旧文件：速率恒为 0，永远判「没在推流」，于是手机、平板上
/// 图标彻底不出现——正是这个功能最不能错的方向。两种都收，谁新用谁。
fn is_streamer_log(name: &str) -> bool {
    name.starts_with("streamer_log_") && (name.ends_with(".txt") || name.ends_with(".slog"))
}

/// 日志名末尾就是写它的那个 GameViewerServer 的 pid，例如
/// streamer_log_controlled_20260910105955987_6056.slog → 6056。有了它，
/// 「显卡上那路编码是不是 GameViewer 开的」就只是一次整数比较。
fn streamer_log_pid(name: &str) -> Option<u32> {
    let stem = name.rsplit_once('.')?.0;
    stem.rsplit_once('_')?.1.parse().ok()
}

fn newest_streamer_log() -> Option<LogFile> {
    // 日志名里带 GameViewerServer 的启动时间和 PID，它一重启就换一份，
    // 所以每次都重新挑最新的，不能把文件名缓存下来。
    let mut best: Option<LogFile> = None;
    for e in std::fs::read_dir(log_dir()).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !is_streamer_log(&name) {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        let Ok(mtime) = md.modified() else { continue };
        if best.as_ref().map_or(true, |b| mtime > b.mtime) {
            best = Some(LogFile { name, size: md.len(), mtime });
        }
    }
    best
}

fn fresh_encoder_pids(st: &State) -> Option<Vec<u32>> {
    let (pids, at) = st.encoder_pids.as_ref()?;
    (at.elapsed().as_millis() <= ENCODER_TTL_MS).then(|| pids.clone())
}

fn detect_remote_session(st: &mut State) -> Option<bool> {
    // 没装 GameViewer，或者目录读不到。
    let Some(cur) = newest_streamer_log() else {
        st.samples.clear();
        st.sample_file = None;
        return None;
    };
    let now = Instant::now();

    // 换了日志文件（服务重启或轮转）或者文件被截短：旧样本作废，重新攒。
    let truncated = st.samples.last().map_or(false, |l| cur.size < l.size);
    if st.sample_file.as_deref() != Some(cur.name.as_str()) || truncated {
        st.sample_file = Some(cur.name.clone());
        st.samples.clear();
    }
    // 样本无条件攒着：编码会话那条路随时可能断（引擎没连上、这台机器没有 N 卡），
    // 那时旧格式日志还得靠下面的速率退路，手上不能是空的。
    st.samples.push(Sample { at: now, size: cur.size });
    while st.samples.len() > 1
        && now.duration_since(st.samples[0].at).as_millis() > REMOTE_WINDOW_MS
    {
        st.samples.remove(0);
    }

    // 首选判据：显卡上有人在编码视频，而且就是写这份日志的那个 GameViewerServer。
    if let (Some(pid), Some(pids)) = (streamer_log_pid(&cur.name), fresh_encoder_pids(st)) {
        let active = pids.contains(&pid);
        // 只在结论翻转时写，免得 3 秒一行把日志刷爆。
        if Some(active) != st.last_active_logged {
            st.last_active_logged = Some(active);
            let list: Vec<String> = pids.iter().map(|p| p.to_string()).collect();
            eprintln!(
                "[远程] nvenc pid {pid} in [{}]? {active}（{}）",
                list.join(","),
                cur.name
            );
        }
        return Some(active);
    }

    // 退路：只有旧的明文 .txt 日志才能靠增长速率判，理由见 REMOTE_MIN_RATE_TXT。
    // 新格式问不到编码会话就认输返回 None——猜一个只会两头都错，而错的那一边是
    // 「远程时图标没了」。
    if !cur.name.ends_with(".txt") {
        return None;
    }
    let first = *st.samples.first()?;
    let span = now.duration_since(first.at).as_millis();
    if span < REMOTE_MIN_SPAN_MS {
        return None;
    }
    let rate = (cur.size.saturating_sub(first.size)) as f64 / (span as f64 / 1000.0);
    let active = rate >= REMOTE_MIN_RATE_TXT;
    if Some(active) != st.last_active_logged {
        st.last_active_logged = Some(active);
        eprintln!(
            "[远程] 速率 {rate:.1} B/s >= {REMOTE_MIN_RATE_TXT}? {active}（{}）",
            cur.name
        );
    }
    Some(active)
}

/// 客户端是不是触屏设备。「在推流」还不够：从另一台电脑连过来时手边有键盘，
/// 图标一样是碍事的。真正要的是「客户端没有实体键盘」。
///
/// Electron 那边拿的是 display.size（逻辑）再乘回 scaleFactor 才是真实像素；
/// Tauri 的 monitor.size() 本来就是物理像素，所以这里不再乘——乘了的话每一台
/// 客户端看着都像电脑，图标会在最需要它的时候消失。
fn client_looks_desktop(app: &AppHandle) -> Option<bool> {
    let m = app.primary_monitor().ok().flatten()?;
    let s = m.size();
    // 显示器正在切换的那一瞬可能读到 0，别拿它下结论。
    if s.width == 0 || s.height == 0 {
        return None;
    }
    Some(s.width > CLIENT_DESKTOP_W + CLIENT_SIZE_SLACK || s.height > CLIENT_DESKTOP_H + CLIENT_SIZE_SLACK)
}

fn start_remote(app: &AppHandle) {
    if REMOTE_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        while REMOTE_RUNNING.load(Ordering::SeqCst) {
            // 客户端尺寸每轮都重读：换设备时显示器事件不保证每次都到，
            // 漏一次就会一直抱着上一台客户端的结论。
            let touch = client_looks_desktop(&app).map(|d| !d);
            let mut changed = false;
            {
                let mut st = state();
                if st.client_touch != touch {
                    st.client_touch = touch;
                    changed = true;
                    eprintln!(
                        "[远程] 客户端 {}",
                        match touch {
                            None => "测不出来",
                            Some(true) => "触屏",
                            Some(false) => "电脑",
                        }
                    );
                }
                let next = detect_remote_session(&mut st);
                if st.remote_active != next {
                    st.remote_active = next;
                    changed = true;
                }
            }
            if changed {
                apply(&app);
            }
            std::thread::sleep(Duration::from_millis(REMOTE_POLL_MS));
        }
    });
}

fn stop_remote() {
    REMOTE_RUNNING.store(false, Ordering::SeqCst);
    let mut st = state();
    st.remote_active = None;
    st.client_touch = None;
    st.samples.clear();
    st.sample_file = None;
    st.last_active_logged = None;
}

// ── 两关合一 ──────────────────────────────────────────────────────

/// 远程限定这一关。两道判据都只在「明确测出不该显示」时才拦，None（测不出来）
/// 放行：主机上多一个图标只是碍眼，远程时图标没了却是功能缺失。
fn passes_remote_only(st: &State) -> bool {
    if !st.remote_only {
        return true;
    }
    if st.remote_active == Some(false) {
        return false; // 没在推流 = 人就坐在主机前
    }
    st.client_touch != Some(false) // 明确是电脑客户端才拦，手边有键盘
}

/// 药丸那条渲染进程只认一件事：此刻该录哪支麦克风。人在远程那头就录 UU 那支虚拟
/// 麦克风，人坐在这台机器前就录桌上那支实体麦克风。
///
/// 监测没在跑就没有判据（「仅在远程时显示」关着，或者悬浮麦整个关掉）。那时一律
/// 当本机：宁可不自动换，也不能凭空把录音切到一支只送零采样的设备上。
fn wants_remote(st: &State) -> bool {
    REMOTE_RUNNING.load(Ordering::SeqCst) && passes_remote_only(st)
}

/// 显示/隐藏的唯一入口，幂等。
pub fn apply(app: &AppHandle) {
    let (remote, want) = {
        let st = state();
        // 图标的显隐是两半判据凑出来的，麦克风只跟远程那一半走。
        //
        // 另一半是「只在这几个程序里显示」——那问的是你此刻在哪个窗口打字，跟你人
        // 在哪儿说话没关系。图标为它藏起来的那些时候，人照样在远程那头，要是跟着
        // 换回桌上那支实体麦克风，录下来就又是一整段房间的空气声。
        (
            wants_remote(&st),
            matches_mic_apps(&st.mic_apps, st.last_foreground.as_ref()) && passes_remote_only(&st),
        )
    };
    push_remote_mic(app, remote);

    let Some(w) = app.get_webview_window(crate::micwin::LABEL) else { return };
    let shown = w.is_visible().unwrap_or(false);
    {
        let mut st = state();
        if st.visible == want && shown == want {
            return;
        }
        st.visible = want;
        // 「图标不见了」有两种：判据说不该显示，和覆盖层丢了置顶变成隐身。
        // 后者不发任何事件，只能靠「这里记了没记」把两种分开。
        let fg = st.last_foreground.clone().unwrap_or_default();
        eprintln!(
            "[悬浮麦] {} · 前台 {}/{} · 远程 {:?} · 客户端触屏 {:?}",
            if want { "显示" } else { "隐藏" },
            if fg.proc.is_empty() { "?" } else { &fg.proc },
            fg.title.chars().take(20).collect::<String>(),
            st.remote_active,
            st.client_touch
        );
    }
    // hide()/show() 而不是销毁重建：webview 活着，录音状态照常送达，进行中的录音
    // 也不会被掐断。窗口带 WS_EX_NOACTIVATE，show() 不会抢焦点（对应那边的
    // showInactive——用 show() 会抬升窗口，反过来触发前台事件造成震荡）。
    if want {
        let _ = w.show();
        // 全屏点击穿透的覆盖层一旦丢了置顶就等于隐身：图标照样画出来，却被压在
        // 当前活动窗口后面，而且掉了**不发任何事件**。每次显示前重申一次。
        let _ = w.set_always_on_top(true);
    } else {
        let _ = w.hide();
    }
}

/// 带去重的推送。null = 没推过——药丸重载后要从头再推一次。
fn push_remote_mic(app: &AppHandle, on: bool) {
    {
        let mut st = state();
        if st.remote_pushed == Some(on) {
            return;
        }
        st.remote_pushed = Some(on);
    }
    eprintln!(
        "[远程] 录音设备 {}",
        if on { "判为远程，录 UU 虚拟麦克风" } else { "判为本机，录实体麦克风" }
    );
    let _ = app.emit_to(crate::BAR, "localless://remote-session", on);
}

/// 设置变了：该起的监测起来，该停的停掉，然后重算一次显隐。
/// micwin::sync 把窗口建好/关掉之后调它。
pub fn sync(app: &AppHandle) {
    let s = crate::settings::read();
    let enabled = s.get("floatingMicEnabled").and_then(|v| v.as_bool()).unwrap_or(false);
    let apps = normalize_mic_apps(s.get("floatingMicApps"));
    // 和 Electron 一样：缺这个键当开着。这一项默认打开，漏读成 false 就是
    // 图标在主机上也常驻。
    let remote_only = s.get("floatingMicRemoteOnly").and_then(|v| v.as_bool()).unwrap_or(true);

    // 守卫在这个块里就还回去：下面 stop_foreground / stop_remote 还要再拿一次，
    // parking_lot 的 Mutex 不可重入，拿着不放就是自己锁死自己。
    let filtering = {
        let mut st = state();
        st.mic_apps = apps;
        st.remote_only = remote_only;
        !st.mic_apps.is_empty()
    };

    // 悬浮麦整个关着的时候两个监测都不必跑。
    if enabled && filtering {
        start_foreground(app);
    } else {
        stop_foreground();
    }
    if enabled && remote_only {
        start_remote(app);
    } else {
        stop_remote();
    }

    // 关掉的时候不走 apply：叫到这里时 micwin 刚请求关窗，而关窗是排队执行的，
    // get_webview_window 这一刻还拿得到那个已经在路上的窗口。apply 于是会对着它
    // show() 一次，并往日志里记一行「显示」——紧跟在「已关闭」后面，读日志的人
    // 只会以为关不掉。顺手把 visible 归位，下次开窗时状态是干净的。
    if !enabled {
        state().visible = false;
        return;
    }
    apply(app);
}

// ── 引擎那条缝 ────────────────────────────────────────────────────

/// 显卡上谁在用 NVENC 编码，由引擎读 NVML 之后经药丸那条 ws 转进来
/// （对应 Electron 的 localless:encoder-pids）。
///
/// `None` 和 `Some(vec![])` 是两件完全不同的事，所以参数必须可空：前者是「这一刻
/// 问不出来」（引擎断了、旧引擎没这个字段），远程判据要退回「测不出来」＝放行；
/// 后者是「问出来了，没人在编码」＝不是远程。收成 `Vec<u32>` 的话，药丸发 null
/// 会在反序列化那一步直接失败，于是引擎重启期间这边一直抱着最后一份读数当真——
/// 正好是用户最需要那个图标的时候。
#[tauri::command]
pub fn encoder_pids(app: AppHandle, pids: Option<Vec<u32>>) {
    state().encoder_pids = pids.map(|p| (p, Instant::now()));
    apply(&app);
}

/// 药丸起来得比主进程晚，而上面那条推送是带去重的——开机时那一条往往发给了一个
/// 还没装上监听器的频道，之后判据不变就再也不会补。让药丸装好监听器后自己来问
/// 一次，推送只负责往后的变化。对应 Electron 的 localless:remote-mic-now。
#[tauri::command]
pub fn remote_mic_now() -> bool {
    wants_remote(&state())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fg(proc: &str, title: &str) -> Foreground {
        Foreground { pid: 1, proc: proc.into(), title: title.into() }
    }

    #[test]
    fn 关键词按那边的规矩切() {
        let v = json!(" Chrome ,， code、\nVSCode\r,chrome,");
        assert_eq!(normalize_mic_apps(Some(&v)), vec!["chrome", "code", "vscode"]);
        // 数组会先 join 成一串再切，和 Array.isArray 那一支一致。
        assert_eq!(normalize_mic_apps(Some(&json!(["A", "b"]))), vec!["a", "b"]);
        // 对象那一支直接空，别把 {} 当成一条关键词去匹配所有窗口。
        assert!(normalize_mic_apps(Some(&json!({ "a": 1 }))).is_empty());
        assert!(normalize_mic_apps(None).is_empty());
        // 超过 64 个字的丢掉。
        assert!(normalize_mic_apps(Some(&json!("x".repeat(65)))).is_empty());
        assert_eq!(normalize_mic_apps(Some(&json!("x".repeat(64)))).len(), 1);
    }

    #[test]
    fn 关键词上限三十二条() {
        let many = (0..40).map(|i| format!("app{i}")).collect::<Vec<_>>().join(",");
        assert_eq!(normalize_mic_apps(Some(&json!(many))).len(), 32);
    }

    #[test]
    fn 没关键词或没测出前台一律放行() {
        assert!(matches_mic_apps(&[], Some(&fg("notepad", "无关"))));
        assert!(matches_mic_apps(&["chrome".into()], None));
    }

    #[test]
    fn 进程名和标题都算命中() {
        let apps = vec!["chrome".into(), "记事本".into()];
        assert!(matches_mic_apps(&apps, Some(&fg("chrome", "随便"))));
        assert!(matches_mic_apps(&apps, Some(&fg("explorer", "我的记事本"))));
        assert!(!matches_mic_apps(&apps, Some(&fg("explorer", "资源管理器"))));
        // 大小写不敏感：关键词已小写，两边都要 to_lowercase。
        assert!(matches_mic_apps(&apps, Some(&fg("Chrome", ""))));
    }

    #[test]
    fn 日志文件名认得出pid() {
        assert_eq!(
            streamer_log_pid("streamer_log_controlled_20260910105955987_6056.slog"),
            Some(6056)
        );
        assert_eq!(streamer_log_pid("streamer_log_20260908_12.txt"), Some(12));
        assert_eq!(streamer_log_pid("streamer_log_nopid.slog"), None);
        assert!(is_streamer_log("streamer_log_controlled_1_2.slog"));
        assert!(is_streamer_log("streamer_log_1.txt"));
        assert!(!is_streamer_log("other_log_1.txt"));
        assert!(!is_streamer_log("streamer_log_1.zip"));
    }

    /// 倒向：只有「明确测出不该显示」才拦。三个 None 全部放行。
    #[test]
    fn 测不出来一律放行() {
        let mut st = State { remote_only: true, ..Default::default() };
        assert!(passes_remote_only(&st), "两边都没测出来时必须显示");

        st.client_touch = Some(false);
        assert!(!passes_remote_only(&st), "明确是电脑客户端才该拦");

        st.client_touch = Some(true);
        assert!(passes_remote_only(&st));

        st.remote_active = Some(false);
        assert!(!passes_remote_only(&st), "没在推流 = 人坐在主机前");

        // 关掉这一关就永远放行，不看另外两个。
        st.remote_only = false;
        assert!(passes_remote_only(&st));
    }

    #[test]
    fn 编码会话过期就当没读到() {
        let mut st = State::default();
        assert!(fresh_encoder_pids(&st).is_none(), "从没报过 = 问不出来");
        st.encoder_pids = Some((vec![6056], Instant::now()));
        assert_eq!(fresh_encoder_pids(&st), Some(vec![6056]));
        st.encoder_pids = Some((
            vec![6056],
            Instant::now() - Duration::from_millis(ENCODER_TTL_MS as u64 + 1),
        ));
        assert!(fresh_encoder_pids(&st).is_none(), "引擎挂了要退回测不出来，不能抱着旧读数");
    }
}

// 全局按键钩子。搬自 Electron 版 main.js:824-877（readTrigger / killOrphanScripts /
// startKeyHook）和 936-944（设置改完之后的重启）。
//
// 钩子本体还是 app/keyhook-localless.ps1 那份，一个字没动：它在进程里现编译一段
// C#，挂 WH_KEYBOARD_LL，把每一下按键当 JSON 行吐到 stdout。这边只负责起它、
// 读它、按触发键决定要不要切换录音。
//
// ── 为什么不在 Rust 里自己挂钩子 ──────────────────────────────────
//
// 能挂，但低级键盘钩子的回调必须跑在一个**有消息循环的线程**上，而且这个回调
// 是全系统输入的串行瓶颈：它慢一点，整台机器的按键就跟着卡。Tauri 的主线程上
// 已经有窗口的消息循环，把钩子挂上去等于让每一下按键都排在窗口事件后面。要做
// 对就得自己另起一个专用线程再跑一套消息循环，那跟现在这个独立进程的区别只是
// 少了一次进程边界——却把「钩子卡死连累整个应用」变成了可能。留在外面更安全。
//
// 这条路唯一的代价是 PowerShell 起来慢（现编译 C# 要 ~800ms），但它只在启动和
// 改快捷键时各付一次。
//
// ── 自家注入的按键不会回流 ────────────────────────────────────────
//
// 粘贴那下 Shift+Insert 也会经过这个钩子。脚本靠 dwExtraInfo 上的 'LLST' 戳认出
// 自家的键直接放行，而我们这边发键的 win::key 就盖着同一个戳（win.rs:8）。
// 两处值必须一致——不一致的表现是钩子把我们自己发的键当用户敲的，在回调里做
// 阻塞 I/O，全机输入跟着卡。

use parking_lot::Mutex;
use std::collections::HashSet;
use std::io::BufRead;
use std::sync::atomic::{AtomicU64, Ordering};
use tauri::AppHandle;

/// 只有这两个单键值得吞。理由在脚本里：按住 RightAlt 时系统会把 Alt 状态喂给
/// 鼠标左键（Alt+左键=拖拽），表现就是「左键失灵」。吞掉之后系统永远看不见
/// 这一下 Alt，左键正常，录音事件照发。
///
/// 带修饰键的组合一律不吞：吞了的话 Ctrl+Shift+D 这种组合里的 D 到不了目标
/// 程序，用户在别处的快捷键就废了。
fn swallow_vk(key: &str) -> i32 {
    match key {
        "RightAlt" => 0xA5,
        "RightCtrl" => 0xA3,
        _ => 0,
    }
}

/// 一个修饰键名对应的实际键。左右两边哪个都算数。
fn mod_keys(name: &str) -> &'static [&'static str] {
    match name {
        "Ctrl" => &["LeftCtrl", "RightCtrl"],
        "Alt" => &["LeftAlt", "RightAlt"],
        "Shift" => &["LeftShift", "RightShift"],
        "Meta" => &["LeftCmd", "RightCmd"],
        _ => &[],
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Trigger {
    pub key: String,
    pub mods: Vec<String>,
}

impl Trigger {
    /// 按下的键里凑齐了所有修饰键没有。
    fn mods_ok(&self, down: &HashSet<String>) -> bool {
        self.mods.iter().all(|m| {
            let alts = mod_keys(m);
            if alts.is_empty() {
                down.contains(m)
            } else {
                alts.iter().any(|k| down.contains(*k))
            }
        })
    }
}

/// 设置里的 dictationShortcut，形如 "RightAlt" / "Ctrl+Shift+D"。
/// 最后一段是主键，前面的都是修饰键。
fn parse_trigger(combo: &str) -> Trigger {
    let mut parts: Vec<String> = combo
        .split('+')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let key = parts.pop().unwrap_or_else(|| "RightAlt".into());
    Trigger { key, mods: parts }
}

fn read_trigger() -> Trigger {
    let s = crate::settings::read();
    let combo = s
        .get("dictationShortcut")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("RightAlt")
        .to_string();
    parse_trigger(&combo)
}

/// 清掉所有还挂着的旧钩子进程。
///
/// taskkill /F 杀主进程不会走退出回调，spawn 出去的 sidecar 会变成孤儿继续跑
/// （钩子会一直全局吞 RightAlt，下次启动就有两个钩子抢同一个键 → 表现为
/// 「按 Alt 没反应」）。起新进程前先按脚本文件名清干净。
///
/// 匹配 '-File ...<脚本名>'：sidecar 都是 -File 起的，而这条查询本身是 -Command
/// 起的，不会自误杀；机器上别的 powershell 也不会被误伤。
///
/// **参数类型是 `&'static str` 不是 `&str`，这是故意的**：它会被拼进 PowerShell
/// 字符串，只能传本文件里硬编码的常量，绝不能是设置值或任何外部输入。
/// 编译器替我们守着这一条。
fn kill_orphan_scripts(script_file_name: &'static str) {
    use std::os::windows::process::CommandExt;
    let script = format!(
        "Get-CimInstance Win32_Process -Filter \"Name='powershell.exe'\" | \
         Where-Object {{ $_.CommandLine -like '*-File*{script_file_name}*' }} | \
         ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}"
    );
    let _ = std::process::Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &script])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

const SCRIPT: &str = "keyhook-localless.ps1";

static CHILD: Mutex<Option<std::process::Child>> = Mutex::new(None);
static CURRENT: Mutex<Option<Trigger>> = Mutex::new(None);
/// 代次。每起一次加一；读取线程只有在代次还是自己那一代时才认事件。
/// 少了它，改快捷键那一下旧进程的最后几行还会触发一次录音。
static GEN: AtomicU64 = AtomicU64::new(0);

pub fn start(app: AppHandle) {
    let trigger = read_trigger();
    *CURRENT.lock() = Some(trigger.clone());

    kill_orphan_scripts(SCRIPT);

    let script = crate::models::root().join("app").join(SCRIPT);
    if !script.is_file() {
        crate::debug::log(&format!("hook 脚本不在：{}", script.display()));
        return;
    }
    let swallow = if trigger.mods.is_empty() { swallow_vk(&trigger.key) } else { 0 };

    use std::os::windows::process::CommandExt;
    let child = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            &script.to_string_lossy(),
            "-SwallowVk",
            &swallow.to_string(),
        ])
        .creation_flags(0x0800_0000)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            crate::debug::log(&format!("hook spawn 失败 {e}"));
            return;
        }
    };
    let pid = child.id();
    let stdout = child.stdout.take();
    let gen = GEN.fetch_add(1, Ordering::SeqCst) + 1;
    *CHILD.lock() = Some(child);
    crate::debug::log(&format!(
        "hook spawned pid={pid} trigger={}+{} swallow={swallow}",
        trigger.mods.join("+"),
        trigger.key
    ));

    let Some(stdout) = stdout else { return };
    std::thread::spawn(move || {
        // 按住不放时钩子会连发 keydown（系统的自动重复），所以「已经按下了」
        // 这件事必须记在集合里而不是计数——不然抬起一次减不完。
        let mut down: HashSet<String> = HashSet::new();
        let reader = std::io::BufReader::new(stdout);
        for line in reader.lines() {
            if GEN.load(Ordering::SeqCst) != gen {
                return; // 已经被换掉了，剩下的行一概不算数
            }
            let Ok(line) = line else { break };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // 坏行直接跳过。这条流一旦因为一行坏 JSON 断掉，快捷键就整个失灵，
            // 而且不报错——按了没反应是这个应用最难查的一类故障。
            let Ok(ev) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let Some(key) = ev.get("key").and_then(|v| v.as_str()) else { continue };
            let is_down = ev.get("down").and_then(|v| v.as_bool()).unwrap_or(false);
            if is_down {
                down.insert(key.to_string());
            } else {
                down.remove(key);
            }
            if key != trigger.key {
                continue;
            }
            let ok = trigger.mods_ok(&down);
            crate::debug::log(&format!("trigger key seen down={is_down} modsOk={ok}"));
            if is_down && ok {
                crate::recorder::toggle(&app, "shortcut");
            }
        }
        if GEN.load(Ordering::SeqCst) == gen {
            crate::debug::log("hook 的输出断了");
        }
    });
}

/// 设置改完之后。只有快捷键真的变了才重起——重起一次要付 ~800ms 的 PowerShell
/// 编译钱，而设置页每改一个开关都会叫这里一次。
pub fn restart_if_changed(app: &AppHandle) {
    let next = read_trigger();
    if CURRENT.lock().as_ref() == Some(&next) {
        return;
    }
    crate::debug::log(&format!("快捷键变了 → {}+{}", next.mods.join("+"), next.key));
    stop();
    start(app.clone());
}

/// 设置页录新触发键的那几秒，把钩子摘掉。
///
/// 钩子对单键触发是**吞**的（`swallow_vk("RightAlt") == 0xA5`），所以录键的时候
/// 按 RightAlt，这一下会被钩子在系统层截走，WebView2 的 keydown 永远不触发——
/// 表现成「触发按键设不了」：按默认那个键没反应，按字母又被「该按键不可用」挡回去，
/// 两条路都堵死，界面上没有任何东西提示是自己把自己的键吃了。
///
/// `on=false` 时直接 `start`，读的是设置里当前的值；就算页面还没把新键存完，
/// 随后的 `settings_changed` → `restart_if_changed` 也会再纠正一次。
#[tauri::command]
pub fn hook_capture(app: AppHandle, on: bool) {
    crate::debug::log(if on { "录触发键：摘钩子" } else { "录触发键：装回钩子" });
    if on {
        stop();
    } else {
        stop();
        start(app);
    }
}

/// 收摊。不杀的话这个 PowerShell 会一直挂着全局吞 RightAlt——下次启动多一个
/// 钩子抢同一个键，表现成「按 Alt 没反应」。
pub fn stop() {
    GEN.fetch_add(1, Ordering::SeqCst);
    if let Some(mut c) = CHILD.lock().take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(keys: &[&str]) -> HashSet<String> {
        keys.iter().map(|s| s.to_string()).collect()
    }

    /// 单键、组合键、多余的空格和加号。
    #[test]
    fn 触发键的解析() {
        assert_eq!(parse_trigger("RightAlt"), Trigger { key: "RightAlt".into(), mods: vec![] });
        assert_eq!(
            parse_trigger("Ctrl+Shift+D"),
            Trigger { key: "D".into(), mods: vec!["Ctrl".into(), "Shift".into()] }
        );
        assert_eq!(
            parse_trigger(" Ctrl + D "),
            Trigger { key: "D".into(), mods: vec!["Ctrl".into()] }
        );
        // 空串退回默认，不能解析成空主键——那样任何一下按键都会命中。
        assert_eq!(parse_trigger("+++"), Trigger { key: "RightAlt".into(), mods: vec![] });
    }

    /// 修饰键左右两边哪个都算数。
    #[test]
    fn 左右修饰键都认() {
        let t = parse_trigger("Ctrl+D");
        assert!(t.mods_ok(&set(&["LeftCtrl"])));
        assert!(t.mods_ok(&set(&["RightCtrl"])));
        assert!(!t.mods_ok(&set(&["LeftShift"])));
    }

    /// 修饰键要齐。缺一个就不能触发，否则 Ctrl+Shift+D 会被单独的 Ctrl+D 顶了。
    #[test]
    fn 修饰键要齐() {
        let t = parse_trigger("Ctrl+Shift+D");
        assert!(!t.mods_ok(&set(&["LeftCtrl"])));
        assert!(t.mods_ok(&set(&["LeftCtrl", "RightShift"])));
    }

    /// 没有修饰键的触发键，按下一堆别的键也照样算数——用户按住 Shift 打字时
    /// 顺手按 RightAlt 也该能开录音。
    #[test]
    fn 单键不管别的键按没按() {
        let t = parse_trigger("RightAlt");
        assert!(t.mods_ok(&set(&[])));
        assert!(t.mods_ok(&set(&["LeftShift", "LeftCtrl"])));
    }

    /// 只有不带修饰键的那两个单键才吞。带修饰键的组合吞了会把用户在别处的
    /// 快捷键一起废掉。
    #[test]
    fn 只吞那两个单键() {
        assert_eq!(swallow_vk("RightAlt"), 0xA5);
        assert_eq!(swallow_vk("RightCtrl"), 0xA3);
        assert_eq!(swallow_vk("D"), 0);
        assert_eq!(swallow_vk("LeftAlt"), 0);
    }
}

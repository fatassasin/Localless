// 托盘。搬自 Electron 版 main.js:1034-1104（deviceMenuLabel / refreshTrayMenu /
// syncTrayIcon / createTray）、254-273（那两个开关）、233-247（回传设置页）。
//
// 两处换了做法：
//
// 1. 图标编进二进制，不按路径读。Electron 那边 appIconPath 会在文件不存在时
//    悄悄退回 createEmpty()——一个看不见的托盘图标，谁也不会想到去查。那个坑
//    真踩过一次（图标本来在 renderer/ 下，而 renderer/ 整个在 .gitignore 里，
//    换台机器克隆下来就是空的）。include_bytes! 把这种失败挪到编译期：文件不在
//    就编不过，不会跑起来之后才发现图标没了。
// 2. 菜单项原地改，不整份重建。Electron 不支持改已挂上去的菜单项，所以每次
//    勾选态或档位一变就得 setContextMenu 一整份新的——用户正打开着菜单的话，
//    那份菜单会被当场换掉。Tauri 能 set_checked/set_text，于是这个代价不存在。
//
// 判据本身一条没改，包括那两条最容易被"顺手改对"的：
// - 勾选态跟着**设置**走，档位标签跟着**引擎实际**走。两者可以不一致，而不一致
//   的那一刻恰恰是用户最需要看见真相的时候。
// - 托盘的"显示悬浮麦克风"写的就是设置页那同一个键，不另开一个临时状态。

use once_cell::sync::OnceCell;
use serde_json::{Map, Value};
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, CheckMenuItemBuilder, MenuBuilder, MenuItem, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Wry};

const TRAY_ID: &str = "localless";

/// icon.ico 是绿那张的副本，兼作兜底：桌面快捷方式认的也是这个名字。
const ICON_GREEN: &[u8] = include_bytes!("../icons/icon.ico");
const ICON_WHITE: &[u8] = include_bytes!("../icons/icon-white.ico");
const ICON_ORANGE: &[u8] = include_bytes!("../icons/icon-orange.ico");

fn icon_bytes(accent: Option<&str>) -> &'static [u8] {
    match accent {
        Some("white") => ICON_WHITE,
        Some("orange") => ICON_ORANGE,
        _ => ICON_GREEN,
    }
}

/// 要原地改的那三项。整份菜单不重建，见文件头第 2 条。
struct Items {
    device: MenuItem<Wry>,
    cpu: CheckMenuItem<Wry>,
    mic: CheckMenuItem<Wry>,
}
static ITEMS: OnceCell<Items> = OnceCell::new();

/// 引擎报上来的实时档位。None = 还没连上/引擎断了。
/// 它和设置里的 cpuOnlyEnabled 不是一回事：自动切换开着时引擎会自己改主意。
static DEVICE: parking_lot::Mutex<Option<Value>> = parking_lot::Mutex::new(None);

// ── 那行「现在跑在哪」 ────────────────────────────────────────────

/// 显示的是**实际生效**的档位，不是设置里选的那个。理由见文件头。
///
/// 读数当参数喂进来，不在里面自己去 lock：这样文件末尾那几条判据能直接验，
/// 不用真起一个引擎，测试之间也不会为了抢同一个全局量打架。
fn device_label(d: Option<&Value>) -> String {
    // 引擎没连上就直说。留着上一次的值会让用户以为还在跑。
    let Some(d) = d else { return "模型位置：等待引擎…".into() };
    let at = d.get("at").and_then(|v| v.as_str()).unwrap_or("");
    let mut parts = vec![format!(
        "当前：{}",
        if at == "cpu" { "内存 (RAM)" } else { "显存 (VRAM)" }
    )];
    if d.get("auto").and_then(|v| v.as_bool()).unwrap_or(false) {
        parts.push("自动".into());
    }
    // 只在引擎的判断和用户的选择真的分叉时才说一句。没分叉还写「引擎改的」只是
    // 噪音；分叉了不写，用户就会觉得开关是坏的。
    let manual = d.get("manual").and_then(|v| v.as_str()).unwrap_or("");
    if d.get("forced").and_then(|v| v.as_bool()).unwrap_or(false) && at != manual {
        parts.push("引擎改的".into());
    }
    // 这里**故意不带 why 里那个数字**：why 是降舱那一刻冻结下来的，之后再不更新，
    // 却用现在时的口气一直挂着。真实发生过的样子是托盘写着「空闲显存只剩 895 MB」，
    // 而任务管理器同一时刻显示 2.6/16.0 GB——数字没错，只是三小时前的。
    // 要给的是下面这行现读的余量。
    if let Some(free) = d.get("freeMb").and_then(|v| v.as_f64()) {
        // 在内存里跑的时候，光说「空闲多少」没用——用户想知道的是还差多少才搬得回去。
        // 门槛由引擎给，和它自己判断升舱用的是同一个数，不能在这边另算一份。
        match (at, d.get("needMb").and_then(|v| v.as_f64())) {
            ("cpu", Some(need)) => {
                parts.push(format!("显存空闲 {free} MB，够 {need} MB 就搬回去"))
            }
            _ => parts.push(format!("显存空闲 {free} MB")),
        }
    }
    parts.join(" · ")
}

/// 勾选态跟着**设置**走，不跟着实际档位走：点它写的就是这个设置。拿实际档位当
/// 勾选态的话，引擎自动降到内存后这一项会自己变成勾上，用户再点一下等于把一个
/// 本来就是 false 的设置又写成 false，看着像点了没反应。
///
/// 代价是「当前：内存」和这一项没勾上会同时出现，看着自相矛盾——所以标题里写了
/// 「优先」，把它读成一个偏好而不是一句现状描述。少了这两个字就是个显示 bug。
fn cpu_checked(d: Option<&Value>, s: &Map<String, Value>) -> bool {
    match d {
        Some(d) => d.get("manual").and_then(|v| v.as_str()) == Some("cpu"),
        None => s.get("cpuOnlyEnabled").and_then(|v| v.as_bool()).unwrap_or(false),
    }
}

// ── 建 / 刷新 ─────────────────────────────────────────────────────

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let s = crate::settings::read();
    let enabled = s.get("floatingMicEnabled").and_then(|v| v.as_bool()).unwrap_or(false);

    let settings = MenuItemBuilder::with_id("settings", "设置").build(app)?;
    let device = MenuItemBuilder::with_id("device", device_label(DEVICE.lock().as_ref()))
        .enabled(false)
        .build(app)?;
    let cpu = CheckMenuItemBuilder::with_id("cpu", "优先用内存跑模型")
        .checked(cpu_checked(DEVICE.lock().as_ref(), &s))
        .build(app)?;
    let mic = CheckMenuItemBuilder::with_id("mic", "显示悬浮麦克风")
        .checked(enabled)
        .build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;

    let menu = MenuBuilder::new(app)
        .item(&settings)
        .separator()
        .item(&device)
        .item(&cpu)
        .separator()
        .item(&mic)
        .separator()
        .item(&quit)
        .build()?;

    let accent = s.get("accentColor").and_then(|v| v.as_str()).map(String::from);
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(Image::from_bytes(icon_bytes(accent.as_deref()))?)
        .tooltip("Localless")
        .menu(&menu)
        // 左键归设置窗口，菜单只挂右键——和 Electron 版的 tray.on('click') 一致。
        .show_menu_on_left_click(false)
        .on_menu_event(|app, ev| on_menu(app, ev.id().as_ref()))
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = ev
            {
                crate::settingswin::toggle(tray.app_handle());
            }
        })
        .build(app)?;

    let _ = ITEMS.set(Items { device, cpu, mic });
    Ok(())
}

fn on_menu(app: &AppHandle, id: &str) {
    match id {
        "settings" => crate::settingswin::toggle(app),
        // 勾选态是 Tauri 自己先翻好的，直接读即可；用 !当前值 反而会在菜单被别的
        // 路径刷新过之后跟真实状态错开。
        "cpu" => {
            let on = ITEMS.get().map_or(false, |i| i.cpu.is_checked().unwrap_or(false));
            set_cpu_only(app, on);
        }
        "mic" => {
            let on = ITEMS.get().map_or(false, |i| i.mic.is_checked().unwrap_or(false));
            set_floating_mic(app, on);
        }
        "quit" => app.exit(0),
        _ => {}
    }
}

/// 档位、勾选态、图标一起对齐当前的设置和引擎读数。
pub fn refresh(app: &AppHandle) {
    let s = crate::settings::read();
    if let Some(i) = ITEMS.get() {
        let d = DEVICE.lock();
        let _ = i.device.set_text(device_label(d.as_ref()));
        let _ = i.cpu.set_checked(cpu_checked(d.as_ref(), &s));
        let _ = i.mic.set_checked(
            s.get("floatingMicEnabled").and_then(|v| v.as_bool()).unwrap_or(false),
        );
    }
    // 换配色不用重启，托盘那张图当场换掉。
    let accent = s.get("accentColor").and_then(|v| v.as_str());
    if let (Some(tray), Ok(img)) = (app.tray_by_id(TRAY_ID), Image::from_bytes(icon_bytes(accent))) {
        let _ = tray.set_icon(Some(img));
    }
}

// ── 托盘上那两个开关 ──────────────────────────────────────────────

/// 故意**不动** autoDeviceEnabled：那是另一件事（准不准引擎自己改主意）。代价是
/// 自动切换开着时，引擎随后仍可能把模型搬回去——所以托盘标签显示的是实际档位，
/// 用户看得见它变回去了，而不是对着一个「我明明点了显存」的哑开关。
fn set_cpu_only(app: &AppHandle, on: bool) {
    let mut patch = Map::new();
    patch.insert("cpuOnlyEnabled".into(), Value::Bool(on));
    // 撞上撕裂读时 merge 返回 None 并跳过写入，这时候别去刷新界面，
    // 免得显示成已经改了、其实没写进去。
    if crate::settings::merge(patch).is_none() {
        refresh(app); // 把刚被 Tauri 翻掉的勾选态拨回真实值
        return;
    }
    let _ = app.emit_to(crate::settingswin::LABEL, "localless://cpu-only", on);
    // 引擎那边要几秒才会把新档位报上来。先按用户点的画一次，别让菜单看着没反应；
    // 真实值到了会再刷一次，不一致的话以引擎为准。
    refresh(app);
}

/// 走的是设置页那同一个 floatingMicEnabled，不另开一个「临时隐藏」状态：两个
/// 开关各管各的，用户从托盘藏起来、到设置页一看还勾着，只会以为坏了。代价是这次
/// 隐藏会跟着写进设置、重启后依然是隐藏的——但这正是「我不想要这个图标」该有的语义。
fn set_floating_mic(app: &AppHandle, on: bool) {
    let mut patch = Map::new();
    patch.insert("floatingMicEnabled".into(), Value::Bool(on));
    if crate::settings::merge(patch).is_none() {
        refresh(app);
        return;
    }
    crate::micwin::sync(app);
    let _ = app.emit_to(crate::settingswin::LABEL, "localless://floating-mic-enabled", on);
    refresh(app);
}

// ── 引擎那条缝 ────────────────────────────────────────────────────

/// 引擎报的实时档位，经药丸那条 ws 转进来（对应 Electron 的
/// localless:device-state）。engine.py 还没搬过来，所以今天没人调它：
/// DEVICE 一直是 None，菜单显示「等待引擎…」，勾选态退回读设置——
/// 和引擎断开时 Electron 版的表现完全一致。
///
/// 只认药丸：设置页、悬浮麦没有理由发这个，认了就等于多开一个能改托盘显示的口子。
#[tauri::command]
pub fn device_state(app: AppHandle, window: tauri::WebviewWindow, device: Option<Value>) {
    if window.label() != crate::BAR {
        return;
    }
    // null 是引擎断开的正常信号，不是脏数据；除此之外只收对象。
    let next = match device {
        None | Some(Value::Null) => None,
        Some(v) if v.is_object() => Some(v),
        _ => return,
    };
    {
        let mut cur = DEVICE.lock();
        // 值没变就别刷——省掉一次没有内容的菜单更新。
        if *cur == next {
            return;
        }
        *cur = next;
    }
    refresh(&app);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// .ico 里每个目录项的宽（0 表示 256）。
    fn ico_sizes(b: &[u8]) -> Vec<u16> {
        let n = u16::from_le_bytes([b[4], b[5]]) as usize;
        (0..n)
            .map(|i| match b[6 + 16 * i] {
                0 => 256,
                w => w as u16,
            })
            .collect()
    }

    /// 这几张图标必须编得进来。真炸过一次：图标路径指向一个 .gitignore 掉的目录，
    /// 换台机器克隆下来托盘就退成一个看不见的空图标，跑起来之前谁也发现不了。
    ///
    /// 后来又炸过第二次，而且悄悄得多：icons/icon.ico 一直是 Tauri 脚手架那张
    /// 默认 logo，从没被换成真图标。它是张正经的 .ico、四万字节里也有一万五，
    /// 光验魔数和长度一个字都看不出来——托盘的绿态、exe、任务栏、安装包于是
    /// 全挂着 Tauri 的 logo。所以下面两条才是真正的判据：三张必须互不相同
    /// （icon.ico 没换过的话它和另外两张不同、但和「绿」这个语义无关，
    /// 真正露馅的是尺寸），且每张都得带 256——脚手架那张只有 16/32/48。
    #[test]
    fn 三张图标都在() {
        for b in [ICON_GREEN, ICON_WHITE, ICON_ORANGE] {
            assert!(b.len() > 1000);
            assert_eq!(&b[..4], &[0, 0, 1, 0], "不是 .ico");
            let sizes = ico_sizes(b);
            assert!(sizes.contains(&256), "缺 256×256：exe 图标和安装包要的就是这一档（有 {sizes:?}）");
            assert!(sizes.contains(&16), "缺 16×16：托盘按这一档取（有 {sizes:?}）");
        }
        assert_ne!(ICON_GREEN, ICON_WHITE);
        assert_ne!(ICON_GREEN, ICON_ORANGE);
        assert_ne!(ICON_WHITE, ICON_ORANGE);
        assert_eq!(icon_bytes(Some("white")), ICON_WHITE);
        assert_eq!(icon_bytes(Some("orange")), ICON_ORANGE);
        assert_eq!(icon_bytes(Some("green")), ICON_GREEN);
        // 认不出来的配色退回绿那张，不是空图标。
        assert_eq!(icon_bytes(Some("chartreuse")), ICON_GREEN);
        assert_eq!(icon_bytes(None), ICON_GREEN);
    }

    #[test]
    fn 引擎没连上就直说() {
        assert_eq!(device_label(None), "模型位置：等待引擎…");
    }

    #[test]
    fn 档位标签按实际档位拼() {
        let gpu = json!({ "at": "gpu", "manual": "gpu", "freeMb": 2600 });
        assert_eq!(device_label(Some(&gpu)), "当前：显存 (VRAM) · 显存空闲 2600 MB");

        // 在内存里跑：要说的是还差多少才搬得回去，不是光报空闲。
        let 降舱 = json!({ "at": "cpu", "manual": "gpu", "auto": true, "forced": true,
                           "freeMb": 895, "needMb": 3200 });
        assert_eq!(
            device_label(Some(&降舱)),
            "当前：内存 (RAM) · 自动 · 引擎改的 · 显存空闲 895 MB，够 3200 MB 就搬回去"
        );

        // 没分叉就不该出现「引擎改的」——那时它只是噪音。
        let 用户自己选的 = json!({ "at": "cpu", "manual": "cpu", "forced": true });
        assert_eq!(device_label(Some(&用户自己选的)), "当前：内存 (RAM)");
    }

    /// 勾选态跟设置走、标签跟引擎走，两者分叉时各说各的，谁也不许去迁就谁。
    #[test]
    fn 勾选态不跟着实际档位跑() {
        let off: Map<String, Value> =
            json!({ "cpuOnlyEnabled": false }).as_object().unwrap().clone();

        // 引擎自动降到了内存，但用户选的仍是显存：这一项必须**不**勾上。
        let 降舱 = json!({ "at": "cpu", "manual": "gpu", "forced": true });
        assert!(!cpu_checked(Some(&降舱), &off));
        assert!(device_label(Some(&降舱)).starts_with("当前：内存"), "标签同时要说实话");

        let 反过来 = json!({ "at": "gpu", "manual": "cpu" });
        assert!(cpu_checked(Some(&反过来), &off), "引擎给的 manual 优先于设置快照");

        // 引擎没连上才退回读设置。
        assert!(!cpu_checked(None, &off));
        let on: Map<String, Value> =
            json!({ "cpuOnlyEnabled": true }).as_object().unwrap().clone();
        assert!(cpu_checked(None, &on));
    }
}

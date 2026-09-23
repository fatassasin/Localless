// 设置窗口。搬自 Electron 版 main.js:1145-1216（建窗与 bounds）、780-815（保存）、
// 936-946（settings-changed）、1000-1029（开机自启）、1396-1410（文件对话框）。
//
// 三件事在这一版里换了做法，其余全部照搬：
//
// 1. 关窗前的兜底保存。Electron 那边设置页 save() 有 180ms 防抖，关窗会把渲染
//    进程直接销毁，定时器永远不触发——那边靠 ipcRenderer.sendSync 挡住渲染进程
//    直到主进程写完。Tauri 没有同步 IPC，这边改成：拦下关闭请求 → 发
//    localless://settings-flush → 页面存完回调 settings_close_force → 真关。
//    带 700ms 超时强关，否则页面里任何一个异常都会变成「设置窗口关不掉」。
// 2. bounds 存的是 DIP。和 Electron 版共用同一份 settingsWinBounds，那边
//    getBounds() 给的就是 DIP；Tauri 的 set_position/set_size 要物理像素，
//    所以只在最后一步乘缩放比。中间一律 DIP。
// 3. 开机自启自己写注册表。Electron 的 app.setLoginItemSettings 这边没有，
//    但那条「先清 StartupApproved 的否决记录再写 Run」的顺序必须原样保留——
//    顺序反了的话写完读回来还是 false，开关会在用户眼前自己弹回去。

use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindowBuilder,
};
use tauri_plugin_dialog::DialogExt;

pub const LABEL: &str = "settings";

/// 和 Electron 版同一组下限。比这更小的 bounds 一律当脏数据丢掉。
const MIN_W: f64 = 600.0;
const MIN_H: f64 = 420.0;
const DEF_W: f64 = 760.0;
const DEF_H: f64 = 680.0;

/// 页面已经存完、可以真关了。见文件头第 1 条。
static FORCE_CLOSE: AtomicBool = AtomicBool::new(false);
/// 摆好了没、画好了没。见 `show_when_ready`。
static PLACED: AtomicBool = AtomicBool::new(false);
static PAINTED: AtomicBool = AtomicBool::new(false);
/// show_when_ready 的一次性闸。摆好、画好、兜底超时三路都会来敲门，
/// 露面那一套（盖住→show→剥样式→放出来）只许跑第一次。
static SHOWN: AtomicBool = AtomicBool::new(false);
/// bounds 防抖的代次。每次 move/resize 加一，400ms 后只有代次没变的那一轮才写。
static BOUNDS_GEN: AtomicU64 = AtomicU64::new(0);
/// 当前页面缩放（f64 的位）。见 `fit_zoom`。
static ZOOM: AtomicU64 = AtomicU64::new(0x3FF0_0000_0000_0000); // 1.0

/// 工作区（DIP）高到这个数，页面就按原大小显示；矮于它就整页等比缩小。
/// 本地 4K@200%（任务栏自动隐藏）的工作区正好 1080 DIP 高，落在 1.0。
const ZOOM_REF_H: f64 = 1080.0;
const ZOOM_MIN: f64 = 0.5;

// ── bounds 的验证 ─────────────────────────────────────────────────

/// 一块屏的可用区（DIP）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// 存下来的窗口位置只在还落得进某块屏幕时才作数。拔掉副屏、换分辨率、从扩展改成
/// 复制之后，上次那套坐标可能整个在可见区域之外；而这个窗口 skip_taskbar，真开到
/// 屏幕外面就既看不见也点不回来，只能去删配置文件。宁可退回右下角的默认位置。
///
/// 纯函数：屏幕列表当参数喂进来，好让文件末尾的测试直接验那几条判据——
/// 「拔掉副屏」这种情形没法靠插拔显示器试。
fn bounds_in(raw: Option<&Value>, screens: &[Rect]) -> Option<Rect> {
    let o = raw?.as_object()?;
    let num = |k: &str| -> Option<f64> {
        o.get(k)
            .and_then(|v| v.as_f64())
            .filter(|n| n.is_finite())
            .map(|n| n.round())
    };
    let (w, h, x, y) = (num("width")?, num("height")?, num("x")?, num("y")?);
    if w < MIN_W || h < MIN_H {
        return None;
    }
    let visible = screens.iter().any(|a| {
        let ox = (x + w).min(a.x + a.w) - x.max(a.x);
        let oy = (y + h).min(a.y + a.h) - y.max(a.y);
        // 光有交集不够，交集还得落在标题栏那一条上：窗口只剩底边露在屏幕里的话，
        // 能看见却抓不住，照样拖不回来。y 不许高过工作区顶就是在保这一条。
        ox >= 80.0 && oy >= 40.0 && y >= a.y - 8.0
    });
    visible.then_some(Rect { x, y, w, h })
}

/// 所有屏的可用区（DIP）。Tauri 的 available_monitors 给的是整块屏，不含任务栏
/// 扣除，所以拿每块屏的中心点去问一次 Win32 要 rcWork——和 Electron 的
/// display.workArea 对齐。
fn screens(app: &AppHandle, scale: f64) -> Vec<Rect> {
    let mons = app.available_monitors().unwrap_or_default();
    let mut out: Vec<Rect> = mons
        .iter()
        .map(|m| {
            let p = m.position();
            let s = m.size();
            let cx = p.x + s.width as i32 / 2;
            let cy = p.y + s.height as i32 / 2;
            let r = crate::micwin::rect_near(cx, cy);
            Rect {
                x: r.left as f64 / scale,
                y: r.top as f64 / scale,
                w: (r.right - r.left) as f64 / scale,
                h: (r.bottom - r.top) as f64 / scale,
            }
        })
        .collect();
    if out.is_empty() {
        let r = crate::micwin::primary_rect();
        out.push(Rect {
            x: r.left as f64 / scale,
            y: r.top as f64 / scale,
            w: (r.right - r.left) as f64 / scale,
            h: (r.bottom - r.top) as f64 / scale,
        });
    }
    out
}

/// 现问显示器，不读 tao 缓存的 scale_factor()——远程切屏时那份缓存不更新，见 display.rs。
fn scale_of(app: &AppHandle) -> f64 {
    app.get_webview_window(LABEL)
        .map(|w| crate::display::live_scale(&w))
        .or_else(|| app.primary_monitor().ok().flatten().map(|m| m.scale_factor()))
        .unwrap_or(1.0)
}

/// 页面缩放跟着屏幕走。
///
/// 远程连进来时主屏是 1920×1080@175%，工作区只剩约 1097×617 DIP；本地是
/// 3840×2160@200% = 1920×1080 DIP。设置窗口（760×522）在远程那块屏上一下占掉
/// 大半，标题栏被挤到上沿外面，关都关不掉。存下来的大小是「原大小」，这里算一个
/// 系数，窗口尺寸和页面缩放一起乘上去，屏幕小多少，整页就等比缩多少。
fn fit_zoom(scale: f64) -> f64 {
    let r = crate::micwin::primary_rect();
    let h = (r.bottom - r.top) as f64 / scale;
    (h / ZOOM_REF_H).clamp(ZOOM_MIN, 1.0)
}

fn zoom_now() -> f64 {
    f64::from_bits(ZOOM.load(Ordering::SeqCst))
}

fn apply_zoom(w: &tauri::WebviewWindow, z: f64) {
    ZOOM.store(z.to_bits(), Ordering::SeqCst);
    if let Err(e) = w.set_zoom(z) {
        eprintln!("[设置] 页面缩放没设上：{e}");
    }
    let _ = w.set_min_size(Some(tauri::LogicalSize::new(MIN_W * z, MIN_H * z)));
}

/// 把窗口整个塞进它压得最多的那块屏：宽高不超过工作区，标题栏不出上沿。
/// 屏幕变小之后（远程切到 1080p）旧的大小和位置可能伸出屏外，标题栏被挤掉就关不了。
fn fit_into(b: Rect, screens: &[Rect]) -> Rect {
    let overlap = |a: &Rect| {
        let ox = ((b.x + b.w).min(a.x + a.w) - b.x.max(a.x)).max(0.0);
        let oy = ((b.y + b.h).min(a.y + a.h) - b.y.max(a.y)).max(0.0);
        ox * oy
    };
    let Some(a) = screens
        .iter()
        .max_by(|p, q| overlap(p).partial_cmp(&overlap(q)).unwrap_or(std::cmp::Ordering::Equal))
    else {
        return b;
    };
    let w = b.w.min(a.w);
    let h = b.h.min(a.h);
    Rect {
        x: b.x.min(a.x + a.w - w).max(a.x).round(),
        y: b.y.min(a.y + a.h - h).max(a.y).round(),
        w: w.round(),
        h: h.round(),
    }
}

/// 分辨率/缩放变了之后，把开着的设置窗口塞回屏幕里。display.rs 先补过
/// WM_DPICHANGED，tao 已经按逻辑尺寸把窗口缩放好了，这里只管别伸出屏外。
pub fn refit(app: &AppHandle) {
    let Some(w) = app.get_webview_window(LABEL) else { return };
    let scale = crate::display::live_scale(&w);
    let (Ok(pos), Ok(size)) = (w.outer_position(), w.outer_size()) else { return };
    let cur = Rect {
        x: (pos.x as f64 / scale).round(),
        y: (pos.y as f64 / scale).round(),
        w: (size.width as f64 / scale).round(),
        h: (size.height as f64 / scale).round(),
    };
    let (z_old, z) = (zoom_now(), fit_zoom(scale));
    let k = z / z_old;
    if (k - 1.0).abs() > 1e-3 {
        apply_zoom(&w, z);
    }
    let want = Rect { w: cur.w * k, h: cur.h * k, ..cur };
    let b = fit_into(want, &screens(app, scale));
    if b == cur {
        return;
    }
    let _ = w.set_size(PhysicalSize::new(
        (b.w * scale).round() as u32,
        (b.h * scale).round() as u32,
    ));
    let _ = w.set_position(PhysicalPosition::new(
        (b.x * scale).round() as i32,
        (b.y * scale).round() as i32,
    ));
    eprintln!("[设置] 屏幕变了，{cur:?} → {b:?}（缩放 {scale}）");
}

// ── 开关窗口 ──────────────────────────────────────────────────────

/// 托盘/快捷键上那一下：开着就关，关着就开。对应 Electron 的 toggleSettings()。
pub fn toggle(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(LABEL) {
        let _ = w.close();
        return;
    }
    if let Err(e) = open(app) {
        eprintln!("[设置] 建窗失败：{e}");
    }
}

/// 把 Win11 的圆角要回来。
///
/// 圆角不是窗口样式的一部分，是 DWM 在非客户区渲染里顺手做的——和投影同一套东西。
/// 于是上面那句 `.shadow(false)`（为了灭掉窗口外沿那条 1 DIP 的亮边，理由见 open()
/// 里的长注释）把圆角一起带走了，窗口变成四个直角。
///
/// DWMWA_WINDOW_CORNER_PREFERENCE 是独立的一路开关，不经过投影：显式要
/// DWMWCP_ROUND，DWM 就照样把四角裁圆，那条玻璃边距不会回来。
///
/// 无条件写、不检查当前值：这是个只写属性，读回来是 E_INVALIDARG，没有「脏了才写」
/// 可言。一次调用是微秒级，放在 show 之后重放一遍比赌 tao 不会重置便宜——它重写
/// 窗口样式的时候 DWM 侧会不会跟着复位，没有文档说得准。
fn round_corners(window: &tauri::WebviewWindow) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    };
    let Ok(raw) = window.hwnd() else { return };
    let hwnd = HWND(raw.0 as *mut std::ffi::c_void);
    let pref = DWMWCP_ROUND;
    unsafe {
        if let Err(e) = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const _ as *const std::ffi::c_void,
            std::mem::size_of_val(&pref) as u32,
        ) {
            // Win10 上没有这个属性，失败是正常的，那边本来也没有圆角。
            eprintln!("[设置] 圆角没设上（Win10 上正常）：{e}");
        }
    }
}

/// 摆好了、也画出来了，才让窗口露面。两件事哪个先到都行，谁后到谁负责 show。
///
/// 不能只等其中一件：只等「画好」的话，导航结束有可能早于下面那两句重摆，
/// 窗口照样会在左上角先露一下；只等「摆好」的话躲不掉白屏那一帧。
fn show_when_ready(w: &tauri::WebviewWindow) {
    if !(PLACED.load(Ordering::SeqCst) && PAINTED.load(Ordering::SeqCst)) {
        return;
    }
    // 只放行第一次。PLACED、PAINTED、还有 1500ms 那个兜底线程都会往这儿撞，
    // 后来的那几下会把已经露面的窗口重新盖一遍再放出来——闪的就是我们自己了。
    if SHOWN.swap(true, Ordering::SeqCst) {
        return;
    }
    // 上屏那一帧先用空区域整块盖住。
    //
    // 下面那句 show() 不是单纯把窗口显示出来：tao 的 set_visible 会按自己缓存的
    // 那套 WindowFlags 把整个 GWL_STYLE 重写一遍，WS_CAPTION 跟着回来（实测
    // 0x14CF0000 = CAPTION+THICKFRAME+SYSMENU+MIN+MAX，且无 POPUP），窗口是**带着
    // 原生标题栏**上屏的，keep_frameless 是事后才剥。中间还隔着一句 set_focus，
    // 桌面级监视实测这两个样式之间相差 166ms——DWM 在这段里合成过好几帧，画出来
    // 的就是那条灰/蓝的 Windows 小标题栏。
    //
    // 为什么用区域而不是把 keep_frameless 提前：提前只是把窗口缩小，不是关掉。
    // 样式什么时候被 tao 写回去由 tao 说了算，我们只能保证「写回去的那一刻屏幕上
    // 没有这扇窗」。区域是纯 Win32 的，不在 tao 缓存的任何一位里，重放不掉。
    crate::pill::blank(w);
    let _ = w.show();
    let _ = w.set_focus();
    // 删标题栏这一刀只能落在 show 之后。它也是 decorations(false)，样式里照样
    // 挂着 WS_CAPTION，而 tao 的 set_visible 会按缓存重写——建完就删是白删，实测
    // 删完量回来一位没少。走保留 WS_THICKFRAME 的那条：这扇窗可缩放，删了边缘就
    // 拖不动了。见 pill::keep_frameless。
    crate::pill::keep_frameless(w, true);
    // keep_frameless 刚动过窗口样式并 SWP_FRAMECHANGED，非客户区这一刻重算了
    // 一遍——圆角放在它后面写，别被那一下顶掉。
    round_corners(w);
    // 样式干净了，放出来。
    crate::pill::unblank(w);
}

fn open(app: &AppHandle) -> tauri::Result<()> {
    let scale = scale_of(app);
    let s = crate::settings::read();
    let saved = bounds_in(s.get("settingsWinBounds"), &screens(app, scale));

    // 没有存档时贴右下角，和 Electron 版同一组数字。
    let primary = {
        let r = crate::micwin::primary_rect();
        Rect {
            x: r.left as f64 / scale,
            y: r.top as f64 / scale,
            w: (r.right - r.left) as f64 / scale,
            h: (r.bottom - r.top) as f64 / scale,
        }
    };
    let b = saved.unwrap_or(Rect {
        x: (primary.w - 780.0).round(),
        y: (primary.h - 720.0).round(),
        w: DEF_W,
        h: DEF_H,
    });
    // 存档是原大小，按这块屏的缩放系数缩一次再往里塞。
    let z = fit_zoom(scale);
    let b = fit_into(Rect { w: b.w * z, h: b.h * z, ..b }, &screens(app, scale));

    PLACED.store(false, Ordering::SeqCst);
    PAINTED.store(false, Ordering::SeqCst);
    SHOWN.store(false, Ordering::SeqCst);

    let w = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("settings.html".into()))
        .title("Localless 设置")
        .decorations(false)
        .transparent(false)
        .always_on_top(true)
        .skip_taskbar(true)
        // 阴影关掉，不是为了省那点合成——是它会在窗口外沿画出一圈亮边。
        //
        // 无边框窗口要有投影，tao 走的是 DwmExtendFrameIntoClientArea，给窗口四边
        // 各留 1 像素的「玻璃」边距；DWM 在那一圈里画的是系统的玻璃帧，不是页面。
        // 于是深色的页面外面就永远箍着一条 1 DIP 的浅色线——200% 缩放下是两个物理
        // 像素，实测 #CCB8A6 / #E5CFBB，颜色固定，跟窗口底下是什么无关。
        //
        // 排除过的：页面 CSS（html/body 是实色 #0e1116，一路铺到边）、
        // DWMWA_BORDER_COLOR（改成红、黑、NONE 都 hr=0 但一个像素不动）、
        // Windows 强调色边框（ColorPrevalence=0，强调色是 #4A5459 的蓝灰，不是这个米色）、
        // WS_THICKFRAME、圆角策略、系统背板、WS_EX_WINDOWEDGE、亚克力合成属性
        // ——全部在线改过，那条线纹丝不动。只剩这一处。
        //
        // 代价是圆角也跟着没了：Win11 的圆角和投影是 DWM 同一套非客户区渲染画的，
        // 投影一关窗口就变成尖角。所以下面 round_corners() 显式把圆角要回来。
        .shadow(false)
        .resizable(true)
        .min_inner_size(MIN_W * z, MIN_H * z)
        // 建出来先别露面。下面那两句重摆是在 build() **之后**发生的，窗口这时候
        // 已经在屏幕上了——于是每次开设置都是先在左上角闪一下再跳到右下角。
        // 藏着建、摆好再 show，那一下闪就没了。
        .visible(false)
        .on_page_load(|w, ev| {
            // 顺带把白屏那一帧也躲掉：WebView2 画出第一帧之前窗口是白的，
            // 而这页是黑色毛玻璃，直接 show 会先闪一块白。
            if matches!(ev.event(), tauri::webview::PageLoadEvent::Finished) {
                PAINTED.store(true, Ordering::SeqCst);
                show_when_ready(&w);
            }
        })
        .build()?;

    // 建窗时给的 size/position 在多 DPI 下不可靠：build() 那一刻窗口还没落到
    // 任何一块屏上，tao 会按缩放 1 去换算逻辑像素。落地之后按物理像素重摆一次。
    // （和 micwin::place 同一个坑，理由见 micwin.rs 头部。）
    let _ = w.set_size(PhysicalSize::new(
        (b.w * scale).round() as u32,
        (b.h * scale).round() as u32,
    ));
    let _ = w.set_position(PhysicalPosition::new(
        (b.x * scale).round() as i32,
        (b.y * scale).round() as i32,
    ));
    apply_zoom(&w, z);

    // Win11 的亚克力：系统模糊窗口背后的桌面。不支持的系统上这一句失败，
    // 页面退回自己的背景色照常显示——所以只记一行，不当错误。
    if let Err(e) = w.set_effects(tauri::utils::config::WindowEffectsConfig {
        effects: vec![tauri::utils::WindowEffect::Acrylic],
        state: None,
        radius: None,
        color: None,
    }) {
        eprintln!("[设置] 亚克力没挂上（不影响使用）：{e}");
    }

    PLACED.store(true, Ordering::SeqCst);
    show_when_ready(&w);
    // 兜底。页面要是压根没导航成功，on_page_load 永远不来，窗口就会一直藏着——
    // 那是「点托盘打不开设置」，比闪一下难查得多。超时就无条件露面。
    {
        let h = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            if let Some(w) = h.get_webview_window(LABEL) {
                if !w.is_visible().unwrap_or(true) {
                    eprintln!("[设置] 页面没画出来，先把窗口显示出来");
                    PAINTED.store(true, Ordering::SeqCst);
                    show_when_ready(&w);
                }
            }
        });
    }

    let handle = app.clone();
    w.on_window_event(move |e| match e {
        // 拖动中 move/resize 是连发的，防抖到停手再写。
        tauri::WindowEvent::Moved(_) | tauri::WindowEvent::Resized(_) => {
            remember_bounds(&handle);
        }
        tauri::WindowEvent::CloseRequested { api, .. } => {
            if FORCE_CLOSE.swap(false, Ordering::SeqCst) {
                persist_bounds(&handle);
                return;
            }
            // 见文件头第 1 条：先让页面把防抖里那笔存完。
            api.prevent_close();
            let _ = handle.emit("localless://settings-flush", json!({}));
            let h = handle.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(700));
                if let Some(w) = h.get_webview_window(LABEL) {
                    eprintln!("[设置] 页面没回话，强关");
                    FORCE_CLOSE.store(true, Ordering::SeqCst);
                    let _ = w.close();
                }
            });
        }
        _ => {}
    });

    Ok(())
}

/// 400ms 防抖后写一次 bounds。这一份是防「进程被杀」的兜底；
/// 正常关窗由 CloseRequested 那边的 persist_bounds 负责——刚拖完就关窗是最常见
/// 的动作，全交给防抖会丢。
fn remember_bounds(app: &AppHandle) {
    let gen = BOUNDS_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let h = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        if BOUNDS_GEN.load(Ordering::SeqCst) == gen {
            persist_bounds(&h);
        }
    });
}

fn persist_bounds(app: &AppHandle) {
    let Some(w) = app.get_webview_window(LABEL) else { return };
    if w.is_minimized().unwrap_or(false) {
        return;
    }
    let (Ok(pos), Ok(size)) = (w.outer_position(), w.inner_size()) else { return };
    let scale = crate::display::live_scale(&w);
    // 存原大小：窗口此刻是按 fit_zoom 缩过的，除回去，换回大屏时才还原得了。
    let z = zoom_now();
    let size = PhysicalSize::new(
        (size.width as f64 / z).round() as u32,
        (size.height as f64 / z).round() as u32,
    );
    eprintln!(
        "[设置] 记住位置 物理 {}×{} @{},{} · 缩放 {scale} → 存 DIP {}×{} @{},{}",
        size.width, size.height, pos.x, pos.y,
        (size.width as f64 / scale).round(), (size.height as f64 / scale).round(),
        (pos.x as f64 / scale).round(), (pos.y as f64 / scale).round()
    );
    let mut patch = Map::new();
    patch.insert(
        "settingsWinBounds".into(),
        json!({
            "x": (pos.x as f64 / scale).round() as i64,
            "y": (pos.y as f64 / scale).round() as i64,
            "width": (size.width as f64 / scale).round() as i64,
            "height": (size.height as f64 / scale).round() as i64,
        }),
    );
    crate::settings::merge(patch);
}

// ── 页面那几条命令 ────────────────────────────────────────────────

fn mine(window: &tauri::WebviewWindow) -> bool {
    window.label() == LABEL
}

/// 保存设置。对应 Electron 的 applySettingsPatch。
///
/// 那三个字段必须剔掉：悬浮麦的位置和比例只由拖动/重置那条路写入，设置页的内存
/// 快照可能稍旧，一次普通保存就会把刚拖好的坐标覆盖回去；窗口自己的 bounds 同理
/// ——设置页那份快照是开窗那一刻读进去的，带的是拖大之前的旧尺寸，不剔掉的话
/// 拖大窗口再随手改一个开关，尺寸就被打回去了。
#[tauri::command]
pub fn settings_save(
    app: AppHandle,
    window: tauri::WebviewWindow,
    patch: Map<String, Value>,
) -> Result<Map<String, Value>, String> {
    if !mine(&window) {
        return Err("invalid settings source".into());
    }
    let mut clean = patch;
    for k in ["floatingMicPosition", "floatingMicAnchor", "settingsWinBounds"] {
        clean.remove(k);
    }
    let next = crate::settings::merge(clean).ok_or_else(|| "这次写入被跳过了（见日志）".to_string())?;
    crate::micwin::sync(&app);
    Ok(next)
}

/// 设置改完之后的副作用。
#[tauri::command]
pub fn settings_changed(app: AppHandle) {
    crate::micwin::sync(&app); // 内部顺带把托盘的勾选态和图标对齐
    // 只有快捷键真的变了才重起钩子——判据在 keyhook 那边，这里无条件叫。
    crate::keyhook::restart_if_changed(&app);
    // 刚把「录音时静音」打开的话，趁现在把 mute.ps1 那一秒的编译付掉，
    // 别留到用户第一次按快捷键的时候。已经起着就是空跑一趟。
    crate::mute::warm_up();
}

/// 页面按「关闭」。走正常的 close，于是照样过一遍 CloseRequested 的 flush 流程。
#[tauri::command]
pub fn settings_close(window: tauri::WebviewWindow) {
    if mine(&window) {
        let _ = window.close();
    }
}

/// 页面存完了，可以真关。见文件头第 1 条。
#[tauri::command]
pub fn settings_close_force(window: tauri::WebviewWindow) {
    if !mine(&window) {
        return;
    }
    FORCE_CLOSE.store(true, Ordering::SeqCst);
    let _ = window.close();
}

/// 悬浮麦回默认位置。对应 Electron 的 localless:reset-floating-mic。
#[tauri::command]
pub fn reset_floating_mic(app: AppHandle) -> Value {
    let p = crate::micwin::reset_position(&app);
    json!({ "x": p.0.round() as i64, "y": p.1.round() as i64 })
}

// ── 开机自启 ──────────────────────────────────────────────────────

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const APPROVED_KEY: &str =
    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
const RUN_NAME: &str = "Localless";
/// 老版本写下的那条，命令行和正规那条一字不差，Electron 从不碰它。
const LEGACY_RUN_NAME: &str = "electron.app.Electron";

/// reg.exe 而不是 powershell：这两个调用一个在启动路径、一个在点开关的那一下，
/// powershell 每次要付 ~800ms 的编译钱，reg 是 ~30ms。值名大小写无所谓，注册表不分。
fn reg(args: &[&str]) -> bool {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("reg")
        .args(args)
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW：别闪黑框
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tauri::command]
pub fn autostart_get() -> bool {
    reg(&["query", RUN_KEY, "/v", RUN_NAME])
}

#[tauri::command]
pub fn autostart_set(enabled: bool) -> bool {
    if enabled {
        // 先清「设置」里那条否决记录，再写 Run。顺序反了的话，写完读回来还是
        // false，开关会在用户眼前自己弹回去——这正是「两边对不上」最常见的那一幕。
        reg(&["delete", APPROVED_KEY, "/v", RUN_NAME, "/f"]);
        let exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        if exe.is_empty() {
            eprintln!("[自启] 拿不到自己的路径，没写注册表");
            return false;
        }
        // 路径带空格（"Program Files"）必须带引号，否则开机时 Windows 会把
        // 第一个空格之后的部分当参数，程序根本起不来。
        let quoted = format!("\"{exe}\"");
        reg(&["add", RUN_KEY, "/v", RUN_NAME, "/t", "REG_SZ", "/d", &quoted, "/f"]);
    } else {
        reg(&["delete", RUN_KEY, "/v", RUN_NAME, "/f"]);
        // 关掉就得两条一起关，不然开关是灭的、机器照样自启。
        reg(&["delete", RUN_KEY, "/v", LEGACY_RUN_NAME, "/f"]);
    }
    autostart_get()
}

// ── 打开文件夹 / 文件对话框 ───────────────────────────────────────

/// which: "recordings" | "models"。对应 Electron 的 open-recordings / open-models。
#[tauri::command]
pub fn open_folder(app: AppHandle, which: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let p = match which.as_str() {
        "recordings" => crate::history::recordings_dir(),
        "models" => crate::models::root().join("models"),
        other => return Err(format!("不认识的目录：{other}")),
    };
    // Electron 版 open-models 也是先 mkdir 再开：第一次用的时候目录还不存在，
    // 直接 openPath 会静默失败，看着就像按钮没反应。
    let _ = std::fs::create_dir_all(&p);
    app.opener()
        .open_path(p.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
}

/// 导出词表。async 是必须的：对话框的回调从主线程的事件循环里发出来，
/// 在同步命令里等它就是主线程等自己，直接死锁。
#[tauri::command]
pub async fn save_json(app: AppHandle, name: String, data: String) -> Value {
    let (tx, rx) = std::sync::mpsc::channel();
    app.dialog()
        .file()
        .set_file_name(if name.is_empty() { "export.json" } else { &name })
        .add_filter("JSON", &["json"])
        .save_file(move |p| {
            let _ = tx.send(p);
        });
    let Ok(Some(p)) = rx.recv() else {
        return json!({ "canceled": true });
    };
    let Ok(path) = p.into_path() else {
        return json!({ "canceled": true });
    };
    match std::fs::write(&path, data) {
        Ok(_) => json!({ "path": path.to_string_lossy() }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

/// 导入词表。
#[tauri::command]
pub async fn open_json(app: AppHandle) -> Value {
    let (tx, rx) = std::sync::mpsc::channel();
    app.dialog()
        .file()
        .add_filter("JSON", &["json"])
        .pick_file(move |p| {
            let _ = tx.send(p);
        });
    let Ok(Some(p)) = rx.recv() else {
        return json!({ "canceled": true });
    };
    let Ok(path) = p.into_path() else {
        return json!({ "canceled": true });
    };
    match std::fs::read_to_string(&path) {
        Ok(data) => json!({ "path": path.to_string_lossy(), "data": data }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── 引擎 ──────────────────────────────────────────────────────────

/// 设置页那颗「重启引擎」，以及改完模型/设备之后的自动重启。
///
/// 不等它起来就返回：重载一次模型要十几秒，同步等会把设置页整个卡住，而页面那侧
/// 本来就只看有没有报错。真起不来的话 engine.rs 自己会退避重试，托盘的设备状态
/// 也会跟着变灰。
#[tauri::command]
pub fn reload_engine() -> Result<(), String> {
    std::thread::spawn(crate::engine::restart);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIN: Rect = Rect { x: 0.0, y: 0.0, w: 1920.0, h: 1040.0 };
    /// 左边那块副屏，原点是负的。
    const SIDE: Rect = Rect { x: -1280.0, y: 0.0, w: 1280.0, h: 984.0 };

    fn b(x: f64, y: f64, w: f64, h: f64) -> Value {
        json!({ "x": x, "y": y, "width": w, "height": h })
    }

    /// 远程切到小屏：伸出屏外的窗口要缩进来，标题栏不许高过工作区顶。
    #[test]
    fn 小屏上塞得进去() {
        let small = Rect { x: 0.0, y: 0.0, w: 1097.0, h: 570.0 }; // 1080p@175% 且留了任务栏
        let got = fit_into(Rect { x: 246.0, y: 87.0, w: 760.0, h: 522.0 }, &[small]);
        assert_eq!(got, Rect { x: 246.0, y: 48.0, w: 760.0, h: 522.0 });
        let got = fit_into(Rect { x: 900.0, y: -40.0, w: 1300.0, h: 900.0 }, &[small]);
        assert_eq!(got, Rect { x: 0.0, y: 0.0, w: 1097.0, h: 570.0 });
    }

    /// 正常的一套坐标原样收下。
    #[test]
    fn 屏内的照收() {
        let got = bounds_in(Some(&b(100.0, 80.0, 760.0, 680.0)), &[MAIN]);
        assert_eq!(got, Some(Rect { x: 100.0, y: 80.0, w: 760.0, h: 680.0 }));
    }

    /// 副屏拔掉之后，落在副屏上的那套坐标必须作废——否则窗口开在看不见的地方，
    /// 而它 skip_taskbar，点都点不回来，只能去删配置文件。
    #[test]
    fn 拔掉副屏就作废() {
        let on_side = b(-900.0, 100.0, 760.0, 680.0);
        assert!(bounds_in(Some(&on_side), &[SIDE, MAIN]).is_some(), "副屏还在时应该认");
        assert!(bounds_in(Some(&on_side), &[MAIN]).is_none(), "副屏拔了还认就点不回来了");
    }

    /// 只剩底边露在屏幕里：能看见，却抓不住标题栏拖不回来。判成无效。
    #[test]
    fn 标题栏在屏外算无效() {
        // y 远在工作区上方，只有窗口下半截露出来。
        assert!(bounds_in(Some(&b(100.0, -600.0, 760.0, 680.0)), &[MAIN]).is_none());
        // 刚好在容差里（-8）的还算数。
        assert!(bounds_in(Some(&b(100.0, -8.0, 760.0, 680.0)), &[MAIN]).is_some());
    }

    /// 横向只露一条边（< 80）也抓不住。
    #[test]
    fn 横向露太少算无效() {
        assert!(bounds_in(Some(&b(1880.0, 100.0, 760.0, 680.0)), &[MAIN]).is_none());
        assert!(bounds_in(Some(&b(1840.0, 100.0, 760.0, 680.0)), &[MAIN]).is_some());
    }

    /// 脏数据：缺字段、NaN、比下限还小，一律退回默认位置。
    #[test]
    fn 脏数据退回默认() {
        assert!(bounds_in(None, &[MAIN]).is_none());
        assert!(bounds_in(Some(&json!(null)), &[MAIN]).is_none());
        assert!(bounds_in(Some(&json!("760x680")), &[MAIN]).is_none());
        assert!(bounds_in(Some(&b(100.0, 80.0, 300.0, 680.0)), &[MAIN]).is_none(), "宽度低于下限");
        assert!(bounds_in(Some(&b(100.0, 80.0, 760.0, 100.0)), &[MAIN]).is_none(), "高度低于下限");
        assert!(bounds_in(Some(&json!({ "x": 1, "y": 2, "width": 760 })), &[MAIN]).is_none());
        let nan = json!({ "x": 0, "y": 0, "width": f64::NAN, "height": 680 });
        assert!(bounds_in(Some(&nan), &[MAIN]).is_none());
    }
}

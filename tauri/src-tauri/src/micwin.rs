// 悬浮麦克风窗口。搬自 Electron 版 main.js（174-232 位置/比例、285-323 建窗、
// 324-374 同步、720-745 常量、948-986 三条 IPC）。
//
// 这个窗口只有一个按钮，但它踩过的坑全在「窗口本身」而不在页面上，所以那几条
// 规矩一条都不能省：
//
// 1. 位置按「空余空间的比例」记，不按绝对像素记。UU 远程会把主机的虚拟显示器
//    直接改成客户端设备的分辨率（电脑 3840×2160、iPad 2420×1668、手机
//    2796×1290），同一个绝对坐标换台设备连过来就落到界外，clamp 只能把它拍回
//    最近的边——原本贴右下角的图标会变成贴在右边或下边的某处。比例取的是空余
//    空间而不是宽高：贴右下角就是 fx=fy=1，换到任何尺寸的屏上仍解出右下角。
// 2. 缩到 MIC_RESIZE_FLOOR 以下要重建窗口，不能 set_size。Electron 版实测
//    setBounds 只挪得动窗口边框，渲染面停在上一档不跟着缩（bounds 一路缩到
//    5×9，内容区却始终卡在 32×36），图标是 88.8889vmin，视口不缩图标就不缩。
//    这是「33 以下再调也没反应」的根因。WebView2 这边还没实测过同样的地板，
//    但两边都是同一个 Win32 窗口最小尺寸在兜底，规则先照搬。
// 3. 窗口必须不可激活。点一下就抢焦点的话，用户正在打字的那个窗口被顶掉，
//    光标一跑，听写结果就粘到别人家里去了。Tauri 的 .focused(false) 只管
//    「创建时不聚焦」，管不住后续点击，所以直接上 WS_EX_NOACTIVATE——Electron
//    的 focusable:false 在 Windows 上落到的也是这一位。
//
// 单位：除了两处 Win32 读工作区，全程用逻辑像素（DIP）。页面报上来的 screenX
// 本来就是 CSS 像素，设置文件里的 floatingMicPosition 也是 Electron 写的 DIP，
// 两边都对得上，中间不需要换算——只有 Win32 那两个 RECT 是物理像素，进门就除掉
// 缩放比。（和 pill.rs 的物理坐标相反：那边比的是全局光标位置，必须物理。）

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindowBuilder};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextW, IsWindowVisible, SetWindowPos,
    SystemParametersInfoW, GWL_EXSTYLE, GW_HWNDPREV, HWND_TOPMOST, SPI_GETWORKAREA, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WS_EX_TOPMOST,
};

pub const LABEL: &str = "floatmic";

const MIC_SIZE_MIN: f64 = 5.0;
const MIC_SIZE_MAX: f64 = 100.0;
const MIC_SIZE_DEFAULT: f64 = 72.0;
/// set_size 能如实缩到的最小边长。Electron 版实测地板在 30–32 之间浮动，取 36
/// 留一档余量：36 也是 Windows 开始如实照给高度的分界，两边刚好对上。
const MIC_RESIZE_FLOOR: f64 = 36.0;
const MIC_MARGIN: f64 = 6.0;

/// 当前窗口是按哪个尺寸建/调的（DIP，×100 存进整数）。0 = 没有窗口。
/// 跟目标尺寸比对，才知道这次要不要动窗口、动了会不会跨过地板。
static WIN_SIZE_X100: AtomicI32 = AtomicI32::new(0);
/// 本次拖动开始时窗口的实际物理尺寸，拖完拿它比对「这一趟有没有被改大」。
/// -1 = 不在拖动中。不能在建窗口时记：Windows 给小窗口的高度会在创建后自己再
/// settle 一次（Electron 版实测 20×20 先给 20×32、随后变 20×36），拿那个瞬时值
/// 当基准每次拖完都会误判。
static DRAG_W: AtomicI32 = AtomicI32::new(-1);
static DRAG_H: AtomicI32 = AtomicI32::new(-1);

// ── 工作区 ────────────────────────────────────────────────────────

/// 逻辑像素的可用区域（已去掉任务栏）。
#[derive(Clone, Copy, Debug)]
struct Area {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

fn to_logical(r: RECT, scale: f64) -> Area {
    Area {
        x: r.left as f64 / scale,
        y: r.top as f64 / scale,
        w: (r.right - r.left) as f64 / scale,
        h: (r.bottom - r.top) as f64 / scale,
    }
}

/// 主显示器的可用区域（物理像素）。分辨率会中途变（远程连进来就是一次），
/// 所以每次现读，不缓存。
pub(crate) fn primary_rect() -> RECT {
    let mut r = RECT::default();
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut r as *mut RECT as *mut std::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    r
}

/// 离某个物理坐标最近那块屏的可用区域。对应 Electron 的
/// screen.getDisplayNearestPoint(...).workArea；取不到就退回主屏。
///
/// 设置窗口也要用它验「上次那套坐标还落不落得进某块屏」，所以开到 crate 内。
pub(crate) fn rect_near(px: i32, py: i32) -> RECT {
    unsafe {
        let mon = MonitorFromPoint(POINT { x: px, y: py }, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(mon, &mut info).as_bool() {
            info.rcWork
        } else {
            primary_rect()
        }
    }
}

fn area_near(scale: f64, point: Option<(f64, f64)>) -> Area {
    match point {
        Some((x, y)) => to_logical(
            rect_near((x * scale).round() as i32, (y * scale).round() as i32),
            scale,
        ),
        None => to_logical(primary_rect(), scale),
    }
}

// ── 位置与比例 ────────────────────────────────────────────────────

// 下面四条 *_in 是纯函数：喂一个 Area 进去，不碰屏幕。位置这一块的坑全在数学
// 上（换设备、换缩放、贴边、可用区比图标还小），而那些情形没法靠插着一台显示器
// 来试——所以数学和读屏幕分开写，文件末尾的测试直接喂合成的 Area。
// 外面那四个同名函数只负责取当前的 Area 再转交。

fn clamp01(v: f64) -> f64 {
    v.max(0.0).min(1.0)
}

fn default_in(a: Area, size: f64) -> (f64, f64) {
    ((a.x + (a.w - size) / 2.0).round(), a.y + 18.0)
}

/// 把坐标拍进可用区域，四边各留 MIC_MARGIN。
/// 先 min 后 max：可用区比图标还窄时（不可能但别崩）结果落在左上角而不是负数，
/// 和 Electron 的 max(lo, min(raw, hi)) 同一个语义。
fn clamp_in(a: Area, size: f64, raw: (f64, f64)) -> (f64, f64) {
    (
        raw.0.min(a.x + a.w - size - MIC_MARGIN).max(a.x + MIC_MARGIN),
        raw.1.min(a.y + a.h - size - MIC_MARGIN).max(a.y + MIC_MARGIN),
    )
}

fn free_space(a: &Area, size: f64) -> (f64, f64) {
    ((a.w - size).max(0.0), (a.h - size).max(0.0))
}

fn anchor_in(a: Area, size: f64, p: (f64, f64)) -> (f64, f64) {
    let (fx, fy) = free_space(&a, size);
    (
        clamp01(if fx != 0.0 { (p.0 - a.x) / fx } else { 0.0 }),
        clamp01(if fy != 0.0 { (p.1 - a.y) / fy } else { 0.0 }),
    )
}

fn point_in(a: Area, size: f64, fx: f64, fy: f64) -> (f64, f64) {
    let (free_x, free_y) = free_space(&a, size);
    (
        (a.x + clamp01(fx) * free_x).round(),
        (a.y + clamp01(fy) * free_y).round(),
    )
}

// ── 上面那四条 + 当前屏幕 ────────────────────────────────────────

fn default_position(scale: f64, size: f64) -> (f64, f64) {
    default_in(area_near(scale, None), size)
}

/// 拍进**离它最近那块屏**的可用区域——先按坐标找屏，再 clamp。
fn clamp_position(scale: f64, size: f64, raw: Option<(f64, f64)>) -> (f64, f64) {
    let raw = match raw {
        Some((x, y)) if x.is_finite() && y.is_finite() => (x.round(), y.round()),
        _ => default_position(scale, size),
    };
    let half = (size / 2.0).round();
    clamp_in(area_near(scale, Some((raw.0 + half, raw.1 + half))), size, raw)
}

fn anchor_from(scale: f64, size: f64, p: (f64, f64)) -> (f64, f64) {
    let half = (size / 2.0).round();
    anchor_in(area_near(scale, Some((p.0 + half, p.1 + half))), size, p)
}

/// 还原时只能按主显示器算：比例里没有「当初在哪块屏」的信息。远程会话本来就只有
/// 一块虚拟显示器，主机接多屏时的退化行为是「回到主屏的同一相对位置」，可以接受。
fn point_from_anchor(scale: f64, size: f64, anchor: Option<(f64, f64)>) -> Option<(f64, f64)> {
    let (fx, fy) = anchor?;
    if !fx.is_finite() || !fy.is_finite() {
        return None;
    }
    Some(point_in(area_near(scale, None), size, fx, fy))
}

// ── 设置 ──────────────────────────────────────────────────────────

/// 步长 1：5–100 之间随意取值，也免掉「最大值不是步长整数倍就永远够不到」的坑。
fn normalize_size(v: Option<&serde_json::Value>) -> f64 {
    let parsed = v.and_then(|v| v.as_f64());
    match parsed {
        Some(n) if n.is_finite() => n.round().max(MIC_SIZE_MIN).min(MIC_SIZE_MAX),
        _ => MIC_SIZE_DEFAULT,
    }
}

fn read_point(v: Option<&serde_json::Value>) -> Option<(f64, f64)> {
    let o = v?.as_object()?;
    Some((o.get("x")?.as_f64()?, o.get("y")?.as_f64()?))
}

fn read_anchor(v: Option<&serde_json::Value>) -> Option<(f64, f64)> {
    let o = v?.as_object()?;
    Some((o.get("fx")?.as_f64()?, o.get("fy")?.as_f64()?))
}

/// 绝对坐标继续存：设置页要显示它，而且比例还原不出「当初在哪块屏」。
/// 比例是真正的还原依据，绝对坐标只在没有比例时（旧配置）兜底。
/// 撞上撕裂读时 settings::merge 返回 None 并跳过写入；位置只是下次启动的恢复值，
/// 丢一次无所谓。
fn persist_position(app: &AppHandle, scale: f64, size: f64, point: (f64, f64)) -> (f64, f64) {
    let p = clamp_position(scale, size, Some(point));
    let (fx, fy) = anchor_from(scale, size, p);
    let pos = serde_json::json!({ "x": p.0.round() as i64, "y": p.1.round() as i64 });
    let mut patch = serde_json::Map::new();
    patch.insert("floatingMicPosition".into(), pos.clone());
    patch.insert("floatingMicAnchor".into(), serde_json::json!({ "fx": fx, "fy": fy }));
    crate::settings::merge(patch);
    // 设置页开着的时候它手里那份 st 是开窗那一刻的快照，拖动写进文件它并不知道。
    // 不告诉它的话，用户拖完图标再在设置页随手改一个开关，save() 就把旧坐标整个
    // 盖回去——图标自己跳回原位，而且没有任何报错。settings_save 那边剔掉这三个
    // 键只挡住了「设置页覆盖」，挡不住「设置页的快照本身是旧的」。
    let _ = app.emit_to(crate::settingswin::LABEL, "localless://floating-mic-position", pos);
    p
}

// ── 窗口 ──────────────────────────────────────────────────────────

/// 这一步不能省，理由见文件头第 3 条。挂不上只是「点它会抢焦点」，不该让整个
/// 窗口建不出来，所以失败只记一行——那一行在 pill 那边。
///
/// 曾经是就地 `GWL_EXSTYLE |= WS_EX_NOACTIVATE`，实测挂不住：量回来 0x00040018，
/// 那一位压根不在，tao 在 build() 之后按自己缓存的 flags 把整个扩展样式重写了一遍。
/// 也就是说这扇窗一直是能抢焦点的，点一下悬浮麦就把用户正在打字的窗口顶掉。
/// 现在统一走 pill::keep_overlay_ex（顺带把它从 APPWINDOW 改成 TOOLWINDOW），
/// 真正保证它不被写回去的是巡逻线程里那一句。
fn make_unfocusable(window: &tauri::WebviewWindow) {
    crate::pill::keep_overlay_ex(window);
}

/// 置顶看门狗：只有当真有窗口压在图标上面时才重新抬一次。
///
/// `always_on_top` 落到的是 WS_EX_TOPMOST，可置顶层内部仍然分先后——谁最后
/// SetWindowPos 谁在最上面。用户桌面上常驻的另一个置顶浮层（Hover Note）自己
/// 抬一次，图标就被压在下面了：图标还在、日志干净、就是点不着。
///
/// 所以按需抬，不定时抬。无条件每轮 SetWindowPos 会和对方形成互抬，两个浮层
/// 轮流跳到最上面，屏幕上就是一直在闪；只有发现「上面确实压着东西」才动手，
/// 这个循环才会收敛。没动手的那些轮次一行日志都不写——700ms 一圈，记日志就是
/// 刷屏，而且会把真正动了手的那几行埋掉。见记忆里那条「监测不动手时完全不
/// 写日志，安静≠坏了」。
const TOP_POLL_MS: u64 = 700;

static TOP_RUNNING: AtomicBool = AtomicBool::new(false);

/// 沿 z 序往上走，找第一个**压在图标矩形上**的可见置顶窗口。
///
/// - 只认 WS_EX_TOPMOST 的：非置顶窗口本来就排在置顶层下面，它们出现在 z 序
///   上方只是切换过程中的一瞬，照着抬反而会互抬。
/// - 只认矩形相交的：另一块屏、另一个角上的置顶窗口压不着图标，不关我们的事。
/// - 走不动就停，另外压一个上限——z 序是系统在改的链表，边走边变，没有上限的
///   循环理论上可以不回来。
unsafe fn covered_by(hwnd: HWND) -> Option<String> {
    let mut me = RECT::default();
    if GetWindowRect(hwnd, &mut me).is_err() {
        return None;
    }
    let mut cur = hwnd;
    for _ in 0..400 {
        let Ok(prev) = GetWindow(cur, GW_HWNDPREV) else {
            return None;
        };
        if prev.is_invalid() {
            return None;
        }
        cur = prev;
        if !IsWindowVisible(cur).as_bool() {
            continue;
        }
        if GetWindowLongPtrW(cur, GWL_EXSTYLE) & WS_EX_TOPMOST.0 as isize == 0 {
            continue;
        }
        let mut r = RECT::default();
        if GetWindowRect(cur, &mut r).is_err() {
            continue;
        }
        if r.right <= me.left || r.left >= me.right || r.bottom <= me.top || r.top >= me.bottom {
            continue;
        }
        let mut buf = [0u16; 128];
        let n = GetWindowTextW(cur, &mut buf) as usize;
        return Some(String::from_utf16_lossy(&buf[..n.min(buf.len())]));
    }
    None
}

/// 起看门狗。重复调只会有一条线程在跑。
fn start_topmost(app: &AppHandle) {
    if TOP_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        while TOP_RUNNING.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(TOP_POLL_MS));
            // 窗口没了就什么都别做。藏着（micwatch 按前台程序在收放）时只补样式，
            // 不抬层级——看不见的窗口谈不上被谁压住。
            let Some(w) = app.get_webview_window(LABEL) else {
                continue;
            };
            // 样式这一刀跟显隐无关，藏着的时候也得盯——下次露脸就晚了。
            crate::pill::keep_frameless(&w, false);
            crate::pill::keep_overlay_ex(&w);
            if !w.is_visible().unwrap_or(false) {
                continue;
            }
            let Ok(raw) = w.hwnd() else { continue };
            let hwnd = HWND(raw.0 as *mut std::ffi::c_void);
            unsafe {
                let Some(who) = covered_by(hwnd) else { continue };
                if let Err(e) = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                ) {
                    eprintln!("[悬浮麦] 重新置顶失败：{e}");
                } else {
                    eprintln!("[悬浮麦] 被「{who}」压住，已重新置顶");
                }
            }
        }
    });
}

/// 停看门狗。设置里关掉悬浮麦时走这条，别留一条线程 700ms 空转。
fn stop_topmost() {
    TOP_RUNNING.store(false, Ordering::SeqCst);
}

/// Windows 给小窗口的高度额外加料：Electron 版实测请求 20×20 得到 20×24、
/// 30×30 得到 30×34，36px 以上才如实照给。这里不去追平它——规则是
/// min(请求高+4, max(请求高,36))，在 33–35 这几档按差值回退根本不收敛（请求 35
/// 得 36，退到 34 仍得 36，实际要退到 31），追平就得循环 set_size，每次都可能闪。
/// 图标本身用 vmin 定尺寸，窗口不是正方它照样是正圆；多出来的几像素是透明留白，
/// 而按钮本来就只占 88.89%，那圈留白一直都在。
fn create(app: &AppHandle, point: (f64, f64), size: f64, scale: f64) -> tauri::Result<()> {
    let w = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("mic.html".into()))
        .title("Localless 麦克风")
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(false)
        .resizable(false)
        .minimizable(false)
        .maximizable(false)
        .focused(false)
        .inner_size(size, size)
        .position(point.0, point.1)
        .build()?;
    make_unfocusable(&w);
    // 建窗时给的那对逻辑坐标/尺寸靠不住：窗口还没落到哪块屏上，tao 只能按
    // 缩放比 1 换算，于是 200% 的屏上位置差一倍、尺寸也只有一半（实测请求
    // 逻辑 40×40 @1773,1027 得到物理 131×40 @1773,1027）。建完再用物理值
    // 摆一次，这时候缩放比是确定的。
    place(&w, point, size, scale);
    // 它和药丸一样是 decorations(false)，样式里照样挂着 WS_CAPTION（实测
    // 0x04C80000）。不删的话谁碰一下它的样式就可能闪一条写着「Localless 麦克风」
    // 的标题栏出来。理由见 pill::no_frame。
    //
    // 放在 place 后面：tao 那几个设尺寸/位置的方法自己也会写 GWL_STYLE。真正
    // 保证它不被写回去的是巡逻线程里那句 keep_frameless，这里只是让第一帧就对。
    crate::pill::no_frame(&w);
    // 药丸上那条「Localless」标题栏在这里也会冒出来（写的是「Localless 麦克风」），
    // 同一个 DefWindowProc 画的，同一道子类挡掉。
    crate::pill::no_caption_paint(&w);
    WIN_SIZE_X100.store((size * 100.0).round() as i32, Ordering::Relaxed);
    Ok(())
}

/// 用物理像素摆窗口。DIP 那一层的换算全在这里做完，Tauri 那边不留任何
/// 「这个数到底是逻辑还是物理」的歧义。
fn place(w: &tauri::WebviewWindow, point: (f64, f64), size: f64, scale: f64) {
    let px = (size * scale).round().max(1.0) as u32;
    if let Err(e) = w.set_size(PhysicalSize::new(px, px)) {
        eprintln!("[悬浮麦] 设尺寸失败：{e}");
    }
    let pos = PhysicalPosition::new((point.0 * scale).round() as i32, (point.1 * scale).round() as i32);
    if let Err(e) = w.set_position(pos) {
        eprintln!("[悬浮麦] 设位置失败：{e}");
    }
    let outer = w.outer_size().ok();
    let at = w.outer_position().ok();
    eprintln!(
        "[悬浮麦] 摆位 逻辑 {size}×{size} @{:.0},{:.0} · 缩放 {scale} → 请求物理 {px}×{px} @{},{} · 实得 {:?} @{:?}",
        point.0, point.1, pos.x, pos.y,
        outer.map(|s| (s.width, s.height)), at.map(|p| (p.x, p.y))
    );
}

/// 只有起点和终点都在地板以上，set_size 才靠得住；任一端在下面都得重建。
fn can_resize_in_place(from: f64, to: f64) -> bool {
    from >= MIC_RESIZE_FLOOR && to >= MIC_RESIZE_FLOOR
}

/// 缩放比。多屏各自缩放不同时这里只取一个值——悬浮麦真正要伺候的场景（远程
/// 连进来的单块虚拟显示器）永远只有一块屏，先不为多屏异构缩放增加复杂度。
///
/// 现问显示器，不读窗口的 scale_factor()：那是 tao 的缓存，远程切屏时不更新，
/// 见 display.rs 文件头。
fn scale_of(app: &AppHandle) -> f64 {
    app.get_webview_window(LABEL)
        .map(|w| crate::display::live_scale(&w))
        .or_else(|| {
            app.primary_monitor()
                .ok()
                .flatten()
                .map(|m| m.scale_factor())
        })
        .unwrap_or(1.0)
}

/// 读设置 → 该关就关，该建就建，该挪就挪。每条改设置的路径最后都要经过它。
///
/// 「此刻该不该露脸」不在这里判：那是 micwatch 的事（前台程序过滤、仅远程时
/// 显示）。这里只管窗口存不存在、多大、在哪，末尾交给 micwatch::sync 去起停
/// 监测并决定显隐——那一步用 hide/show，不销毁窗口，录音状态才不会被掐断。
pub fn sync(app: &AppHandle) {
    let s = crate::settings::read();
    let size = normalize_size(s.get("floatingMicSize"));
    let enabled = s.get("floatingMicEnabled").and_then(|v| v.as_bool()).unwrap_or(false);
    // 托盘那个「显示悬浮麦克风」的勾选态跟这个键走。改设置的路径全都汇到这里，
    // 所以勾选态也只在这里对齐一次，免得每条路径各记各的、迟早漏一条。
    crate::tray::refresh(app);

    if !enabled {
        stop_topmost();
        if let Some(w) = app.get_webview_window(LABEL) {
            let _ = w.close();
            WIN_SIZE_X100.store(0, Ordering::Relaxed);
            eprintln!("[悬浮麦] 已关闭");
        }
        // 窗口都没了，两个监测还在 450ms/3s 空转就是白烧电。
        crate::micwatch::sync(app);
        return;
    }

    let scale = scale_of(app);
    // 优先按比例还原：远程换了设备、或者缩放改了，屏幕尺寸跟着变，比例还能落回
    // 同一个相对位置。没有比例的旧配置退回绝对坐标，并就地补一份比例出来。
    let requested = read_point(s.get("floatingMicPosition"));
    let anchor = read_anchor(s.get("floatingMicAnchor"));
    let anchored = point_from_anchor(scale, size, anchor);
    let point = clamp_position(scale, size, anchored.or(requested));
    let needs_backfill = anchor.is_none() && requested.is_some();

    let win_size = WIN_SIZE_X100.load(Ordering::Relaxed) as f64 / 100.0;
    match app.get_webview_window(LABEL) {
        None => {
            if let Err(e) = create(app, point, size, scale) {
                eprintln!("[悬浮麦] 建窗失败：{e}");
                return;
            }
        }
        Some(w) if win_size != size && !can_resize_in_place(win_size, size) => {
            // 过地板就重建，理由见文件头第 2 条。重建只在设置页改尺寸时发生。
            eprintln!("[悬浮麦] {win_size} → {size} 跨过 {MIC_RESIZE_FLOOR}，重建窗口");
            let _ = w.close();
            WIN_SIZE_X100.store(0, Ordering::Relaxed);
            if let Err(e) = create(app, point, size, scale) {
                eprintln!("[悬浮麦] 重建失败：{e}");
                return;
            }
        }
        Some(w) => {
            place(&w, point, size, scale);
            let _ = w.set_always_on_top(true);
            WIN_SIZE_X100.store((size * 100.0).round() as i32, Ordering::Relaxed);
        }
    }

    start_topmost(app);

    // 补比例不看有没有被 clamp 动过：旧配置的坐标可能一点没动，但比例仍然得写
    // 出去，否则下次换设备还是只能拿绝对坐标兜底。
    if needs_backfill
        || requested.map_or(false, |r| r.0.round() != point.0 || r.1.round() != point.1)
    {
        persist_position(app, scale, size, point);
    }

    // 摆好之后才决定露不露脸：上面可能刚重建过窗口，新窗口默认是显示的。
    crate::micwatch::sync(app);
}

/// 回默认位置（顶部居中）。对应 Electron 的 localless:reset-floating-mic。
/// 设置页上那个「重置位置」按钮走这条——用户把图标拖到某块拔掉了的副屏上、
/// 或者远程换了设备之后找不着它了，这是唯一一条不靠鼠标就能把它找回来的路。
///
/// 先落盘再挪窗口：窗口可能根本没开（设置里关着悬浮麦），那种情况下也得把
/// 位置存正，否则下次打开还在老地方。
pub fn reset_position(app: &AppHandle) -> (f64, f64) {
    let scale = scale_of(app);
    let size = normalize_size(crate::settings::read().get("floatingMicSize"));
    let p = persist_position(app, scale, size, default_position(scale, size));
    if let Some(w) = app.get_webview_window(LABEL) {
        let _ = w.set_position(PhysicalPosition::new(
            (p.0 * scale).round() as i32,
            (p.1 * scale).round() as i32,
        ));
    }
    p
}

// ── 页面那三条命令 ────────────────────────────────────────────────
//
// 每条都先认窗口。Electron 版是 event.sender !== micWin.webContents → 'invalid-source'，
// 这边比 label：别的页面（设置页、药丸）不该能隔空搬动这个窗口。

fn mine(window: &tauri::WebviewWindow) -> bool {
    window.label() == LABEL
}

#[tauri::command]
pub fn mic_toggle(app: AppHandle, window: tauri::WebviewWindow) -> Result<(), String> {
    if !mine(&window) {
        return Err("invalid-source".into());
    }
    crate::recorder::toggle(&app, "floating-mic");
    Ok(())
}

#[tauri::command]
pub fn mic_move(app: AppHandle, window: tauri::WebviewWindow, x: f64, y: f64) {
    if !mine(&window) {
        return;
    }
    // 本次拖动的第一帧：记下起始物理尺寸做基准。
    mark_drag_start(&window);
    let scale = scale_of(&app);
    let size = normalize_size(crate::settings::read().get("floatingMicSize"));
    let p = clamp_position(scale, size, Some((x, y)));
    // 只给位置，不碰尺寸。Electron 版在这里必须显式钉住尺寸：它的 setPosition
    // 会把窗口当前的物理尺寸读回来换算成 DIP、再换算回物理像素写下去，缩放比
    // 不是整数倍时两次取整的误差是单向的，一次拖动几百帧就累积成「跟着鼠标慢慢
    // 变大」。这边直接给物理坐标，中间不过 DIP 那一手，累积链天生就断了——
    // 下面拖完还是要自检一次，万一不是。
    let _ = window.set_position(PhysicalPosition::new(
        (p.0 * scale).round() as i32,
        (p.1 * scale).round() as i32,
    ));
}

fn mark_drag_start(window: &tauri::WebviewWindow) {
    if DRAG_W.load(Ordering::Relaxed) < 0 {
        if let Ok(sz) = window.outer_size() {
            DRAG_W.store(sz.width as i32, Ordering::Relaxed);
            DRAG_H.store(sz.height as i32, Ordering::Relaxed);
        }
    }
}

/// 触控拖动走这条，给的是「手指离按下那一点偏了多少」（CSS 像素），不是屏幕坐标。
///
/// 触控事件的 screenX 在窗口被挪动时靠不住：它是「客户区坐标 + WebView2 缓存的
/// 窗口原点」，而原点要等 NotifyParentWindowPositionChanged 异步传过去才更新。
/// 窗口刚被挪到手指下面，下一个事件还按旧原点算，screenX 就退回按下时的值——
/// 窗口被拽回原位，再下一个事件又拽过去，就是「在原位和手指之间来回闪」。
/// 鼠标事件的 screenX 直接取自系统，所以鼠标拖没事。
///
/// 客户区坐标是按窗口真实位置算的，拿它和窗口此刻的真实位置相加就不依赖那份
/// 缓存。页面保证一次只有一条在路上、并丢掉窗口挪动之前生成的事件，所以这里的
/// outer_position 就是那个事件被换算时窗口所在的位置。
///
/// 回的是摆到的逻辑坐标，拖完落盘用。
#[tauri::command]
pub fn mic_nudge(app: AppHandle, window: tauri::WebviewWindow, dx: f64, dy: f64) -> Option<(f64, f64)> {
    if !mine(&window) || !dx.is_finite() || !dy.is_finite() {
        return None;
    }
    mark_drag_start(&window);
    let scale = scale_of(&app);
    let size = normalize_size(crate::settings::read().get("floatingMicSize"));
    let at = window.outer_position().ok()?;
    let from = (at.x as f64 / scale, at.y as f64 / scale);
    let p = clamp_position(scale, size, Some((from.0 + dx, from.1 + dy)));
    let _ = window.set_position(PhysicalPosition::new(
        (p.0 * scale).round() as i32,
        (p.1 * scale).round() as i32,
    ));
    Some(p)
}

#[tauri::command]
pub fn mic_drag_end(app: AppHandle, window: tauri::WebviewWindow, x: f64, y: f64) {
    if !mine(&window) {
        return;
    }
    let scale = scale_of(&app);
    let size = normalize_size(crate::settings::read().get("floatingMicSize"));
    let p = clamp_position(scale, size, Some((x, y)));
    let _ = window.set_position(PhysicalPosition::new(
        (p.0 * scale).round() as i32,
        (p.1 * scale).round() as i32,
    ));
    let start = (DRAG_W.swap(-1, Ordering::Relaxed), DRAG_H.swap(-1, Ordering::Relaxed));
    persist_position(&app, scale, size, p);
    // 拖完自检：这一趟窗口有没有被改大。触摸端的捏合缩放是另一条路（那条不改
    // 窗口尺寸，这里看不见）。真要变了，sync 会按设置里的尺寸重新摆正。
    // 只在拖完做：拖动中重建会丢掉 pointer capture，把这次拖动直接掐断。
    if start.0 >= 0 {
        if let Ok(now) = window.outer_size() {
            if now.width as i32 != start.0 || now.height as i32 != start.1 {
                eprintln!(
                    "[悬浮麦] 拖动中尺寸变了 {}x{} → {}x{}，重新同步",
                    start.0, start.1, now.width, now.height
                );
                sync(&app);
            }
        }
    }
}

/// 设置页改完尺寸/开关之后叫一声。
#[tauri::command]
pub fn mic_sync(app: AppHandle) {
    sync(&app);
}

// ── 位置数学的测试 ────────────────────────────────────────────────
//
// 这一块要伺候的情形全都没法靠插一台显示器试出来：从 1920×1080 的远程换成
// iPad 的 2420×1668、缩放从 100% 改成 200%、旧配置只有绝对坐标没有比例。
// 所以喂合成的可用区域，直接验那几条不变量。
#[cfg(test)]
mod tests {
    use super::*;

    const A: Area = Area { x: 0.0, y: 0.0, w: 1920.0, h: 1040.0 };
    /// 原点不是 0,0 的第二块屏——比例是相对于可用区的，不是相对于屏幕原点的。
    const B: Area = Area { x: -1080.0, y: 120.0, w: 1210.0, h: 834.0 };

    fn close(a: (f64, f64), b: (f64, f64)) -> bool {
        (a.0 - b.0).abs() <= 1.0 && (a.1 - b.1).abs() <= 1.0
    }

    /// 四个角的比例必须是 0/1 的组合，换屏之后还解回同一个角。
    /// 这正是「贴右下角的图标换台设备连过来跑到屏幕中间」那个 bug 的判据。
    #[test]
    fn 四角换屏后还在四角() {
        let size = 72.0;
        for (fx, fy) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
            let on_a = point_in(A, size, fx, fy);
            // A 上算出来的比例
            assert!(close(anchor_in(A, size, on_a), (fx, fy)), "A 角 {fx},{fy} 比例对不上");
            // 换到 B：还是同一个角
            let on_b = point_in(B, size, fx, fy);
            let want = (
                B.x + fx * (B.w - size),
                B.y + fy * (B.h - size),
            );
            assert!(close(on_b, want), "B 角 {fx},{fy}：{on_b:?} != {want:?}");
            // 而且落在 B 里面（clamp 不该再动它一下）——四边留 MARGIN=6，
            // 贴死角的点会被 clamp 拉进来 6px，所以这里只验没被拉出可用区。
            let c = clamp_in(B, size, on_b);
            assert!(c.0 >= B.x && c.0 + size <= B.x + B.w, "B 角 {fx},{fy} clamp 后出界");
            assert!(c.1 >= B.y && c.1 + size <= B.y + B.h, "B 角 {fx},{fy} clamp 后出界");
        }
    }

    /// 比例 → 坐标 → 比例，一圈下来不该漂。漂了的话每次开机图标都往一个方向挪。
    #[test]
    fn 比例往返不漂() {
        for size in [5.0, 36.0, 72.0, 100.0] {
            for i in 0..=20 {
                let f = i as f64 / 20.0;
                let p = point_in(A, size, f, 1.0 - f);
                let back = anchor_in(A, size, p);
                assert!(close(
                    (back.0 * (A.w - size), back.1 * (A.h - size)),
                    (f * (A.w - size), (1.0 - f) * (A.h - size))
                ), "size={size} f={f} 漂了：{back:?}");
            }
        }
    }

    /// 界外的坐标要被拉回来，而且拉回来之后整个图标都在可用区里。
    /// 旧版没有比例时就只有这一条兜底，拉歪了就是「图标贴在右边某处」。
    #[test]
    fn 界外坐标拉回可用区() {
        let size = 72.0;
        for raw in [
            (-9999.0, -9999.0),
            (9999.0, 9999.0),
            (A.w + 10.0, -3.0),
            (-3.0, A.h + 10.0),
        ] {
            let p = clamp_in(A, size, raw);
            assert!(p.0 >= A.x + MIC_MARGIN, "{raw:?} → {p:?} 左边出界");
            assert!(p.1 >= A.y + MIC_MARGIN, "{raw:?} → {p:?} 上边出界");
            assert!(p.0 + size <= A.x + A.w - MIC_MARGIN, "{raw:?} → {p:?} 右边出界");
            assert!(p.1 + size <= A.y + A.h - MIC_MARGIN, "{raw:?} → {p:?} 下边出界");
            // 幂等：clamp 两次和一次一样。
            assert_eq!(clamp_in(A, size, p), p, "{raw:?} clamp 不幂等");
        }
    }

    /// 可用区比图标还小的退化情形（图标 100、可用区 80）。要落在左上角，
    /// 不能是负数，更不能 NaN——比例那边 free=0，除法必须被绕开。
    #[test]
    fn 可用区比图标还小也不炸() {
        let tiny = Area { x: 10.0, y: 20.0, w: 80.0, h: 80.0 };
        let size = 100.0;
        let p = clamp_in(tiny, size, (500.0, 500.0));
        assert_eq!(p, (tiny.x + MIC_MARGIN, tiny.y + MIC_MARGIN));
        let a = anchor_in(tiny, size, p);
        assert_eq!(a, (0.0, 0.0));
        assert!(a.0.is_finite() && a.1.is_finite());
        assert_eq!(point_in(tiny, size, 1.0, 1.0), (tiny.x, tiny.y));
    }

    /// 比例里混进脏数据（NaN、超出 0–1、旧版写坏的负数）不能把坐标带跑。
    #[test]
    fn 脏比例被夹住() {
        let size = 72.0;
        assert_eq!(point_in(A, size, -5.0, 2.0), point_in(A, size, 0.0, 1.0));
        assert!(point_from_anchor(1.0, size, Some((f64::NAN, 0.5))).is_none());
        assert!(point_from_anchor(1.0, size, None).is_none());
    }

    /// 默认位置：顶部居中，和 Electron 的 defaultMicPosition 一致。
    #[test]
    fn 默认位置顶部居中() {
        let size = 72.0;
        assert_eq!(default_in(A, size), ((A.w - size) / 2.0, 18.0));
        assert_eq!(default_in(B, size), ((B.x + (B.w - size) / 2.0).round(), B.y + 18.0));
    }

    /// 尺寸归一化：非法值退回默认，越界夹到 5–100，小数四舍五入。
    #[test]
    fn 尺寸归一化() {
        use serde_json::json;
        assert_eq!(normalize_size(None), MIC_SIZE_DEFAULT);
        assert_eq!(normalize_size(Some(&json!("大一点"))), MIC_SIZE_DEFAULT);
        assert_eq!(normalize_size(Some(&json!(null))), MIC_SIZE_DEFAULT);
        assert_eq!(normalize_size(Some(&json!(0))), MIC_SIZE_MIN);
        assert_eq!(normalize_size(Some(&json!(9999))), MIC_SIZE_MAX);
        assert_eq!(normalize_size(Some(&json!(40.4))), 40.0);
        assert_eq!(normalize_size(Some(&json!(40.6))), 41.0);
    }

    /// 地板规则：两端都在 36 以上才准原地改尺寸，任一端在下面都得重建窗口。
    #[test]
    fn 过地板要重建() {
        assert!(can_resize_in_place(72.0, 40.0));
        assert!(can_resize_in_place(36.0, 36.0));
        assert!(!can_resize_in_place(72.0, 20.0));
        assert!(!can_resize_in_place(20.0, 72.0));
        assert!(!can_resize_in_place(20.0, 10.0));
    }
}

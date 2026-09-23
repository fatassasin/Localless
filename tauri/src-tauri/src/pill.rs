// 药丸窗口的大小和点击边界。这是转 Tauri 的第二个风险点。
//
// Electron 版是这么做的（main.js:686 / 1222，preload.cjs:1784）：
//   窗口铺满整个 workArea、透明、置顶，默认 setIgnoreMouseEvents(true, {forward:true})。
//   forward 这个参数是关键——它让窗口在穿透状态下**仍然**把 mousemove 送进渲染
//   进程，于是药丸元素的 mouseenter/mouseleave 能正常触发，回头再 IPC 让主进程
//   临时关掉穿透。
//
// Tauri 的 set_ignore_cursor_events 没有 forward，穿透一开页面就彻底收不到鼠标，
// 悬停检测整条路断掉。第一版的补法是从 Rust 轮询全局光标，跨进/跨出药丸矩形的
// 那一帧才翻穿透——照抄了 Electron 的形状，也照抄了它的病：
//
//   **悬停这个阶段在触摸屏上根本不存在。** 手指落下的那一刻窗口还是穿透的，
//   这一下直接落到底下的程序里；等这边轮询发现光标进了药丸再放行，那一下已经
//   没了。悠悠远程上「用手指根本点不到确认键」就是这么来的，跟远程软件无关。
//
// 第二版改成给铺满屏的窗口套一个 SetWindowRgn 区域，区域就是药丸那一小块。判定
// 对了，但「铺满屏」这个底子没动——于是远程上又出了一次「听写时整个屏幕点不了」，
// 而屏幕上是空的，没有任何东西能解释为什么点不动。区域这条命脉只要有一环没生效
// （远程注入的那一下走的是不是同一条命中测试、DPI 变了之后坐标还对不对、
// SetWindowRgn 本身失没失败），代价就是整块屏幕。
//
// 所以现在**窗口本身只有药丸这么大**：页面把药丸的外接矩形报上来，窗口就调成
// 「矩形 + 四周 SHADOW_PAD」那么大，摆到 workArea 底部居中。区域留着当第二道
// 保险（藏起来时清空），但就算它整条失效，能被吃掉的也只有药丸自己那一小块。
// 这是用户定下的规矩：只遮药丸大小。
//
// 连带解决的两件事：
// - 分辨率/缩放中途变（远程连进来最常见）。旧版的 fit() 只在建窗口时跑一次，
//   之后窗口大小、scale_factor、inner_size 全是旧的，区域按旧尺寸裁，药丸落在
//   屏幕外。现在每次显示都现读 workArea 和缩放比，重新摆一遍。
// - 置顶丢了就等于隐身。每次从藏到显都重挂一次 always_on_top。
//
// 页面那侧配套的一点：药丸的宽度必须是 width:max-content。fixed 定位元素的包含
// 块是视口，窗口一小，shrink-to-fit 就会把它压扁，量出来的宽度不再是内容的自然
// 宽度——窗口跟着这个宽度再缩一次，就成了自激。max-content 让它和视口无关。

use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{PhysicalPosition, PhysicalSize};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMNCRP_DISABLED, DWMWA_NCRENDERING_POLICY,
    DWMWA_TRANSITIONS_FORCEDISABLED,
};
use windows::Win32::Graphics::Gdi::{CreateRectRgn, DeleteObject, SetWindowRgn};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, SystemParametersInfoW, GWL_EXSTYLE,
    GWL_STYLE, SPI_GETWORKAREA, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WS_CAPTION, WS_EX_APPWINDOW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_MAXIMIZEBOX, WS_MINIMIZEBOX,
    WM_NCACTIVATE, WM_SETICON, WM_SETTEXT, WS_POPUP, WS_SYSMENU, WS_THICKFRAME, WS_VISIBLE,
};

/// 覆盖层的扩展样式：该有的和绝不能有的。
///
/// 药丸和悬浮麦克风都不是「窗口」，是画在别人上面的两块东西。可实测它们的
/// GWL_EXSTYLE 里挂的是 **WS_EX_APPWINDOW**（药丸 0x08040038、悬浮麦 0x00040018），
/// 没有 WS_EX_TOOLWINDOW——对 Shell 来说这是两扇正经应用窗口：进 Alt-Tab、有任务栏
/// 语义、点上去就走完整套窗口激活流程。之前那条标题栏、以及点药丸之后输入框失焦，
/// 根子都在这里；光删样式位只是让它「画不出来」，它本身还是窗口。
///
/// WS_EX_TOOLWINDOW 才是 Electron 版那种覆盖层：不进 Alt-Tab，不参与任务栏，
/// 永远不画标题栏。加上 WS_EX_NOACTIVATE（点了也不抢焦点），这两扇窗才真正
/// 「不是窗口」。
///
/// 这两位和窗口样式一样保不住——tao 每次 set_visible / 设尺寸都按自己缓存的
/// flags 重写整个 GWL_EXSTYLE。实测悬浮麦克风建完挂的 WS_EX_NOACTIVATE 就是这么
/// 没的（量回来 0x00040018，那一位压根不在），也就是说点一下悬浮麦就会把用户正在
/// 打字的窗口顶掉。所以和边框一样，得有地方重放。
const OVERLAY_EX_ON: isize = (WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0) as isize;
const OVERLAY_EX_OFF: isize = WS_EX_APPWINDOW.0 as isize;

/// 自动隐藏的任务栏靠鼠标碰到屏幕最底边唤起。开了自动隐藏时 workArea 就是整块
/// 屏幕，窗口贴到最底下就把那一行像素占住了，任务栏再也划不出来——置顶窗口挡的
/// 是「边缘占用」，点击穿透救不了这个。底部留一条缝把边缘还回去。
const EDGE_GAP: i32 = 3;

/// 药丸的 box-shadow 是 `0 14px 44px`，糊在 getBoundingClientRect 之外。窗口按这个
/// 数往外放一圈，不然阴影会被窗口边缘裁出一道硬边。CSS 像素。
///
/// **和 pill.html 里的 `--ll-pad` 是同一个数**，那边拿它当药丸在窗口里的左上角偏移。
/// 两边对不上，药丸就会贴着窗口边或者露出一条空白。
///
/// 代价是药丸周围这一圈看不见的地方也吃点击。44px 的模糊到最外沿已经淡到看不
/// 出来，所以放得比阴影实际范围小一点，换那一圈少占些地方。
const SHADOW_PAD: f64 = 28.0;

/// 还没量到药丸之前窗口的大小（CSS 像素）。这段时间区域是空的，屏幕上什么都
/// 没有，这个数只是给 WebView2 一个像样的视口——别让页面在 1×1 的视口里排版。
const START_W: f64 = 420.0;
const START_H: f64 = 150.0;

/// 窗口当前认不认点击。只用来判断要不要再写一次扩展样式和重挂置顶——那两下都要
/// 过一遍事件循环，而 pill_rect 是跟着 rAF 报上来的。
static CLICKABLE: AtomicBool = AtomicBool::new(false);

/// 主显示器的可用区域（去掉任务栏）。远程/超宽屏的原点未必是 0,0，分辨率也会
/// 中途变，所以每次都现读，不缓存。
fn work_area() -> RECT {
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

/// 给窗口套区域。坐标是**窗口自己**的左上角起算。
///
/// `None` = 空区域 = 整个窗口一个像素都不占。窗口现在只有药丸那么大，所以显示
/// 时给的就是「整窗」，区域退化成一道保险：万一哪天窗口又被谁撑大了，藏起来的
/// 那一路照样是零占用。
fn set_region(window: &tauri::WebviewWindow, rect: Option<(i32, i32, i32, i32)>) -> bool {
    let Ok(raw) = window.hwnd() else {
        eprintln!("[药丸] 拿不到 HWND，区域没设上");
        return false;
    };
    let hwnd = HWND(raw.0 as *mut std::ffi::c_void);
    let (x1, y1, x2, y2) = rect.unwrap_or((0, 0, 0, 0));
    unsafe {
        let rgn = CreateRectRgn(x1, y1, x2, y2);
        if rgn.is_invalid() {
            eprintln!("[药丸] CreateRectRgn 失败");
            return false;
        }
        // 成功之后区域归系统所有，不能再碰；失败了它还在我们手上，得自己删，
        // 不然每帧漏一个 GDI 对象，几分钟就把配额耗光。
        if SetWindowRgn(hwnd, Some(rgn), true) == 0 {
            eprintln!("[药丸] SetWindowRgn 失败");
            let _ = DeleteObject(rgn.into());
            return false;
        }
    }
    true
}

/// 撤掉区域，窗口恢复成整块可见。`SetWindowRgn(hwnd, None, ..)` 和给一个区域是
/// 两回事：前者是「没有区域」，后者哪怕给整窗大小，往后窗口一改尺寸区域也不会
/// 跟着长——所以恢复必须走 None，不能拿当前尺寸再套一遍。
fn clear_region(window: &tauri::WebviewWindow) {
    let Ok(raw) = window.hwnd() else { return };
    let hwnd = HWND(raw.0 as *mut std::ffi::c_void);
    unsafe {
        if SetWindowRgn(hwnd, None, true) == 0 {
            eprintln!("[药丸] 撤区域失败");
        }
    }
}

/// 把窗口整块挡住，一个像素都不上屏。
///
/// 给的是别人家的窗口也照样管用——区域是纯 Win32 的东西，不在 tao 缓存的那套
/// WindowFlags 里，所以 tao 后面怎么重放样式都不会把它顶掉。设置窗口用它来盖住
/// 「带着原生标题栏上屏」的那一两帧，见 settingswin::show_when_ready。
pub fn blank(window: &tauri::WebviewWindow) {
    set_region(window, None);
}

/// 和 blank 配对：样式改干净之后把窗口放出来。
pub fn unblank(window: &tauri::WebviewWindow) {
    clear_region(window);
}

/// 当前缩放比。窗口一直待在主显示器上，所以这个数就是主显示器的 DPI 比例。
/// 现问显示器而不是读 tao 的缓存：远程切屏时 WM_DPICHANGED 不来，缓存会停在
/// 旧值，见 display.rs。
fn scale(window: &tauri::WebviewWindow) -> f64 {
    crate::display::live_scale(window)
}

/// 把窗口摆到 workArea 底部居中，给定的是物理像素尺寸。物理坐标，绕开逻辑像素
/// 那一层——窗口位置和药丸尺寸必须在同一个坐标系里算，混用缩放比就是「高分屏上
/// 位置差一截」。
///
/// `lift` 是药丸底边离 workArea 底边还要抬多高（物理像素）。
///
/// 最后那一下夹紧不是保险，是必须的：窗口底边比药丸底边还低一整圈 SHADOW_PAD，
/// 按药丸的位置算出来的窗口会伸到屏幕外面去，正好压住最底下那一行像素——
/// EDGE_GAP 要还回去的就是那一行。实测 1.75 缩放下超出 25 物理像素，自动隐藏的
/// 任务栏在药丸那一段宽度里划不出来。代价是药丸整体比设计位置高一点点（底下那
/// 圈阴影留白被挤掉），比起吞掉屏幕边缘，这个换得值。
fn place(window: &tauri::WebviewWindow, w: i32, h: i32, lift: i32) {
    let r = work_area();
    let (w, h) = (w.max(1), h.max(1));
    let x = r.left + ((r.right - r.left - w) / 2).max(0);
    let y = (r.bottom - EDGE_GAP - lift - h).min(r.bottom - EDGE_GAP - h).max(r.top);
    // 先量后摆：窗口这会儿要么区域是空的（什么都没画），要么就是同一个药丸在
    // 挪位置，两种情况下都不会闪。
    let _ = window.set_size(PhysicalSize::new(w as u32, h as u32));
    let _ = window.set_position(PhysicalPosition::new(x, y));
}

/// 建完窗口先收成一小块、区域清空、扩展样式挂好（穿透 + 不可激活）。
///
/// 别在这后面再调 set_ignore_cursor_events——它会照 tao 自己缓存的 flags 重写整个
/// GWL_EXSTYLE，把 WS_EX_NOACTIVATE 一声不吭地抹掉。
pub fn fit(window: &tauri::WebviewWindow) {
    let s = scale(window);
    let w = (START_W * s).round() as i32;
    let h = (START_H * s).round() as i32;
    place(window, w, h, 0);
    // 页面还没报过矩形之前，区域是「整个窗口」。窗口虽然已经不大了，但那也是
    // 一块看不见的东西在吃点击，建完立刻清零，别留这个空窗期。
    set_region(window, None);
    no_frame(window);
    apply_ex(window, true);
    no_caption_paint(window);
    CLICKABLE.store(false, Ordering::Relaxed);
    eprintln!("[药丸] 起始 {w}×{h} 物理（缩放 {s}），区域清空");
}

/// 真把标题栏那几位从 GWL_STYLE 里抠掉。
///
/// `.decorations(false)` **不删样式位**：tao 只是接管 WM_NCCALCSIZE，把非客户区
/// 算成零，让标题栏没地方画。实测这个窗口建完之后 style=0x14CB0000，
/// CAPTION | SYSMENU | MINIMIZEBOX | MAXIMIZEBOX 一位不少地挂着。
///
/// 于是只要有谁碰一下窗口样式（下面 apply_ex 那一下就是），系统就可能拿着这份
/// 「我有标题栏」的缓存先画一帧默认边框，再被 WM_NCCALCSIZE 收回去——屏幕上就是
/// 「点 ✕ 的一瞬间闪过一条 Windows 窗口顶栏」。药丸窗口又矮又宽，那一条的形状
/// 跟真的标题栏几乎一样，所以看上去像另一个窗口一闪而过。
///
/// 位删掉之后就没有东西可画了，这是治根的那一半；apply_ex 里的 SWP_FRAMECHANGED
/// 是另一半（让非客户区立刻重算，而不是拖到下一次绘制）。
///
/// ── 光删样式位不够 ────────────────────────────────────────────────
///
/// 删完实测 style=0x14000000（CAPTION 那几位确实没了），可那条标题栏还是被抓拍
/// 到过一次：药丸窗口矩形顶部亮起一条浅色带，左边写着窗口标题「Localless」，
/// 三帧之内淡出——正是用户截图里那个「框」。窗口矩形比药丸本体上下各大一圈
/// SHADOW_PAD（28 CSS 像素），那条带子画在上面那圈透明留白里，所以看上去像是
/// 药丸上方凭空浮着一个独立的小窗口。
///
/// 画它的不是经典的非客户区绘制（那个看 GWL_STYLE，已经没东西可画了），是 DWM：
/// 它按自己那份 NCRENDERING 策略决定画不画，而默认策略 DWMNCRP_USEWINDOWSTYLE
/// 认的是**建窗口那一刻**的样式——而那一刻 tao 给的是 0x14CB0000，标题栏一位不少。
/// 所以显式把策略改成 DISABLED，从根上取消 DWM 对这个窗口的非客户区绘制。
///
/// 顺手关掉 DWM 的窗口过渡动画：抓到的那次是三帧淡出，就是它在淡。策略关掉之后
/// 本来也没东西可淡，但这一条同时省掉药丸每次改大小时的一次无意义合成。
///
/// ── 还得改成 WS_POPUP，而且每次动边框都要重来一遍 ─────────────────
///
/// 关掉 DWM 之后那条带子又被抓到一次，这回是**点药丸的一瞬**，而且颜色从浅米色
/// 变成了淡蓝——同一条标题栏的「非活动」和「活动」两种配色。窗口矩形宽 226、
/// 药丸本体宽 128，用户截图里那个框和药丸的宽度比是 1.78，和 226/128 一模一样，
/// 所以它就是这个窗口的标题栏，不是别的什么窗口。
///
/// 两条补法：
/// - 补上 WS_POPUP。删掉 CAPTION 那几位之后窗口仍然是 WS_OVERLAPPED（那个值是 0，
///   删不掉，只能靠 POPUP 盖过去），系统眼里它还是一个「本该有标题栏的普通窗口」。
///   置上 POPUP 之后经典非客户区绘制和 DWM 两边看到的都是「弹出窗口，没有标题栏」，
///   最后一扇门也关上了。
/// - 每次动边框之前重来一遍。下面 apply_ex 每次药丸显隐都要写一次 GWL_EXSTYLE 再
///   SWP_FRAMECHANGED，而这正是那一帧被抓到的时刻；DWM 那两个属性和窗口样式是
///   两套独立的状态，谁也不保证改完扩展样式之后它们还在。两次 DwmSetWindowAttribute
///   加一次 GetWindowLongPtrW 是微秒级，每次显隐重放一遍比赌它不会被重置便宜。
///
/// 悬浮麦克风和设置窗口也是 decorations(false)，样式里同样挂着 WS_CAPTION
/// （实测 0x04C80000），所以它们建完也调这里——这个函数因此是 pub 的。
pub fn no_frame(window: &tauri::WebviewWindow) {
    no_frame_inner(window, false)
}

/// 同上，但把 WS_THICKFRAME 留着——给**可缩放**的无边框窗口用。
///
/// 无边框窗口的边缘拖拽靠的就是这一位：tao 只是在 WM_NCCALCSIZE 里把非客户区
/// 算成零，边框本身还在，命中测试照样把四条边判成 HTSIZE。连它一起删掉，窗口
/// 就再也拉不动了，而且没有任何报错——只是拖不动。设置窗口走的是下面那条
/// keep_frameless（它得等 show 之后才删得掉），这个参数就是给它留的。
fn no_frame_inner(window: &tauri::WebviewWindow, keep_resize: bool) {
    let Ok(raw) = window.hwnd() else {
        eprintln!("[药丸] 拿不到 HWND，边框样式没删");
        return;
    };
    unsafe { no_frame_hwnd(HWND(raw.0 as *mut std::ffi::c_void), keep_resize) }
}

/// 样式被谁写回去了就补一刀，没被写回去就什么都不做。
///
/// 建完窗口调一次 no_frame 是不够的——实测悬浮麦克风建完立刻删掉 CAPTION，回头
/// 量到的仍然是 0x04C80000（一位没少）。tauri/tao 在 build() 返回之后还会按自己
/// 缓存的那套 WindowFlags 重写一遍 GWL_STYLE，把手工改的位盖掉，而且一声不吭。
/// 药丸没露出这个毛病只是因为它每次显隐都经过 apply_ex，顺手重放了一遍。
///
/// 所以给没有天然重放点的窗口留这条：读一次样式，干净就直接回，脏了才动手。
/// 一次 GetWindowLongPtrW 是纳秒级，放在 700ms 的巡逻里白给。
pub fn keep_frameless(window: &tauri::WebviewWindow, keep_resize: bool) {
    let Ok(raw) = window.hwnd() else { return };
    let hwnd = HWND(raw.0 as *mut std::ffi::c_void);
    let mut junk = (WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0 | WS_MAXIMIZEBOX.0) as isize;
    if !keep_resize {
        junk |= WS_THICKFRAME.0 as isize;
    }
    let pop = WS_POPUP.0 as isize;
    unsafe {
        let old = GetWindowLongPtrW(hwnd, GWL_STYLE);
        if old & junk == 0 && old & pop != 0 {
            return;
        }
        no_frame_hwnd(hwnd, keep_resize);
    }
}

unsafe fn no_frame_hwnd(hwnd: HWND, keep_resize: bool) {
    // 两个 DWM 属性无条件写：它们和样式位是两套独立的开关，样式早就干净了
    // 也不代表 DWM 那边关着。写失败只记一行，药丸照常用。
    let off = DWMNCRP_DISABLED;
    if let Err(e) = DwmSetWindowAttribute(
        hwnd,
        DWMWA_NCRENDERING_POLICY,
        &off as *const _ as *const std::ffi::c_void,
        std::mem::size_of_val(&off) as u32,
    ) {
        eprintln!("[药丸] DWM 非客户区绘制没关掉：{e}");
    }
    let yes: i32 = 1;
    if let Err(e) = DwmSetWindowAttribute(
        hwnd,
        DWMWA_TRANSITIONS_FORCEDISABLED,
        &yes as *const _ as *const std::ffi::c_void,
        std::mem::size_of_val(&yes) as u32,
    ) {
        eprintln!("[药丸] DWM 过渡动画没关掉：{e}");
    }

    let mut junk = (WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0 | WS_MAXIMIZEBOX.0) as isize;
    if !keep_resize {
        junk |= WS_THICKFRAME.0 as isize;
    }
    let old = GetWindowLongPtrW(hwnd, GWL_STYLE);
    let want = (old & !junk) | WS_POPUP.0 as isize;
    if want == old {
        return;
    }
    SetWindowLongPtrW(hwnd, GWL_STYLE, want);
    frame_changed(hwnd);
}

/// 改完样式位立刻让非客户区重算一遍。
///
/// SetWindowLongPtrW 只改了那个整数，窗口自己缓存的边框尺寸要等下一次
/// SetWindowPos 才更新——中间那段时间系统手上是一份过期的边框。SWP_FRAMECHANGED
/// 就是「现在就重算」，位置大小层级激活状态全不动。
unsafe fn frame_changed(hwnd: HWND) {
    let _ = SetWindowPos(
        hwnd,
        None,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
    );
}

/// 扩展样式的两位一起写，别用 `set_ignore_cursor_events`。
///
/// tao 的那个方法不是「把 WS_EX_TRANSPARENT 这一位改掉」，而是**照自己缓存的一
/// 整套 flags 重写 GWL_EXSTYLE**。于是任何在它之前手工挂上去的位都会被它抹掉，
/// 而且一声不吭：实测在它前面挂 WS_EX_NOACTIVATE，读回来是 False。
///
/// - WS_EX_NOACTIVATE：药丸从头到尾都不该被激活。`.focused(false)` 只管建窗口那
///   一下，点上去照样会把用户正在打字的窗口顶掉——光标一跑，说完按勾粘贴就粘到
///   别人家里去了。以前这件事被「平时整窗穿透，只有光标压在药丸上那一瞬才可点」
///   挡着，现在整段录音期间药丸都可点（触摸屏必须如此），它就真会发生。
/// - WS_EX_TRANSPARENT：药丸藏着的时候连这一小块窗口也别吃点击。区域已经清空了，
///   这一位是第二道。
/// - WS_EX_TOOLWINDOW / 去掉 WS_EX_APPWINDOW：见 OVERLAY_EX_ON 那段。
///
/// 写完必须跟一下 SWP_FRAMECHANGED：改了样式整数不等于窗口知道自己的边框变了，
/// 中间那段过期缓存正是「点 ✕ 闪一条顶栏」的另一半（另一半在 no_frame）。
///
/// 而那一下 SWP_FRAMECHANGED 正是标题栏被抓拍到的那一帧，所以进来先把 no_frame
/// 整套重放一遍——理由见那边的注释。
fn apply_ex(window: &tauri::WebviewWindow, transparent: bool) {
    let Ok(raw) = window.hwnd() else {
        eprintln!("[药丸] 拿不到 HWND，扩展样式没挂上");
        return;
    };
    let hwnd = HWND(raw.0 as *mut std::ffi::c_void);
    let trans = WS_EX_TRANSPARENT.0 as isize;
    unsafe {
        no_frame_hwnd(hwnd, false);
        let mut style = (GetWindowLongPtrW(hwnd, GWL_EXSTYLE) | OVERLAY_EX_ON) & !OVERLAY_EX_OFF;
        if transparent {
            style |= trans;
        } else {
            style &= !trans;
        }
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style);
        frame_changed(hwnd);
    }
}

/// 不让 DefWindowProc 在药丸上画标题栏。
///
/// 样式位删干净、DWM 非客户区绘制关掉之后，那条「Localless」标题栏还是被抓拍到了：
/// 设置窗口在前台时点药丸中间，药丸成了前台窗口（WS_EX_NOACTIVATE 挡不住——点下去
/// WebView2 自己要焦点，而这个进程正好握着前台权），紧接着窗口矩形顶上亮起一条
/// 窄窄的活动配色标题栏，写着窗口标题。
///
/// 画它的是 DefWindowProc 处理 WM_NCACTIVATE 的那一段：它按活动/非活动状态**直接**
/// 重画标题栏，不经过 WM_NCPAINT，也不看 WS_CAPTION。WM_SETTEXT / WM_SETICON 同理。
/// 这条消息又不能整个吞掉——tao 靠它记窗口的焦点状态。
///
/// 所以在 tao 前面垫一层子类：三条消息照常往下传，只是传的时候窗口暂时摘掉
/// WS_VISIBLE，DefWindowProc 看它「不可见」就不画了，传完再挂回去。只改那个整数，
/// 不走 ShowWindow，屏幕上什么都不变。Chromium 的 DefWindowProcWithRedrawLock 就是
/// 这么对付同一件事的；WM_NCACTIVATE 另外按文档传 lParam=-1（「别重画非客户区」），
/// 两道一起上。
pub fn no_caption_paint(window: &tauri::WebviewWindow) {
    let Ok(raw) = window.hwnd() else { return };
    let hwnd = HWND(raw.0 as *mut std::ffi::c_void);
    // 必须在建窗口的那个线程上调——setup 里就是。
    if !unsafe { SetWindowSubclass(hwnd, Some(no_caption_proc), 1, 0) }.as_bool() {
        eprintln!("[药丸] 标题栏子类没挂上");
    }
}

unsafe extern "system" fn no_caption_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if msg != WM_NCACTIVATE && msg != WM_SETTEXT && msg != WM_SETICON {
        return DefSubclassProc(hwnd, msg, wp, lp);
    }
    let lp = if msg == WM_NCACTIVATE { LPARAM(-1) } else { lp };
    let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
    let vis = WS_VISIBLE.0 as isize;
    if style & vis == 0 {
        return DefSubclassProc(hwnd, msg, wp, lp);
    }
    SetWindowLongPtrW(hwnd, GWL_STYLE, style & !vis);
    let r = DefSubclassProc(hwnd, msg, wp, lp);
    // 读当前值再补位，别拿进来时的快照整个写回去——中间 tao 可能改过样式。
    let now = GetWindowLongPtrW(hwnd, GWL_STYLE);
    SetWindowLongPtrW(hwnd, GWL_STYLE, now | vis);
    r
}

/// 扩展样式被写回去了就补一刀，没被写回去就什么都不做。
///
/// 和 keep_frameless 一个道理、一个用法：给没有天然重放点的覆盖层窗口（悬浮麦
/// 克风）留的。药丸不用它——它每次显隐都过 apply_ex，那边已经把这三位写全了。
pub fn keep_overlay_ex(window: &tauri::WebviewWindow) {
    let Ok(raw) = window.hwnd() else { return };
    let hwnd = HWND(raw.0 as *mut std::ffi::c_void);
    unsafe {
        let old = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let want = (old | OVERLAY_EX_ON) & !OVERLAY_EX_OFF;
        if want == old {
            return;
        }
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, want);
        frame_changed(hwnd);
    }
}

/// 页面报上来的药丸外接矩形：CSS 像素，**已经含 transform 的缩放**。换算和 DPI 都
/// 在这边做，页面不需要知道缩放比是多少。
///
/// - `w` / `h`：药丸自己的宽高。零 = 药丸藏着 = 窗口一个像素都不占。
/// - `lift`：药丸底边离 workArea 底边要抬多高（CSS 像素，页面那边是 `12 * k`）。
#[tauri::command]
pub fn pill_rect(window: tauri::WebviewWindow, w: f64, h: f64, lift: f64) {
    let visible = w > 0.0 && h > 0.0 && w.is_finite() && h.is_finite();
    if !visible {
        // 藏起来：区域清零 + 挂回穿透。窗口大小不动——视口一小，页面那边 fixed
        // 定位的排版就跟着变，下次显示量出来的是个畸形矩形。
        set_region(&window, None);
        if CLICKABLE.swap(false, Ordering::Relaxed) {
            apply_ex(&window, true);
            eprintln!("[药丸] 收起");
        }
        return;
    }

    let s = scale(&window);
    let pad = SHADOW_PAD * s;
    let pw = ((w * s) + pad * 2.0).round() as i32;
    let ph = ((h * s) + pad * 2.0).round() as i32;
    // 药丸底边要留的空隙，减掉窗口自己多出来的那一圈——窗口底边比药丸底边还低
    // SHADOW_PAD。
    let lift = ((lift * s) - pad).round() as i32;
    place(&window, pw, ph, lift);

    // 整窗可点，而整窗就是药丸。区域没设上也无所谓了：能被吃掉的就这一小块。
    let ok = set_region(&window, Some((0, 0, pw, ph)));
    if !CLICKABLE.swap(true, Ordering::Relaxed) {
        apply_ex(&window, false);
        // 置顶一旦丢了，药丸就被别的窗口盖住——症状是「药丸呼不出来」，而录音
        // 链路其实全好。每次从藏到显重挂一次，远程连进来切换会话时最容易丢。
        let _ = window.set_always_on_top(true);
        eprintln!("[药丸] 展开");
    }
    eprintln!(
        "[药丸] 窗口 {pw}×{ph} 抬 {lift}（CSS {w:.0}×{h:.0} · 留白 {SHADOW_PAD:.0} · 缩放 {s} · 区域 {ok}）"
    );
}

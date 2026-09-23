// 分辨率/缩放中途变了之后，把几扇窗口拉回来。
//
// UU 远程连进来时，主屏从本地的 3840×2160@200% 换成一块 1920×1080@175% 的虚拟
// 显示器。实测切过去之后悬浮麦还是 80 物理像素（40×2），也就是 Windows **没给
// 已有窗口发 WM_DPICHANGED**，tao 缓存的缩放比停在 2.0：
// - 悬浮麦、药丸都按旧缩放比换算大小和位置；
// - 设置窗口保持 4K 时的物理大小，在 1080p 上溢出屏幕，标题栏被挤出去；
// - tao 的 min_inner_size 也按旧缩放比换算，窗口想缩都缩不回来。
//
// Electron 版靠 screen 的 display-metrics-changed 重摆，Tauri 没有这个事件，所以
// 这里自己巡逻：主屏工作区或缩放比变了，就给缓存过期的窗口补一条 WM_DPICHANGED
// （tao 收到后更新缩放比、按逻辑尺寸重算窗口大小），再让悬浮麦和设置窗口按
// 各自的规矩重摆一次。

use tauri::{AppHandle, Manager};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, SendMessageW, WM_DPICHANGED};

const POLL_MS: u64 = 700;

type Key = (i32, i32, i32, i32, i64);

fn key(app: &AppHandle) -> Key {
    let r = crate::micwin::primary_rect();
    let s = app
        .primary_monitor()
        .ok()
        .flatten()
        .map(|m| (m.scale_factor() * 1000.0).round() as i64)
        .unwrap_or(0);
    (r.left, r.top, r.right, r.bottom, s)
}

/// 起巡逻线程。setup 里调一次。
pub fn watch(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let mut last = key(&app);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
            let now = key(&app);
            if now == last {
                continue;
            }
            last = now;
            eprintln!("[屏幕] 工作区/缩放变成 {now:?}，重摆窗口");
            let a = app.clone();
            // 重摆可能重建悬浮麦窗口，建窗口必须在主线程。
            let _ = app.run_on_main_thread(move || on_change(&a));
        }
    });
}

fn on_change(app: &AppHandle) {
    for w in app.webview_windows().values() {
        resync_dpi(w);
    }
    crate::micwin::sync(app);
    crate::settingswin::refit(app);
}

/// 窗口所在那块屏的实时缩放比。`WebviewWindow::scale_factor()` 读的是 tao 的
/// 缓存，只在收到 WM_DPICHANGED 时才更新，而远程切屏时这条消息不来（见文件头）；
/// 显示器那边的 scale_factor 是每次现问 GetDpiForMonitor。
pub fn live_scale(w: &tauri::WebviewWindow) -> f64 {
    w.current_monitor()
        .ok()
        .flatten()
        .map(|m| m.scale_factor())
        .filter(|s| s.is_finite() && *s > 0.0)
        .or_else(|| w.scale_factor().ok())
        .unwrap_or(1.0)
}

/// tao 缓存的缩放比过期了，就补发一条它本该收到的 WM_DPICHANGED。
fn resync_dpi(w: &tauri::WebviewWindow) {
    let live = live_scale(w);
    let cached = w.scale_factor().unwrap_or(live);
    if (live - cached).abs() < 1e-3 {
        return;
    }
    let Ok(raw) = w.hwnd() else { return };
    let hwnd = HWND(raw.0 as *mut std::ffi::c_void);
    let dpi = (live * 96.0).round() as usize;
    let mut r = RECT::default();
    unsafe {
        if GetWindowRect(hwnd, &mut r).is_err() {
            return;
        }
        SendMessageW(
            hwnd,
            WM_DPICHANGED,
            Some(WPARAM(dpi | (dpi << 16))),
            Some(LPARAM(&r as *const RECT as isize)),
        );
    }
    eprintln!("[屏幕] 「{}」缩放 {cached} → {live}，已补 WM_DPICHANGED", w.label());
}

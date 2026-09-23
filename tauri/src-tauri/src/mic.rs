// 麦克风授权。这是整个 Tauri 版第一个会炸的地方，也是最值得单独测一次的地方。
//
// Electron 版靠的是 main.js 里的一行 setPermissionRequestHandler，放行
// media/microphone/audioCapture 就完事。WebView2 没有这个一行版：它默认既不弹窗
// 也不放行，getUserMedia 直接拒。失败形态是「波形不动、控制台干净、什么都不报」
// ——对一个全部功能都建立在录音上的应用，这是最糟的一种失败。
//
// 走 COM 这一层：给 ICoreWebView2 挂 PermissionRequested，认到麦克风就
// SetState(ALLOW)。SetHandled(true) 在 ...EventArgs2 上，老版本 WebView2 运行时
// 没有这个接口，所以是 cast 成功才调——它只是压住系统那个询问框，设不上也只是
// 多弹一次窗，不影响放行。
//
// 为什么不预置 Profile4::SetPermissionState：那条路要先知道页面的 origin
// （Tauri 在 Windows 上是 http://tauri.localhost），写死一个 origin 是个安静的
// 定时炸弹——将来换成自定义协议或者 https 就又不响了，而症状还是「录不到音」。
// 事件回调这条路不关心 origin。

use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2PermissionRequestedEventArgs2, COREWEBVIEW2_PERMISSION_KIND_MICROPHONE,
    COREWEBVIEW2_PERMISSION_STATE_ALLOW,
};
use webview2_com::PermissionRequestedEventHandler;
use windows::core::Interface;

/// 给一个窗口的 webview 放行麦克风。每个窗口都要单独挂：事件是挂在
/// ICoreWebView2 实例上的，不是全进程的开关。
pub fn allow_microphone(window: &tauri::WebviewWindow) -> Result<(), String> {
    window
        .with_webview(|pv| unsafe {
            let core = match pv.controller().CoreWebView2() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[mic] 拿不到 ICoreWebView2：{e}");
                    return;
                }
            };
            let mut token = Default::default();
            let hr = core.add_PermissionRequested(
                &PermissionRequestedEventHandler::create(Box::new(|_wv, args| {
                    let Some(args) = args else { return Ok(()) };
                    let mut kind = Default::default();
                    args.PermissionKind(&mut kind)?;
                    eprintln!("[mic] PermissionRequested kind={}", kind.0);
                    if kind == COREWEBVIEW2_PERMISSION_KIND_MICROPHONE {
                        args.SetState(COREWEBVIEW2_PERMISSION_STATE_ALLOW)?;
                        if let Ok(a2) = args.cast::<ICoreWebView2PermissionRequestedEventArgs2>() {
                            let _ = a2.SetHandled(true);
                        }
                        eprintln!("[mic] 放行麦克风");
                    }
                    Ok(())
                })),
                &mut token,
            );
            match hr {
                Ok(()) => eprintln!("[mic] 已挂上 PermissionRequested"),
                Err(e) => eprintln!("[mic] 挂不上 PermissionRequested：{e}"),
            }
        })
        .map_err(|e| e.to_string())
}

// 录音的开关，以及「现在是什么状态」这件事对外的唯一出口。
//
// 搬自 Electron 版 main.js:154-173（normalizeRecorderState / publishRecorderState）
// 和 650-681（keepBarOnTop / toggleRecording）。
//
// ── 谁在管状态 ────────────────────────────────────────────────────
//
// 真正的状态机在页面那侧（药丸自己那套 idle/rec/loading/…）。主进程这边只留一份
// **归一化过的**副本，而且只有一个用途：喂给悬浮麦克风那个窗口，让它那颗图标
// 跟着变色。归一化的意义就在这儿——悬浮麦只认四种颜色，不该去理解药丸内部那
// 七八种状态名。
//
// ── 和 Electron 版的差别：toggle 拿不到返回值 ─────────────────────
//
// 那边是 bar.webContents.executeJavaScript('window.__llToggle()')，是个 Promise，
// 主进程能拿到页面的返回值，再把 'thinking' 翻成「上一段仍在处理」。
//
// Tauri 的 eval 没有返回值（它是单向的「把这段 JS 丢过去」）。所以改成：主进程
// 发 localless://toggle 事件 → 页面自己决定，该报状态就回调 recorder_state。
// 等于把那条 if 从主进程挪进了页面，判据一字没变，只是换了方向。
//
// 顺带收了个好处：Electron 那条路在页面抛异常时会把整个 toggle 判成 error，
// 于是「上一段还在转写」和「药丸的 JS 崩了」显示成同一个样子。这边分得开。

use parking_lot::Mutex;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

/// 悬浮麦克风认的四种。和 mic.html 里那几个 class 一一对应。
const KNOWN: [&str; 4] = ["idle", "recording", "busy", "error"];

static STATE: Mutex<Option<Value>> = Mutex::new(None);

/// 药丸那侧的状态名 → 悬浮麦认的四种。
///
/// 两条入口：页面直接给 `{state,message}`（已经是归一化的，原样收下），或者给
/// 药丸内部那套 `{t,s}`。后者要翻译。
///
/// 最后那条按关键词判错是兜底：药丸有一堆「done + 一句话」的收尾态，句子内容
/// 才是它成没成的唯一线索。漏判的后果只是悬浮麦显示成 idle 而不是红色，不影响
/// 听写本身，所以宁可窄一点也不要把正常结束的那几句误判成错误。
fn normalize(detail: &Value) -> Value {
    let get = |k: &str| detail.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let state = get("state");
    if KNOWN.contains(&state) {
        let message = if get("message").is_empty() { get("s") } else { get("message") };
        return json!({ "state": state, "message": message });
    }
    let message = if get("s").is_empty() { get("message") } else { get("s") };
    let t = get("t");
    let state = if t == "rec" {
        "recording"
    } else if t == "loading" || t == "refine" {
        "busy"
    } else if ["失败", "错误", "超时", "未连接", "没听清", "无法", "断开", "崩溃"]
        .iter()
        .any(|w| message.contains(w))
    {
        "error"
    } else {
        "idle"
    };
    json!({ "state": state, "message": message })
}

/// 记下来并推给悬浮麦克风。
pub fn publish(app: &AppHandle, detail: &Value) -> Value {
    let next = normalize(detail);
    *STATE.lock() = Some(next.clone());
    let _ = app.emit_to(crate::micwin::LABEL, "localless://mic-state", next.clone());
    next
}

/// 悬浮麦克风刚建出来时要补一次当前状态——它是按需开关的窗口，错过的那些
/// publish 谁也不会重发。Electron 版是在 did-finish-load 里推，这边反过来由
/// 页面自己来取：窗口建好和页面加载完之间没有可靠的时机可以挂。
#[tauri::command]
pub fn recorder_state_now() -> Value {
    STATE
        .lock()
        .clone()
        .unwrap_or_else(|| json!({ "state": "idle", "message": "" }))
}

/// 药丸报告状态。对应 Electron 的 localless:recorder-state。
///
/// 只认药丸那个窗口：悬浮麦和设置页都不该能伪造录音状态。
#[tauri::command]
pub fn recorder_state(app: AppHandle, window: tauri::WebviewWindow, detail: Value) {
    if window.label() != crate::BAR {
        return;
    }
    publish(&app, &detail);
}

/// 切换录音。快捷键和悬浮麦克风都汇到这儿。
///
/// 补置顶就补这一处：全屏点击穿透的覆盖层一旦丢了置顶就等于隐身——药丸照样
/// 画出来，却被压在当前活动窗口后面。症状是「按键有反应、日志里一切正常，就是
/// 看不见」，坏的根本不是录音链路而是 z 序。独占全屏的程序（远程串流、游戏、
/// 某些播放器）能把置顶挤掉，而且掉了不发任何事件，没法监听，只能每次显示前
/// 重申一遍。
pub fn toggle(app: &AppHandle, source: &str) {
    let Some(bar) = app.get_webview_window(crate::BAR) else {
        crate::debug::log(&format!("toggle({source}) 药丸窗口不在"));
        return;
    };
    let _ = bar.set_always_on_top(true);
    crate::debug::log(&format!("toggle({source})"));
    let _ = bar.emit_to(crate::BAR, "localless://toggle", json!({ "source": source }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &Value) -> String {
        v.get("state").and_then(|x| x.as_str()).unwrap_or("").into()
    }

    /// 页面已经给了归一化的状态名就原样收下，不要再过一遍关键词判据——
    /// 否则一条 `{state:'idle', message:'没听清'}` 会被改判成 error。
    #[test]
    fn 已经归一化的原样收下() {
        let v = normalize(&json!({ "state": "idle", "message": "没听清" }));
        assert_eq!(s(&v), "idle");
        assert_eq!(v["message"], "没听清");
    }

    /// 药丸内部那套状态名的翻译。
    #[test]
    fn 药丸状态名的翻译() {
        assert_eq!(s(&normalize(&json!({ "t": "rec" }))), "recording");
        assert_eq!(s(&normalize(&json!({ "t": "loading" }))), "busy");
        assert_eq!(s(&normalize(&json!({ "t": "refine" }))), "busy");
        assert_eq!(s(&normalize(&json!({ "t": "done" }))), "idle");
    }

    /// 收尾那句话里带着失败的字眼才算错。`s` 和 `message` 两个键都得认：
    /// 药丸发的是 s，别处发的是 message。
    #[test]
    fn 按句子判错() {
        assert_eq!(s(&normalize(&json!({ "t": "done", "s": "识别失败" }))), "error");
        assert_eq!(s(&normalize(&json!({ "t": "done", "s": "引擎未连接" }))), "error");
        assert_eq!(s(&normalize(&json!({ "message": "无法切换录音" }))), "error");
        assert_eq!(s(&normalize(&json!({ "t": "done", "s": "已复制" }))), "idle");
    }

    /// 什么都没有也要给得出一个状态——这条路上任何一次 panic 都会让悬浮麦
    /// 永远停在上一个颜色。
    #[test]
    fn 空对象退回空闲() {
        let v = normalize(&json!({}));
        assert_eq!(s(&v), "idle");
        assert_eq!(v["message"], "");
    }
}

mod debug;
mod cliphist;
mod display;
mod deliver;
mod engine;
mod history;
mod job;
mod keyhook;
mod mic;
mod micwatch;
mod micwin;
mod models;
mod mute;
mod paste;
mod pill;
mod pipeline;
mod procs;
mod recorder;
mod settings;
mod settingswin;
mod tray;
mod vocab;
mod win;

use tauri::{WebviewUrl, WebviewWindowBuilder};

/// 四个窗口的 label。另外两个在 micwin / settingswin 里，挨着各自的建窗代码。
///
/// 当常量而不是随手写字面量，是为了让文件末尾那条测试能把它们和
/// capabilities/default.json 的 windows 名单对齐。那份名单漏了谁，谁就收不到
/// 任何 emit_to，而且没有任何报错——详见那条测试上面的说明。
pub const BAR: &str = "bar";
/// 麦克风自检页。只在 LOCALLESS_MIC_CHECK 下建，但权限得照给。
pub const MIC_CHECK: &str = "mic";


/// 自检结果回传。页面上那段结论只有人眼看得见，而这些验证以后每换一次
/// WebView2 运行时都要重跑一遍——让它落进 stderr，就能不看屏幕地比对。
#[tauri::command]
fn selfcheck(line: String) {
    eprintln!("[自检] {line}");
}

/// 药丸上那颗「复制」。走自家那条 Win32 路而不是 WebView2 的剪贴板 API：只有
/// 它能在写入的同时挂上 CanIncludeInClipboardHistory=0，让听写内容能 Ctrl+V、
/// 却不在 Win+V 的历史里留痕。理由见 paste.rs 头部。
///
/// 留不留痕跟着「自动清理剪贴板」走，和投递那条路同一个判据——两边不一致的话，
/// 同一段听写会因为「用点了复制还是自动粘贴」而在 Win+V 里有不同下场。
#[tauri::command]
async fn copy_text(text: String) -> Result<(), String> {
    let no_history = deliver::hide_from_history();
    if deliver::clipboard_write(text, no_history).await {
        Ok(())
    } else {
        Err("剪贴板写不进去".into())
    }
}



pub fn run() {
    // 麦克风那一关已经过了（PD100X 峰值 0.0161，UU 虚拟声卡是静音因为没人推流）。
    // 页面留着不删：换 WebView2 运行时之后要能一条命令重跑。
    let mic_check = std::env::var("LOCALLESS_MIC_CHECK").is_ok();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            selfcheck,
            copy_text,
            vocab::add_learned_word,
            vocab::undo_learned_word,
            settings::settings_read,
            settings::settings_merge,
            pill::pill_rect,
            micwin::mic_toggle,
            micwin::mic_move,
            micwin::mic_nudge,
            micwin::mic_drag_end,
            micwin::mic_sync,
            micwatch::encoder_pids,
            micwatch::remote_mic_now,
            procs::procs_list,
            tray::device_state,
            settingswin::settings_save,
            settingswin::settings_changed,
            settingswin::settings_close,
            settingswin::settings_close_force,
            settingswin::reset_floating_mic,
            settingswin::autostart_get,
            settingswin::autostart_set,
            settingswin::open_folder,
            settingswin::save_json,
            settingswin::open_json,
            settingswin::reload_engine,
            keyhook::hook_capture,
            pipeline::pipeline_update,
            pipeline::pipeline_latest,
            deliver::clipboard_write,
            deliver::paste_begin,
            deliver::paste_again,
            deliver::paste_release,
            deliver::clipboard_record,
            deliver::read_focused,
            debug::dict_log,
            recorder::recorder_state,
            recorder::recorder_state_now,
            history::history_available,
            history::history_list,
            history::history_delete,
            history::history_update,
            history::history_purge,
            history::corrections_top,
            history::read_audio,
            history::get_context,
            history::save_result,
            history::save_audio_only,
            history::save_correction,
            history::note_learned,
            history::mic_pref,
            models::model_status,
            models::download_model,
            models::delete_model,
            mute::mute_set
        ])
        .setup(move |app| {
            if mic_check {
                let w = WebviewWindowBuilder::new(
                    app,
                    MIC_CHECK,
                    WebviewUrl::App("mic-check.html".into()),
                )
                .title("Localless · 麦克风自检")
                .inner_size(560.0, 620.0)
                .build()?;

                // 必须在页面跑 getUserMedia 之前挂上。build() 之后、首帧之前这段是够的：
                // WebView2 的导航是异步的，而 with_webview 走的是同一个 UI 线程队列。
                if let Err(e) = mic::allow_microphone(&w) {
                    eprintln!("[mic] 放行失败：{e}");
                }
                return Ok(());
            }

            // 历史库。建表那一遍放在这儿付掉，不然第一次建表会发生在第一次听写
            // 结束的那一刻——正是最不该多花时间的地方。
            history::init();

            // 引擎先起。它要 import torch、连 8765、加载声学模型，十几秒起步；
            // 放在建窗口之前是为了让这段等待和界面初始化重叠——Electron 版
            // startEngine() 也在 createBar() 前面，顺序照搬。
            // 起不来不影响界面：药丸照样出得来，只是一说话就转不出字。
            engine::start();

            // 药丸。铺满 workArea 的透明置顶层，它盖在所有窗口上面，不该抢任何
            // 一下点击。靠 SetWindowRgn 把窗口收到药丸那一小块上，区域之外根本
            // 不属于这个窗口——判定在 pill.rs，页面只管把矩形报上来。
            let bar = WebviewWindowBuilder::new(app, BAR, WebviewUrl::App("pill.html".into()))
                .title("Localless")
                .decorations(false)
                .transparent(true)
                .always_on_top(true)
                .skip_taskbar(true)
                .shadow(false)
                .resizable(false)
                // 拿焦点就会把用户正在打字的那个窗口顶掉，光标一跑，粘贴就粘到
                // 别人家里去了。药丸从头到尾都不该被激活。
                .focused(false)
                .build()?;

            // 贴合 workArea 的同时把窗口区域清零、扩展样式挂好（穿透 + 不可激活）。
            // 别在这后面再调 set_ignore_cursor_events——它会照 tao 自己缓存的
            // flags 重写整个 GWL_EXSTYLE，把 WS_EX_NOACTIVATE 一声不吭地抹掉。
            pill::fit(&bar);
            // 药丸自己也要开麦克风——波形那条电平流是独立的一次 getUserMedia。
            // 漏了这一步的症状是「柱子一动不动」，而不是任何一条报错。
            if let Err(e) = mic::allow_microphone(&bar) {
                eprintln!("[mic] 放行失败：{e}");
            }

            // 悬浮麦克风。开没开、多大、在哪，全看设置——和 Electron 版共用
            // 同一份 localless-settings.json，所以那边摆在哪这边起来就在哪。
            micwin::sync(app.handle());

            // 远程切屏时 Windows 不给已有窗口发 WM_DPICHANGED，自己巡逻补上。见 display.rs。
            display::watch(app.handle());

            // 托盘。这是设置窗口唯一的入口：三个窗口全都 skip_taskbar，没有它
            // 就只剩杀进程这一条路。建不出来直接让启动失败，不留一个既看不见
            // 图标、又打不开设置的活进程。
            tray::create(app.handle())?;

            // 按键钩子。放在最后：它一起来就可能触发录音，而录音要药丸在场。
            // Electron 版的顺序也是 createBar → createTray → startKeyHook。
            keyhook::start(app.handle().clone());

            // 「录音时静音」开着的话，提前把 mute.ps1 那一秒的 Add-Type 付掉。
            // 不然第一次听写是说完了才静下来。关着就一个字都不做。
            mute::warm_up();

            // 读输入框的那个 helper 也提前起好。冷启动 500ms，而它第一次被用到
            // 正是一段听写刚转完、要决定往哪儿投递的那一刻——最不该等的地方。
            deliver::warm_up();

            // 没有引擎的时候没法看药丸长什么样。这条假状态流按真实时序走一遍
            // 五个状态，只在设了环境变量时才跑。
            if std::env::var("LOCALLESS_PILL_DEMO").is_ok() {
                demo(app.handle().clone());
            }

            // 设置窗平时只有托盘那一下能开。托盘图标在通知区里，脚本点不着它，
            // 于是设置页整页都验不了。给它一条和上面同一种写法的后门。
            if std::env::var("LOCALLESS_SETTINGS").is_ok() {
                settingswin::toggle(app.handle());
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("Localless 起不来")
        // 收摊。engine.py 是独立进程，不跟着父进程死——不杀的话退出后它还占着
        // 8765 和 3.4 GB 显存，下次启动的新引擎抢不到端口，表现成「重启一次就
        // 再也转不出字了」。按键钩子同理：不杀的话它会一直全局吞 RightAlt，
        // 下次启动多一个钩子抢同一个键，表现成「按 Alt 没反应」。
        // Electron 版是在 will-quit 里一起杀的，同一件事。
        //
        // 静音排在最前面，而且是唯一一个要等它退完的：别人的声音还按在我们
        // 手里，晚一步还原用户看到的就是「退出之后别的程序还是没声音」。
        //
        // 只有正常退出（托盘 Quit、关机）走得到这里，所以这三个 stop 都不能起新
        // 进程：关机时 CreateProcess 会弹 0xc0000142 的错误框把重启卡住。
        // 被 taskkill /F 的话：引擎树由 Job Object 的 KILL_ON_JOB_CLOSE 收掉；钩子
        // 会变成孤儿，靠下次启动的 killOrphanScripts 清掉来兜底；静音那个自己就能兜
        // （stdin 一关它就还原）。
        .run(|_app, event| {
            if let tauri::RunEvent::Exit = event {
                mute::stop();
                keyhook::stop();
                engine::stop();
            }
        });
}

/// 假状态流。字段名和 Electron 版 localless:ui 的 detail 一字不差，所以页面那侧
/// 走的是和真引擎完全相同的代码路径——不是另做一套预览。
fn demo(app: tauri::AppHandle) {
    use serde_json::json;
    use tauri::Emitter;
    std::thread::spawn(move || {
        let steps = [
            (2200, json!({ "kind": "ui", "detail": { "t": "loading" } })),
            (6000, json!({ "kind": "ui", "detail": { "t": "rec" } })),
            (2600, json!({ "kind": "ui", "detail": { "t": "refine", "save": true } })),
            (4200, json!({ "kind": "result", "detail": "这是一句转写出来的话，长一点好看清楚结果态的排版是不是贴着 Electron 版。" })),
            (4000, json!({ "kind": "learned", "detail": { "id": "demo", "term": "声学模型", "tagName": "技术词库", "ms": 3500 } })),
            (1800, json!({ "kind": "ui", "detail": { "t": "done", "s": "没听清" } })),
        ];
        loop {
            for (hold, payload) in &steps {
                let _ = app.emit("localless://ui", payload);
                std::thread::sleep(std::time::Duration::from_millis(*hold));
            }
            std::thread::sleep(std::time::Duration::from_millis(1500));
        }
    });
}

#[cfg(test)]
mod tests {
    /// 四个窗口的 label 必须和 capabilities/default.json 的 windows 名单一字不差。
    ///
    /// 这条盯的是一种没有任何症状的坏法。那份名单管的是**插件命令**：core:event
    /// 的 listen、core:window 的那些调用、dialog、opener。自家用
    /// `generate_handler!` 注册的命令不归它管——这一点实测过，别再靠猜：
    /// 把 floatmic 从名单里拿掉，点悬浮麦克风照样调得通 mic_toggle。
    /// 但把 bar 拿掉，药丸就收不到 localless://toggle 了，于是
    /// debug.log 里记着 toggle(floating-mic)、录音却永远不开始，stderr 一行报错
    /// 都没有——emit 到一个没权限 listen 的窗口是彻底静默的。
    ///
    /// 所以名单漏一个窗口未必当场坏：漏了 floatmic 今天什么事都没有，因为
    /// mic.html 一条插件命令都不调。它是颗定时的：谁哪天往那页加一句 listen()
    /// （recorder.rs 里 mic-state 那条注释正是在邀请这件事），就会掉进上面那个
    /// 全静默的坑里，而且不会想到是权限。
    ///
    /// 两个方向都验：
    /// - 少了 → 上面那件事；
    /// - 多了 → 名单里那条对不上任何窗口，多半是把 label 拼错了，而拼错的那个
    ///   正是上一条。所以「多」和「少」总是成对出现，只验一边等于没验。
    #[test]
    fn 每个窗口都在能力清单里() {
        let json = include_str!("../capabilities/default.json");
        let v: serde_json::Value = serde_json::from_str(json).expect("capabilities 不是合法 JSON");
        let mut listed: Vec<&str> = v["windows"]
            .as_array()
            .expect("没有 windows 这一栏")
            .iter()
            .map(|w| w.as_str().expect("windows 里混进了非字符串"))
            .collect();
        listed.sort();

        let mut real = vec![
            super::BAR,
            super::MIC_CHECK,
            crate::micwin::LABEL,
            crate::settingswin::LABEL,
        ];
        real.sort();

        assert_eq!(
            listed, real,
            "能力清单和实际窗口对不上。少的那个收不到任何 emit_to（全静默），\
             多的那个多半是 label 拼错了"
        );
    }
}

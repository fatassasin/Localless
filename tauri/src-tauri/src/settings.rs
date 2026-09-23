// 设置的读写。文件位置、字段名、原子写的做法都和 Electron 版
// （main.js:715 / 763-786）完全一致——两版共用同一份 localless-settings.json，
// 所以搬过程中可以来回切着用，也不会有"换了版本设置全没了"这一出。
//
//   %APPDATA%\localless\localless-settings.json
//
// 有两个坑是 Electron 版踩出来的，这边原样搬：
//
// 1. 读失败 ≠ 空设置。JSON 解析不出来但文件确实有内容，那是撞上了别人写到
//    一半的瞬间。这时候把它当 {} 处理，接着 merge 再原子写回，一次撕裂读就能
//    把整份设置替换成只剩 patch 的残骸。宁可跳过这次写入。
// 2. 写必须是临时文件 + rename。直接覆写的话，进程在写到一半时被杀（托盘退出、
//    注销）留下的就是半个 JSON，下次启动读不出来。

use serde_json::{Map, Value};
use std::path::PathBuf;

/// 和 Electron 版同一个路径。取不到 APPDATA 时退回当前目录，和那边的
/// `process.env.APPDATA || '.'` 一个意思。
pub fn path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("localless").join("localless-settings.json")
}

/// 读不出来就当空设置。调用方要区分"真的空"和"读坏了"的话去看 has_content()。
pub fn read() -> Map<String, Value> {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

/// 文件里有没有东西。`> 2` 是为了把 `{}` 也算成空——和 Electron 版同一条判据。
fn has_content() -> bool {
    std::fs::metadata(path()).map(|m| m.len() > 2).unwrap_or(false)
}

/// 把 patch 铺到现有设置上写回去。返回写完之后的完整设置；判定为撕裂读时
/// 返回 None 并且**什么都不写**。
pub fn merge(patch: Map<String, Value>) -> Option<Map<String, Value>> {
    let current = read();
    if current.is_empty() && has_content() {
        eprintln!("[settings] 读取失败但文件非空，放弃这次写入以免覆盖");
        return None;
    }
    let mut next = current;
    for (k, v) in patch {
        next.insert(k, v);
    }

    let p = path();
    let dir = p.parent()?;
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("[settings] 建目录失败：{e}");
        return None;
    }
    let text = serde_json::to_string_pretty(&next).ok()?;
    // 临时文件名带上 pid：两个实例同时写不会互相踩到对方的半成品。
    let temp = p.with_extension(format!("json.tmp-{}", std::process::id()));
    if let Err(e) = std::fs::write(&temp, text) {
        eprintln!("[settings] 写临时文件失败：{e}");
        return None;
    }
    if let Err(e) = std::fs::rename(&temp, &p) {
        eprintln!("[settings] rename 失败：{e}");
        let _ = std::fs::remove_file(&temp);
        return None;
    }
    Some(next)
}

// ── 给页面用的两条命令 ──────────────────────────────────────────────

#[tauri::command]
pub fn settings_read() -> Map<String, Value> {
    read()
}

#[tauri::command]
pub fn settings_merge(patch: Map<String, Value>) -> Result<Map<String, Value>, String> {
    merge(patch).ok_or_else(|| "这次写入被跳过了（见日志）".into())
}

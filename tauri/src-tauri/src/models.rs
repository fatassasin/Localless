// 模型的状态、下载、删除。搬自 Electron 版 main.js:1380-1396。
//
// 模型清单来自 model-registry.json，和设置页共用同一份拷贝（tauri/src/），
// 用 include_str! 编进来——两边读的必须是同一个文件，否则「页面说装好了、
// 引擎说找不到」这种对不上的事迟早发生。
//
// 目录布局跟 Electron 版一字不差，因为两版共用同一个 models 目录：
//
//   <仓库根>/models/<installDir>/          下好的模型
//   <仓库根>/models/<installDir>/.complete  下完的标记（没有它就算没装）
//   <仓库根>/models/<installDir>.part/      下到一半的临时目录
//   <仓库根>/models/<entry>                 单文件模型直接就是这个路径
//
// 「装没装好」看的是 .complete 而不是目录在不在：下到一半被掐断留下的是一个
// 内容不全的目录，认它就是让引擎去加载半个 safetensors，报出来的错跟模型本身
// 坏了一模一样，查起来能查半天。

use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter};

static REGISTRY_JSON: &str = include_str!("../../src/model-registry.json");

/// 正在下的那几个。Electron 版存的是子进程对象，这边只需要 id——进度是靠
/// 事件推的，没人需要回头去拿那个 Child。
static DOWNLOADING: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn downloading() -> parking_lot::MappedMutexGuard<'static, HashSet<String>> {
    parking_lot::MutexGuard::map(DOWNLOADING.lock(), |o| o.get_or_insert_with(HashSet::new))
}

fn registry() -> Vec<Map<String, Value>> {
    serde_json::from_str::<Value>(REGISTRY_JSON)
        .ok()
        .and_then(|v| v.get("models").cloned())
        .and_then(|v| match v {
            Value::Array(a) => Some(a),
            _ => None,
        })
        .map(|a| {
            a.into_iter()
                .filter_map(|m| match m {
                    Value::Object(o) => Some(o),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

// ── 仓库根 ────────────────────────────────────────────────────────
//
// Electron 那边 ROOT 是 path.join(__dirname,'..')，__dirname 永远是 app/，
// 所以一句话就定了。这边 exe 在 tauri/src-tauri/target/debug/ 下面，离仓库根
// 隔着四层，而打包之后又完全是另一个地方——只能往上找。
//
// 判据用「同时有 app/model-registry.json 和 models/」而不是只看 models/：
// 单看一个目录名，碰上别的项目里恰好也有个 models 就认错了，而认错的后果是
// 把几个 G 的模型下到一个莫名其妙的地方。

/// 沿着 exe 往上找仓库根。找不到就退回当前目录——和 Electron 的
/// `process.env.APPDATA || '.'` 一个路子：宁可用一个看得见的错路径，
/// 也不要在这里 panic 把整个应用拖死。
pub fn root() -> PathBuf {
    if let Some(p) = std::env::var_os("LOCALLESS_ROOT") {
        return PathBuf::from(p);
    }
    let start = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let mut cur: &Path = &start;
    loop {
        if cur.join("app").join("model-registry.json").is_file() && cur.join("models").is_dir() {
            return cur.to_path_buf();
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => break,
        }
    }
    eprintln!("[模型] 没找到仓库根（从 {} 往上），退回当前目录", start.display());
    PathBuf::from(".")
}

fn models_dir() -> PathBuf {
    root().join("models")
}

/// 对应 Electron 的 modelPath()：entry 优先，其次 installDir。
fn model_path(m: &Map<String, Value>) -> PathBuf {
    let rel = m
        .get("entry")
        .and_then(|v| v.as_str())
        .or_else(|| m.get("installDir").and_then(|v| v.as_str()))
        .unwrap_or("");
    models_dir().join(rel)
}

/// 递归目录大小。下到一半的 .part 目录有多大全靠它，算不出来就当 0——
/// 进度条少一格，比让整个模型列表报错强。
fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let mut n = 0u64;
    for e in entries.flatten() {
        match e.file_type() {
            Ok(t) if t.is_dir() => n += dir_bytes(&e.path()),
            Ok(_) => n += e.metadata().map(|m| m.len()).unwrap_or(0),
            Err(_) => {}
        }
    }
    n
}

fn imported_file() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("localless").join("imported-models.json")
}

fn imported_models() -> Vec<Map<String, Value>> {
    std::fs::read_to_string(imported_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| match v {
            Value::Array(a) => Some(a),
            _ => None,
        })
        .map(|a| {
            a.into_iter()
                .filter_map(|m| match m {
                    Value::Object(o) => Some(o),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn save_imported(xs: &[Map<String, Value>]) {
    let p = imported_file();
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(text) = serde_json::to_string_pretty(xs) {
        let _ = std::fs::write(p, text);
    }
}

/// 见文件头：装没装好看 .complete，单文件模型看文件本身。
fn installed(m: &Map<String, Value>) -> bool {
    let p = model_path(m);
    let has_entry = m.get("entry").and_then(|v| v.as_str()).is_some();
    let marker = if has_entry { p.clone() } else { p.join(".complete") };
    let builtin = m.get("builtin").and_then(|v| v.as_bool()).unwrap_or(false);
    marker.exists() || (builtin && p.exists())
}

// ── 命令 ──────────────────────────────────────────────────────────

#[tauri::command]
pub fn model_status() -> Value {
    let live = downloading().clone();
    let mut out: Vec<Value> = registry()
        .into_iter()
        .map(|m| {
            let p = model_path(&m);
            let part = m
                .get("installDir")
                .and_then(|v| v.as_str())
                .map(|d| models_dir().join(format!("{d}.part")));
            let total: u64 = m
                .get("files")
                .and_then(|v| v.as_object())
                .map(|f| {
                    f.values()
                        .filter_map(|x| x.get(0).and_then(|n| n.as_u64()))
                        .sum()
                })
                .unwrap_or(0);
            let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();

            let mut o = m.clone();
            o.insert("installed".into(), json!(installed(&m)));
            o.insert("bytes".into(), json!(part.map(|p| dir_bytes(&p)).unwrap_or(0)));
            o.insert("total".into(), json!(total));
            o.insert("downloading".into(), json!(live.contains(&id)));
            o.insert("path".into(), json!(p.to_string_lossy()));
            Value::Object(o)
        })
        .collect();

    for mut m in imported_models() {
        let ok = m
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| Path::new(p).exists())
            .unwrap_or(false);
        m.insert("installed".into(), json!(ok));
        m.insert("imported".into(), json!(true));
        out.push(Value::Object(m));
    }

    json!({ "models": out })
}

/// 下模型。跟 Electron 版一样交给 download-model.py——断点续传、分片校验、
/// 镜像切换那一套全在那个脚本里，用 Rust 重写一遍只会多出一套要对齐的行为。
///
/// 进度按 NDJSON 一行一条推给设置页，事件名和字段跟 Electron 版对齐，
/// 所以页面那侧的 refreshModel 完全不用改。
#[tauri::command]
pub fn download_model(app: AppHandle, model_id: String) -> Value {
    let Some(m) = registry().into_iter().find(|m| {
        m.get("id").and_then(|v| v.as_str()) == Some(model_id.as_str())
            && m.contains_key("files")
            && m.contains_key("base")
    }) else {
        return json!({ "success": false, "error": "unsupported model" });
    };
    let _ = m;

    {
        let mut live = downloading();
        if live.contains(&model_id) {
            return json!({ "success": false, "error": "download already running" });
        }
        live.insert(model_id.clone());
    }

    let root = root();
    let py = root.join("app").join(".venv").join("Scripts").join("python.exe");
    let script = root.join("app").join("download-model.py");
    let models = models_dir();

    let child = std::process::Command::new(&py)
        .arg(&script)
        .arg("--model")
        .arg(&model_id)
        .arg("--models-dir")
        .arg(&models)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            downloading().remove(&model_id);
            eprintln!("[模型] 起不来 {}：{e}", py.display());
            return json!({ "success": false, "error": format!("起不来下载器：{e}") });
        }
    };

    let stdout = child.stdout.take();
    let id = model_id.clone();
    std::thread::spawn(move || {
        if let Some(out) = stdout {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                // 解不出来的行直接丢。download-model.py 偶尔会往 stdout 写
                // 非 JSON 的诊断，Electron 版也是 try{}catch{} 吞掉的。
                if let Ok(Value::Object(mut d)) = serde_json::from_str::<Value>(line) {
                    d.insert("modelId".into(), json!(id));
                    let _ = app.emit("localless://download-progress", Value::Object(d));
                }
            }
        }
        let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
        downloading().remove(&id);
        let _ = app.emit(
            "localless://download-progress",
            json!({ "modelId": id, "status": if code == 0 { "ready" } else { "error" } }),
        );
    });

    json!({ "success": true })
}

/// 删模型。路径包含检查不能省：registry 里的 installDir 要是写成 `..\..\`，
/// 这一句就是个能删任意目录的递归删除。Electron 版同样有这道检查。
#[tauri::command]
pub fn delete_model(id: String) -> Value {
    if let Some(m) = registry()
        .into_iter()
        .find(|m| m.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
    {
        if m.get("builtin").and_then(|v| v.as_bool()).unwrap_or(false) {
            return json!({ "success": false, "error": "内置模型不能删除" });
        }
        let rel = m
            .get("installDir")
            .and_then(|v| v.as_str())
            .or_else(|| m.get("entry").and_then(|v| v.as_str()))
            .unwrap_or("");
        let base = models_dir();
        let p = base.join(rel);
        // 用 starts_with 之前先把 `..` 规整掉——没规整的话 models/../../x
        // 字面上仍然以 models 开头，检查等于没做。
        let normalized = normalize(&p);
        if rel.is_empty() || !normalized.starts_with(&base) || normalized == base {
            eprintln!("[模型] 拒绝删除越界路径 {}", normalized.display());
            return json!({ "success": false });
        }
        let _ = std::fs::remove_dir_all(&normalized);
        let _ = std::fs::remove_file(&normalized);
        let _ = std::fs::remove_dir_all(normalized.with_extension("part"));
        return json!({ "success": true });
    }

    let xs = imported_models();
    let next: Vec<_> = xs
        .iter()
        .filter(|x| x.get("id").and_then(|v| v.as_str()) != Some(id.as_str()))
        .cloned()
        .collect();
    if next.len() != xs.len() {
        save_imported(&next);
        return json!({ "success": true });
    }
    json!({ "success": false })
}

/// 纯字符串上的 `.`/`..` 规整。不用 canonicalize：那个要求路径真实存在，
/// 而这里恰恰要在删之前判断一个可能不存在的路径安不安全。
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 清单能解出来，而且那几个引擎点名要的 id 都在。
    /// 解不出来的症状是模型页一片空白，不是任何一条报错。
    #[test]
    fn 清单解得开() {
        let r = registry();
        assert!(r.len() >= 5, "只解出 {} 个模型", r.len());
        let ids: Vec<_> = r
            .iter()
            .filter_map(|m| m.get("id").and_then(|v| v.as_str()))
            .collect();
        for want in ["qwen3-asr-1.7b-hf", "qwen3-4b", "silero-vad"] {
            assert!(ids.contains(&want), "清单里没有 {want}");
        }
    }

    /// 路径规整：能把 `..` 吃掉，这样删模型那道包含检查才是真的检查。
    #[test]
    fn 规整吃掉上级() {
        assert_eq!(normalize(Path::new("a/b/../c")), PathBuf::from("a/c"));
        assert_eq!(normalize(Path::new("a/./b")), PathBuf::from("a/b"));
        assert_eq!(normalize(Path::new("a/b/../../../x")), PathBuf::from("x"));
    }

    /// 越界的 installDir 必须删不动。这条是防「清单被改坏就能删任意目录」。
    #[test]
    fn 越界路径判得出来() {
        let base = PathBuf::from("S:/repo/models");
        for evil in ["../../windows", "..", "../models2"] {
            let p = normalize(&base.join(evil));
            assert!(!p.starts_with(&base) || p == base, "{evil} 竟然通过了检查");
        }
        let good = normalize(&base.join("qwen3-asr-1.7b-hf"));
        assert!(good.starts_with(&base) && good != base);
    }
}

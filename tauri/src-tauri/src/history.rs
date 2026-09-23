// 历史库。搬自 Electron 版设置页里那几条直接打在渲染进程的 better-sqlite3 调用
// （settings.html 的 purgeHistory / renderHistory / retranscribe）。
//
//   %APPDATA%\localless\history.db
//
// 搬过来最大的变化是「同步变异步」：那边 db.prepare(...).all() 当场就回来，这边
// 隔着一次 IPC。SQL 本身一个字没改——两版共用同一个库文件，engine.py 还在往里写，
// 字段名或者 WHERE 条件动一下就是两边对不上。
//
// 几条不能省的规矩：
//
// 1. 删历史必须先删 wav 再删行。反过来的话路径就再也查不到了，recordings 目录
//    会一直留着孤儿 wav——这正是 Electron 版注释里记下来的那一条。
// 2. 打不开库不是致命错误。引擎没跑过、或者第一次启动时库还不存在，设置页要能
//    照常打开，历史那一页显示「历史数据库不可用」就行，不能整个窗口起不来。
// 3. busy_timeout 要给。engine.py 正在写一条听写记录的那一瞬间打开设置页是很
//    常见的操作，默认的 0 超时会直接返回 SQLITE_BUSY，表现成「历史是空的」。

use rusqlite::Connection;
use serde_json::{json, Map, Value};
use std::path::PathBuf;

fn db_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("localless").join("history.db")
}

/// 打开库，顺手保证表在。
///
/// **建表这件事 Electron 版是 preload.cjs 干的**（文件开头那段 CREATE TABLE IF
/// NOT EXISTS + 三条 ALTER），engine.py 一行 SQL 都没有。preload 在这一版里整个
/// 没了，所以这段必须落在这儿——漏掉的症状是干净机器上第一次听写起，历史、
/// 自学习、纠错偏置三样全都静默不工作，而且一条报错都没有（每条 SQL 各自
/// try/catch 掉了）。
///
/// 三条 ALTER 是给老库补列的。列已经存在时 SQLite 报错，忽略掉就是了——
/// 没有 IF NOT EXISTS 这种写法。
fn open() -> Result<Connection, String> {
    let p = db_path();
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let c = Connection::open(&p).map_err(|e| format!("打不开 {}：{e}", p.display()))?;
    // 见文件头第 3 条。3 秒比 engine 写一条记录的时间长一个数量级。
    let _ = c.busy_timeout(std::time::Duration::from_secs(3));

    static ONCE: std::sync::Once = std::sync::Once::new();
    let mut err = None;
    ONCE.call_once(|| {
        // 字段名、类型、默认值都必须和 Electron 版一字不差：两版共用同一个库文件。
        if let Err(e) = c.execute_batch(
            "CREATE TABLE IF NOT EXISTS history(
               id TEXT PRIMARY KEY, status TEXT, mode TEXT DEFAULT 'voice_transcript',
               refined_text TEXT, duration REAL, audio_metadata TEXT, audio_local_path TEXT,
               mode_meta TEXT, client_metadata TEXT, debug_info TEXT, audio_context TEXT,
               created_at TEXT, updated_at TEXT);
             CREATE TABLE IF NOT EXISTS corrections(
               wrong_text TEXT NOT NULL, right_text TEXT NOT NULL,
               count INTEGER NOT NULL DEFAULT 1,
               created_at TEXT NOT NULL, last_seen TEXT NOT NULL,
               UNIQUE(wrong_text,right_text));",
        ) {
            err = Some(format!("建表失败：{e}"));
            return;
        }
        for col in ["raw_text TEXT", "corrected_text TEXT", "target_app TEXT"] {
            let _ = c.execute(&format!("ALTER TABLE history ADD COLUMN {col}"), []);
        }
    });
    match err {
        Some(e) => Err(e),
        None => Ok(c),
    }
}

/// 启动时先开一次，把建表那一遍付掉。没有它的话第一次建表会发生在第一次听写
/// 结束的那一刻——正是最不该多花时间的地方。
pub fn init() {
    if let Err(e) = open() {
        eprintln!("[历史] {e}");
    }
}

/// 把一行拆成 JSON 对象。列是按名字取的，不是按下标——engine.py 以后往表里加列
/// 的话，按下标取会整体错位，而且错得静悄悄。
fn row_to_json(row: &rusqlite::Row, names: &[String]) -> Map<String, Value> {
    let mut m = Map::new();
    for (i, name) in names.iter().enumerate() {
        let v = match row.get_ref(i) {
            Ok(rusqlite::types::ValueRef::Null) | Err(_) => Value::Null,
            Ok(rusqlite::types::ValueRef::Integer(n)) => Value::from(n),
            Ok(rusqlite::types::ValueRef::Real(f)) => Value::from(f),
            Ok(rusqlite::types::ValueRef::Text(t)) => {
                Value::from(String::from_utf8_lossy(t).into_owned())
            }
            // BLOB 这张表里没有，真出现了也没法当文本用，给 null 比给乱码强。
            Ok(rusqlite::types::ValueRef::Blob(_)) => Value::Null,
        };
        m.insert(name.clone(), v);
    }
    m
}

// ── 给设置页的命令 ────────────────────────────────────────────────

/// 历史库在不在。对应 Electron 版 `let db;try{db=new Database(DB)}catch{}` 之后的
/// `if(!db)`——页面靠它决定是显示列表还是「历史数据库不可用」。
#[tauri::command]
pub fn history_available() -> bool {
    open().is_ok()
}

/// 最近 100 条。`has_audio` 是这边替页面算好的：那边是 fs.existsSync(r.audio_local_path)，
/// 渲染进程直接摸磁盘；这边一行一次 IPC 太贵，所以查库的时候顺手 stat 掉。
#[tauri::command]
pub fn history_list() -> Result<Vec<Map<String, Value>>, String> {
    let c = open()?;
    let mut stmt = c
        .prepare("SELECT * FROM history ORDER BY created_at DESC LIMIT 100")
        .map_err(|e| e.to_string())?;
    let names: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
    let rows = stmt
        .query_map([], |row| Ok(row_to_json(row, &names)))
        .map_err(|e| e.to_string())?;

    let mut out = Vec::new();
    for r in rows {
        let mut m = r.map_err(|e| e.to_string())?;
        let has = m
            .get("audio_local_path")
            .and_then(|v| v.as_str())
            .map(|p| std::path::Path::new(p).is_file())
            .unwrap_or(false);
        m.insert("has_audio".into(), Value::Bool(has));
        out.push(m);
    }
    Ok(out)
}

/// 删一条。先 wav 后行，理由见文件头第 1 条。
#[tauri::command]
pub fn history_delete(id: String) -> Result<(), String> {
    let c = open()?;
    let wav: Option<String> = c
        .query_row("SELECT audio_local_path FROM history WHERE id=?", [&id], |r| r.get(0))
        .ok()
        .flatten();
    if let Some(p) = wav {
        let _ = std::fs::remove_file(p);
    }
    c.execute("DELETE FROM history WHERE id=?", [&id])
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 重新转录之后写回。SQL 和 Electron 版一字不差，包括那个写死的 status='completed'。
#[tauri::command]
pub fn history_update(id: String, raw: String, text: String, updated_at: String) -> Result<(), String> {
    let c = open()?;
    c.execute(
        "UPDATE history SET raw_text=?,refined_text=?,status='completed',updated_at=? WHERE id=?",
        rusqlite::params![raw, text, updated_at, id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 按保留期清理。days 来自设置里的 historyRetention（1d/3d/7d/30d），
/// 「永久」那一档页面根本不会调过来。
#[tauri::command]
pub fn history_purge(days: i64) -> Result<usize, String> {
    if days <= 0 {
        return Ok(0);
    }
    let c = open()?;
    let cond = "datetime(created_at)<datetime('now',?)";
    let arg = format!("-{days} days");

    // 先删文件。这一段整个失败也要继续往下删行——查不出路径是"没有录音"的
    // 正常情形，不该把清理整条拦下来。
    if let Ok(mut stmt) = c.prepare(&format!(
        "SELECT audio_local_path FROM history WHERE {cond} AND audio_local_path IS NOT NULL"
    )) {
        if let Ok(rows) = stmt.query_map([&arg], |r| r.get::<_, Option<String>>(0)) {
            for p in rows.flatten().flatten() {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    c.execute(&format!("DELETE FROM history WHERE {cond}"), [&arg])
        .map_err(|e| e.to_string())
}

/// 重新转录时带给引擎的偏置词。LIMIT 30 和排序跟 Electron 版一致——这两个数字
/// 决定了引擎那边 prompt 的长度，改了就是改识别行为。
#[tauri::command]
pub fn corrections_top() -> Vec<Map<String, Value>> {
    // 这条失败只意味着少一点偏置，转录照样能跑。Electron 版就是 try{}catch{} 吞掉的。
    let Ok(c) = open() else { return Vec::new() };
    let Ok(mut stmt) = c.prepare(
        "SELECT wrong_text AS wrong, right_text AS \"right\" FROM corrections \
         ORDER BY count DESC,last_seen DESC LIMIT 30",
    ) else {
        return Vec::new();
    };
    let names: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
    stmt.query_map([], |row| Ok(row_to_json(row, &names)))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

// ── 听写那条路上的读写 ────────────────────────────────────────────
//
// 搬自 preload.cjs:411-487（getContext / saveResult / saveAudioOnly /
// saveCorrection / noteLearned）。那边是渲染进程直接握着 better-sqlite3 同步调用，
// 这边隔着一次 IPC，所以全是 async + spawn_blocking：这几条在一次听写收尾的
// 几十毫秒里连着跑，同步命令会占着主线程，药丸的波形当场卡住。

/// 开录前问一次：设置、最近几条上文、最常见的纠错对。
///
/// 顺带按保留期清一次历史。Electron 版就是在这儿清的——没有别的定时器，这是
/// 唯一会触发清理的地方，挪走就等于把「历史保留时间」这个设置废掉。
#[tauri::command]
pub async fn get_context() -> Value {
    tauri::async_runtime::spawn_blocking(|| {
        let st = crate::settings::read();
        let days = match st.get("historyRetention").and_then(|v| v.as_str()).unwrap_or("") {
            "1d" => 1,
            "3d" => 3,
            "7d" => 7,
            "30d" => 30,
            _ => 0,
        };
        if days > 0 {
            let _ = history_purge(days);
        }
        let Ok(c) = open() else {
            return json!({ "settings": st, "history": [], "corrections": [] });
        };

        // 写死 3 条。原来是设置页上的一个数字框，但它调的是「给模型看几条上文」——
        // 多了挤占提示词预算（截断逻辑在 engine 那侧，超了 ASR 会吐空），少了名词
        // 对不齐，3 是这两头之间唯一能一直用的值，跟用户的偏好无关。
        let mut history: Vec<String> = Vec::new();
        if let Ok(mut stmt) = c.prepare(
            "SELECT COALESCE(corrected_text,refined_text) text FROM history \
             WHERE COALESCE(corrected_text,refined_text) IS NOT NULL \
             ORDER BY created_at DESC LIMIT 3",
        ) {
            if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
                for t in rows.flatten() {
                    // 每条只取末尾 300 字。按字符切不是按字节——中文一个字三个
                    // 字节，按字节切会把最后一个字劈成半个，送进模型就是乱码。
                    let n = t.chars().count();
                    history.push(t.chars().skip(n.saturating_sub(300)).collect());
                }
            }
        }
        history.reverse(); // 旧的在前，读起来才是一段对话

        json!({ "settings": st, "history": history, "corrections": corrections_top() })
    })
    .await
    .unwrap_or_else(|_| json!({ "settings": {}, "history": [], "corrections": [] }))
}

/// 转写成功，写一条完整记录。
///
/// 没有 refined_text 就什么都不写——失败的听写走 save_audio_only 那条路，
/// 在历史里留一条「有声音、没文字」。
#[tauri::command]
pub async fn save_result(id: String, raw_text: String, refined_text: String, duration: f64) {
    if id.is_empty() || refined_text.is_empty() {
        return;
    }
    let _ = tauri::async_runtime::spawn_blocking(move || {
        let Ok(c) = open() else { return };
        let now = now_iso();
        // ON CONFLICT 那一串和 Electron 版一字不差。撞上的情形是 save_audio_only
        // 先落了一条 audio_only，这里要把它升级成 completed。
        let _ = c.execute(
            "INSERT INTO history(id,status,mode,raw_text,refined_text,duration,created_at,updated_at)
             VALUES(?,'completed','voice_transcript',?,?,?,?,?)
             ON CONFLICT(id) DO UPDATE SET raw_text=excluded.raw_text,
               refined_text=excluded.refined_text,duration=excluded.duration,
               status='completed',updated_at=excluded.updated_at",
            rusqlite::params![id, raw_text, refined_text, duration, now, now],
        );
    })
    .await;
}

/// 一次听写的音频落盘。**每条听写都走这儿**，不只是失败的那些。
///
/// ── 为什么走原始二进制而不是普通参数 ──────────────────────────
///
/// 一段 60 秒的听写是 1.9 MB 的 wav。Tauri 默认会把 `Vec<u8>` 序列化成 JSON 的
/// 数字数组，1.9 MB 变成 ~7 MB 文本，光是 JSON.stringify + 解析就要卡住半秒多，
/// 而这一步正好发生在用户刚说完话、等着看结果的那一刻。
///
/// 所以用 `tauri::ipc::Request` 收原始 body。元数据跟着一起塞进 body 的头部，
/// 格式和引擎那条 ws 的音频帧同一个路数（preload.cjs:1418 的 encodeAudioFrame）：
///
///   [u32 LE 元数据字节数][元数据 JSON, UTF-8][wav 原始字节]
///
/// 不用 header 传元数据是因为 reason 里是中文（「没听清，峰值 0.003」这类），
/// HTTP header 只认可见 ASCII。
///
/// 冲突时只更新音频路径和时长，不动 status：万一这条已经转成功了，不能被一条
/// 迟到的失败记录降级回 audio_only。
#[tauri::command]
pub async fn save_audio_only(request: tauri::ipc::Request<'_>) -> Result<String, String> {
    let tauri::ipc::InvokeBody::Raw(body) = request.body() else {
        return Err("这条命令只收原始字节".into());
    };
    let body = body.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (meta, wav) = split_audio_body(&body)?;
        let id = meta.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_empty() || wav.is_empty() {
            return Err("缺 id 或者音频是空的".into());
        }
        // id 会被拼成文件名。真出现路径分隔符就是往别的目录写文件了。
        if id.contains(['/', '\\', ':']) || id.contains("..") {
            return Err(format!("id 不像个文件名：{id}"));
        }
        let duration = meta.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let reason = meta.get("reason").and_then(|v| v.as_str()).unwrap_or("");

        let dir = recordings_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let p = dir.join(format!("{id}.wav"));
        std::fs::write(&p, wav).map_err(|e| e.to_string())?;

        let path = p.to_string_lossy().into_owned();
        if let Ok(c) = open() {
            let now = now_iso();
            let _ = c.execute(
                "INSERT INTO history(id,status,mode,duration,audio_local_path,debug_info,created_at,updated_at)
                 VALUES(?,'audio_only','voice_transcript',?,?,?,?,?)
                 ON CONFLICT(id) DO UPDATE SET audio_local_path=excluded.audio_local_path,
                   duration=excluded.duration,updated_at=excluded.updated_at",
                rusqlite::params![id, duration, path, reason, now, now],
            );
        }
        trim_recordings(&dir);
        Ok(path)
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()))
}

/// 拆开上面那个 body。长度字段对不上就整个拒掉——按错误的偏移切出来的「wav」
/// 会被原样写进录音目录，回放时是一段噪音，而且看不出是从哪儿坏的。
fn split_audio_body(body: &[u8]) -> Result<(Map<String, Value>, &[u8]), String> {
    if body.len() < 4 {
        return Err("body 太短".into());
    }
    let n = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
    if 4 + n > body.len() {
        return Err(format!("元数据说有 {n} 字节，body 只有 {} 字节", body.len() - 4));
    }
    let meta: Map<String, Value> = serde_json::from_slice(&body[4..4 + n])
        .map_err(|e| format!("元数据不是 JSON 对象：{e}"))?;
    Ok((meta, &body[4 + n..]))
}

/// 用户把粘出来的字改了。记进这条历史，也记进纠错表当下次的偏置。
#[tauri::command]
pub async fn save_correction(id: String, wrong: String, right: String) {
    let (wrong, right) = (wrong.trim().to_string(), right.trim().to_string());
    if id.is_empty() || wrong.is_empty() || right.is_empty() || wrong == right {
        return;
    }
    let _ = tauri::async_runtime::spawn_blocking(move || {
        let Ok(c) = open() else { return };
        let now = now_iso();
        let _ = c.execute(
            "UPDATE history SET corrected_text=?,updated_at=? WHERE id=?",
            rusqlite::params![right, now, id],
        );
        let _ = c.execute(
            "INSERT INTO corrections(wrong_text,right_text,count,created_at,last_seen)
             VALUES(?,?,1,?,?)
             ON CONFLICT(wrong_text,right_text) DO UPDATE SET count=count+1,
               last_seen=excluded.last_seen",
            rusqlite::params![wrong, right, now, now],
        );
    })
    .await;
}

/// 这条听写触发了一次自学习，记一行。
///
/// 复用现成的 mode_meta 列存一行 JSON，不为一条动态开新表。
#[tauri::command]
pub async fn note_learned(
    history_id: String,
    term: String,
    tag_name: String,
    wrong: String,
    right: String,
) {
    if history_id.is_empty() || term.is_empty() {
        return;
    }
    let _ = tauri::async_runtime::spawn_blocking(move || {
        let Ok(c) = open() else { return };
        let now = now_iso();
        let cut = |s: &str| -> String { s.chars().take(600).collect() };
        let blob = json!({
            "learned": {
                "term": term, "tag": tag_name, "at": now,
                "wrong": cut(&wrong), "right": cut(&right),
            }
        })
        .to_string();
        let _ = c.execute(
            "UPDATE history SET mode_meta=?,updated_at=? WHERE id=?",
            rusqlite::params![blob, now, history_id],
        );
    })
    .await;
}

/// 开录那一刻读一次麦克风偏好。
///
/// 不能用 get_context 顺回来的那份快照：那份是**上一次**开录时拿的，设置页刚
/// 改完的选择要等到下下次才生效。这里只读两个键，不碰数据库。
#[tauri::command]
pub fn mic_pref() -> Value {
    let s = crate::settings::read();
    let get = |k: &str| s.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    json!({ "id": get("micDeviceId"), "label": get("micDeviceLabel") })
}

// ── 录音目录封顶 ──────────────────────────────────────────────────

/// 2 GB，约 18 小时。
const RECORDINGS_CAP: u64 = 2 * 1024 * 1024 * 1024;

/// 每条听写都留一份 wav，而历史保留时间默认是「永远」——按保留期清理那条路
/// 根本不会跑，不封顶这个目录就只涨不落。16 kHz 单声道 16 位 = 32 KB/s，一天
/// 说满一小时就是 115 MB，一年能把盘填了。
///
/// 满了从最旧的开始删。文字那一行留着（还有用），只把 audio_local_path 抹掉，
/// 免得历史页指着一个已经不在的文件。
///
/// 分两批：先删已经转出文字的——那些 wav 只是「还能回放一下」；转失败的排在
/// 最后，它们是当时说的那句话仅存的一份，删了「重新转录」就再也没东西可跑。
fn trim_recordings(dir: &std::path::Path) {
    struct F {
        path: String,
        size: u64,
        at: std::time::SystemTime,
        orphan: bool,
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<F> = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().map(|x| !x.eq_ignore_ascii_case("wav")).unwrap_or(true) {
            continue;
        }
        let Ok(m) = e.metadata() else { continue };
        files.push(F {
            path: p.to_string_lossy().into_owned(),
            size: m.len(),
            at: m.modified().unwrap_or(std::time::UNIX_EPOCH),
            orphan: true,
        });
    }
    let mut total: u64 = files.iter().map(|f| f.size).sum();
    if total <= RECORDINGS_CAP {
        return;
    }
    files.sort_by_key(|f| f.at);

    let c = open().ok();
    if let Some(c) = &c {
        for f in files.iter_mut() {
            let t: Option<String> = c
                .query_row("SELECT refined_text FROM history WHERE audio_local_path=?", [&f.path], |r| r.get(0))
                .ok()
                .flatten();
            f.orphan = t.filter(|s| !s.is_empty()).is_none();
        }
    }
    // 已经转出文字的排前面先删，孤儿排最后。
    let order: Vec<usize> = (0..files.len())
        .filter(|&i| !files[i].orphan)
        .chain((0..files.len()).filter(|&i| files[i].orphan))
        .collect();
    for i in order {
        if total <= RECORDINGS_CAP {
            break;
        }
        let f = &files[i];
        if std::fs::remove_file(&f.path).is_err() {
            continue;
        }
        total -= f.size;
        if let Some(c) = &c {
            let _ = c.execute("UPDATE history SET audio_local_path=NULL WHERE audio_local_path=?", [&f.path]);
        }
    }
}

// ── 录音文件 ──────────────────────────────────────────────────────

/// 读一整个 wav 回去。页面拿它画波形、算增益、喂给引擎重转录——那边是
/// fs.readFileSync 当场读，这边只能过 IPC。
///
/// 返回原始字节而不是数组：Tauri 默认会把 Vec<u8> 序列化成 JSON 数字数组，
/// 一段 3 分钟的录音 5.7 MB，变成 JSON 是二十多兆的文本，光解析就要卡住好几秒。
/// tauri::ipc::Response 走的是二进制通道，页面那边直接拿到 ArrayBuffer。
#[tauri::command]
pub fn read_audio(path: String) -> Result<tauri::ipc::Response, String> {
    let bytes = std::fs::read(&path).map_err(|e| format!("{path}：{e}"))?;
    Ok(tauri::ipc::Response::new(bytes))
}

// ── 时间戳 ────────────────────────────────────────────────────────

/// 和 Electron 版 `new Date().toISOString()` 一模一样的串：UTC、三位毫秒、结尾 Z。
///
/// 这个格式不是随便挑的。history 表里 created_at 存的就是它，而清理和取上文都靠
/// `datetime(created_at)` 让 SQLite 去解析——SQLite 认 ISO8601，但只认这一种写法：
/// 带时区偏移的、或者用空格分隔日期和时间的，`datetime()` 会返回 NULL，于是
/// 「按保留期清理」一条都删不掉，「最近 3 条上文」一条都取不到，而且两处都不报错。
/// 两版共用同一个库，这边写进去的时间戳那边也要能读，所以更不能改。
pub fn now_iso() -> String {
    const F: &[time::format_description::FormatItem] = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    time::OffsetDateTime::now_utc()
        .format(F)
        .unwrap_or_else(|_| String::from("1970-01-01T00:00:00.000Z"))
}

/// 录音目录。设置页的「打开录音文件夹」和 Electron 版指的是同一个地方。
pub fn recordings_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("localless").join("recordings")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 建一个临时库，验 row_to_json 是按列名取的、类型也对得上。
    /// 真库里那张表以后加列是迟早的事，加完这条测试还得过。
    #[test]
    fn 按列名取值() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE t(id TEXT, duration REAL, n INTEGER, gone TEXT);
             INSERT INTO t VALUES('a', 1.5, 7, NULL);",
        )
        .unwrap();
        let mut stmt = c.prepare("SELECT * FROM t").unwrap();
        let names: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
        let row = stmt
            .query_map([], |r| Ok(row_to_json(r, &names)))
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(row["id"], json!("a"));
        assert_eq!(row["duration"], json!(1.5));
        assert_eq!(row["n"], json!(7));
        assert_eq!(row["gone"], Value::Null);
    }

    /// 时间戳必须能被 SQLite 的 datetime() 解析，否则清理和取上文会静默失效。
    /// 这里只能验形状：`2026-09-21T03:04:05.678Z`，24 个字符，分隔符一个不差。
    #[test]
    fn 时间戳是带毫秒的_utc_iso() {
        let s = now_iso();
        assert_eq!(s.len(), 24, "{s}");
        assert!(s.ends_with('Z'), "{s}");
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
        assert_eq!(&s[10..11], "T");
        assert_eq!(&s[13..14], ":");
        assert_eq!(&s[16..17], ":");
        assert_eq!(&s[19..20], ".");
        assert!(s.chars().filter(|c| c.is_ascii_digit()).count() == 17, "{s}");
        // 真拿 SQLite 解一遍。格式错了 datetime() 返回 NULL，而不是报错。
        let c = Connection::open_in_memory().unwrap();
        let parsed: Option<String> = c
            .query_row("SELECT datetime(?)", [&s], |r| r.get(0))
            .unwrap();
        assert!(parsed.is_some(), "SQLite 解不动 {s}");
    }

    /// 清理的 days 必须是正数。0 和负数传进来会变成 datetime('now','-0 days')——
    /// 那是「现在之前」，等于一次把整张表删光。
    #[test]
    fn 非正数不清理() {
        assert_eq!(history_purge(0).unwrap(), 0);
        assert_eq!(history_purge(-3).unwrap(), 0);
    }

    fn frame(meta: &str, wav: &[u8]) -> Vec<u8> {
        let mut b = (meta.len() as u32).to_le_bytes().to_vec();
        b.extend_from_slice(meta.as_bytes());
        b.extend_from_slice(wav);
        b
    }

    /// 元数据和音频要按长度前缀切得干干净净。
    #[test]
    fn 音频帧的拆包() {
        let body = frame(r#"{"id":"abc","duration":1.5}"#, &[82, 73, 70, 70]);
        let (meta, wav) = split_audio_body(&body).unwrap();
        assert_eq!(meta["id"], json!("abc"));
        assert_eq!(meta["duration"], json!(1.5));
        assert_eq!(wav, &[82, 73, 70, 70]);
    }

    /// 中文 reason 的字节数和字符数不一样。前缀写的是字节数，切错一个字节后面
    /// 整段 wav 就错位了——写进录音目录是一段噪音，还看不出是哪儿坏的。
    #[test]
    fn 中文元数据按字节切() {
        let meta = r#"{"id":"x","reason":"没听清，峰值 0.003"}"#;
        assert_ne!(meta.len(), meta.chars().count(), "这条测试得用多字节内容");
        let body = frame(meta, &[1, 2, 3]);
        let (m, wav) = split_audio_body(&body).unwrap();
        assert_eq!(m["reason"], json!("没听清，峰值 0.003"));
        assert_eq!(wav, &[1, 2, 3]);
    }

    /// 长度字段对不上一律拒掉，不按错误的偏移硬切。
    #[test]
    fn 坏帧要拒掉() {
        assert!(split_audio_body(&[]).is_err());
        assert!(split_audio_body(&[1, 2]).is_err());
        let mut body = frame(r#"{"id":"a"}"#, &[9]);
        body[0] = 200; // 元数据长度吹成 200，body 根本没那么长
        assert!(split_audio_body(&body).is_err());
        assert!(split_audio_body(&frame("不是 JSON", &[9])).is_err());
    }
}

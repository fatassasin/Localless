// 自学习词库。搬自 preload.cjs:460-493（addLearnedWord / undoLearnedWord）。
//
// 一次听写结束、用户把粘出来的字改了之后，这边判断那个改动里有没有一个值得记住
// 的专有名词，有就塞进设置里的 customWords，下次开录当偏置词一起喂给引擎。
//
// ── 只合并 customWords 一个键 ────────────────────────────────────
//
// 绝不整份覆写 localless-settings.json。Electron 版踩过两次：
// 一次是写的时候被读走半个文件（不原子），一次是拿开录那一刻的旧快照把用户
// 刚在设置页改的开关盖了回去。所以这边走 settings::merge——它做的是
// 「读最新的 + 只替换这一个键 + 临时文件改名落盘」。

use serde_json::{json, Map, Value};

/// 归一化之后再比是不是同一个词。
///
/// 以前是精确比对，于是 GitHub/github、Codex/codex 各存一份，同一个词被反复
/// 「学会」——用户看到的是药丸每隔几天就弹一次「已添加进词库」，弹的还是同一个词。
fn norm(s: &str) -> String {
    s.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 毫秒时间戳。词条的 id/order/createdAt 用的都是它，和 Electron 版的
/// `Date.now()` 同一个数——设置页按 order 排序，两版共用同一份词表。
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 老词条是裸字符串。统一成对象，不然后面取 .text 全是 undefined。
fn normalize_words(raw: Option<&Value>) -> Vec<Value> {
    raw.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .enumerate()
                .map(|(i, w)| match w.as_str() {
                    Some(s) => json!({ "id": format!("manual-{i}"), "text": s, "origin": "manual" }),
                    None => w.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 挑一个分组。AI 给的分类名优先（按名字不分大小写匹配，或者直接给 id），
/// 匹配不到退回 people，再退回第一个分组，全空就是 general。
///
/// 分出来的是 (tag_id, 给人看的名字)——药丸上要显示「已添加进『技术词库』」，
/// 显示的是名字，存进词条的是 id。
fn pick_tag(tags: &[Value], tag_name: &str) -> (String, String) {
    let want = tag_name.trim().to_lowercase();
    let matched = (!want.is_empty())
        .then(|| {
            tags.iter().find(|g| {
                g.get("name").and_then(|v| v.as_str()).map(|n| n.to_lowercase()) == Some(want.clone())
                    || g.get("id").and_then(|v| v.as_str()) == Some(tag_name)
            })
        })
        .flatten();
    let grp = matched
        .or_else(|| tags.iter().find(|g| g.get("id").and_then(|v| v.as_str()) == Some("people")))
        .or_else(|| tags.first());
    let id = grp
        .and_then(|g| g.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("general")
        .to_string();
    let name = grp
        .and_then(|g| g.get("name"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&id)
        .to_string();
    (id, name)
}

/// 算出「加完之后的词表」和要回给页面的那条结果。已经有了就返回 None。
///
/// 纯函数，设置当参数喂进来：下面那几条测试要验的是「大小写不同算同一个词」
/// 这类判据，不该为此去动用户真正的词库文件。
fn plan(st: &Map<String, Value>, term: &str, tag_name: &str) -> Option<(Vec<Value>, Value)> {
    let term = term.trim();
    let key = norm(term);
    if key.is_empty() {
        return None;
    }
    let mut words = normalize_words(st.get("customWords"));
    if words
        .iter()
        .any(|w| norm(w.get("text").and_then(|v| v.as_str()).unwrap_or("")) == key)
    {
        return None;
    }
    let tags: Vec<Value> = st
        .get("wordTags")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let (tag, shown) = pick_tag(&tags, tag_name);
    let ms = now_ms();
    let id = format!("learned-{ms}");
    words.push(json!({
        "id": id, "text": term, "origin": "learned", "tag": tag,
        "order": ms, "createdAt": ms, "updatedAt": ms,
    }));
    Some((words, json!({ "id": id, "tagName": shown })))
}

/// 学一个新词。返回 null 表示没学——词是空的、已经有了、或者写盘被跳过了。
///
/// 调用点在药丸那条听写收尾的链路里，拿返回值决定要不要弹「已添加进词库」，
/// 所以必须等写盘结束再回，不能 fire-and-forget。
#[tauri::command]
pub fn add_learned_word(term: String, tag_name: String) -> Value {
    let st = crate::settings::read();
    let Some((words, out)) = plan(&st, &term, &tag_name) else {
        return Value::Null;
    };
    let mut patch = Map::new();
    patch.insert("customWords".into(), Value::Array(words));
    // merge 返回 None 是「这次写入被跳过了」（疑似读到了半个文件）。那种时候
    // 词其实没进去，不能回一个成功——药丸会弹「已添加」，而下次开录根本没这个词。
    if crate::settings::merge(patch).is_none() {
        return Value::Null;
    }
    out
}

/// 撤回刚学进去的那个词。药丸上那条提示里的「撤销」。
///
/// 裸字符串那些是用户手工加的，一律留着——它们没有 id，按 id 过滤会把它们全删了。
#[tauri::command]
pub fn undo_learned_word(id: String) -> Result<(), String> {
    if id.is_empty() {
        return Err("没给 id".into());
    }
    let st = crate::settings::read();
    let words: Vec<Value> = st
        .get("customWords")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter(|w| w.is_string() || w.get("id").and_then(|v| v.as_str()) != Some(id.as_str()))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let mut patch = Map::new();
    patch.insert("customWords".into(), Value::Array(words));
    crate::settings::merge(patch)
        .map(|_| ())
        .ok_or_else(|| "这次写入被跳过了（见日志）".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(words: Value, tags: Value) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("customWords".into(), words);
        m.insert("wordTags".into(), tags);
        m
    }

    fn tags() -> Value {
        json!([
            { "id": "general", "name": "通用" },
            { "id": "people", "name": "人名" },
            { "id": "tech", "name": "技术词库" },
        ])
    }

    /// 大小写、首尾空白、内部空格差异都算同一个词。少了这一条，同一个词会被
    /// 反复「学会」，用户每隔几天就看见一次同样的「已添加进词库」。
    #[test]
    fn 归一化后算同一个词() {
        let s = st(json!([{ "id": "a", "text": "GitHub" }]), tags());
        assert!(plan(&s, "github", "").is_none());
        assert!(plan(&s, "  GITHUB  ", "").is_none());
        assert!(plan(&s, "Git  Hub", "").is_some(), "中间有空格是另一个词");
        assert!(plan(&s, "git hub", "").is_some());
        // 但 "git  hub" 和 "git hub" 是同一个——内部连续空白压成一个。
        let s2 = st(json!([{ "id": "a", "text": "git  hub" }]), tags());
        assert!(plan(&s2, "git hub", "").is_none());
    }

    /// 空词不学。空串进去会变成一个空的偏置词，引擎那侧的提示词里多一个空行。
    #[test]
    fn 空词不学() {
        let s = st(json!([]), tags());
        assert!(plan(&s, "", "").is_none());
        assert!(plan(&s, "   ", "").is_none());
    }

    /// 老的裸字符串词条要能参与去重。不归一化的话 w.text 是 undefined，
    /// 用户手工加过的词会被当成没有，再学一遍。
    #[test]
    fn 裸字符串也算数() {
        let s = st(json!(["Codex", { "id": "a", "text": "别的" }]), tags());
        assert!(plan(&s, "codex", "").is_none());
    }

    /// 分组：名字匹配 → id 匹配 → people → 第一个 → general。
    #[test]
    fn 分组的挑法() {
        let t: Vec<Value> = tags().as_array().unwrap().clone();
        assert_eq!(pick_tag(&t, "技术词库"), ("tech".into(), "技术词库".into()));
        assert_eq!(pick_tag(&t, "tech"), ("tech".into(), "技术词库".into()));
        assert_eq!(pick_tag(&t, "查无此组"), ("people".into(), "人名".into()));
        assert_eq!(pick_tag(&t, ""), ("people".into(), "人名".into()));
        // 没有 people 就退回第一个
        let t2 = vec![json!({ "id": "x", "name": "X" })];
        assert_eq!(pick_tag(&t2, ""), ("x".into(), "X".into()));
        // 一个分组都没有
        assert_eq!(pick_tag(&[], ""), ("general".into(), "general".into()));
    }

    /// 新词条的形状要和 Electron 版一字不差——设置页那边按 order 排序、
    /// 按 origin 显示「自动学习」的标记，两版共用同一份词表。
    #[test]
    fn 新词条的形状() {
        let s = st(json!([]), tags());
        let (words, out) = plan(&s, " 声学模型 ", "技术词库").unwrap();
        assert_eq!(words.len(), 1);
        let w = &words[0];
        assert_eq!(w["text"], json!("声学模型"), "首尾空白要去掉");
        assert_eq!(w["origin"], json!("learned"));
        assert_eq!(w["tag"], json!("tech"));
        assert!(w["order"].as_i64().unwrap() > 0);
        assert_eq!(w["id"], out["id"]);
        assert_eq!(out["tagName"], json!("技术词库"), "回给药丸的是名字不是 id");
    }
}

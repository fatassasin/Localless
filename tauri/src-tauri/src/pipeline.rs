// 「上一次语音识别流程」那张时间线的后端。搬自 Electron 版 main.js:1130-1141。
//
// 引擎把每个环节的进展报给药丸那条 ws，药丸转发到这里，这里攒成一条按 audio_id
// 归拢的记录，再推给设置页。设置页自己不连引擎——它随时可能关着，而引擎的进展
// 是一次性的，错过就没了，所以必须有人在中间存着。
//
// 只存 50 条，按「最近更新」淘汰。这不是内存优化：这张表只给人看最近一次听写，
// 攒多了既没人看，又会让设置页每次打开都要渲染一大堆过期流程。

use parking_lot::Mutex;
use serde_json::{Map, Value};
use tauri::{AppHandle, Emitter};

/// 插入顺序即淘汰顺序，所以用 Vec 而不是 HashMap——JS 那边靠的是 Map 的插入序，
/// 每次更新都 delete 再 set 把它挪到末尾。这边照搬同一个动作。
static PIPELINES: Mutex<Vec<(String, Value)>> = Mutex::new(Vec::new());
const CAP: usize = 50;

/// 药丸报上来一条环节更新。
///
/// 合并是浅合并，和 Electron 版 `{...old, ...update}` 一致：引擎分两次报同一个环节
/// （先 running 只有 model，后 completed 才带 elapsed_ms 和文本）时，后一条不能把
/// 前一条的字段抹掉——抹掉的表现是时间线上的模型名突然消失。
#[tauri::command]
pub fn pipeline_update(app: AppHandle, update: Value) {
    let Some(u) = update.as_object() else { return };
    let (Some(id), Some(stage)) = (
        u.get("audio_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()),
        u.get("stage").and_then(|v| v.as_str()).filter(|s| !s.is_empty()),
    ) else {
        return;
    };
    let (id, stage) = (id.to_string(), stage.to_string());

    let pipeline = {
        let mut xs = PIPELINES.lock();
        // 取出来（连带从原位置摘掉），改完再压回末尾 —— 这就是 JS 那边
        // delete + set 的效果。
        let mut p = match xs.iter().position(|(k, _)| *k == id) {
            Some(i) => xs.remove(i).1,
            None => {
                let mut m = Map::new();
                m.insert("audio_id".into(), Value::from(id.clone()));
                m.insert("stages".into(), Value::Object(Map::new()));
                Value::Object(m)
            }
        };

        {
            let root = p.as_object_mut().expect("上面刚造的就是对象");
            let stages = root
                .entry("stages")
                .or_insert_with(|| Value::Object(Map::new()));
            // stages 被写成了别的类型只可能是脏数据，直接换成空对象，
            // 不能 panic —— 这条链路跑在听写过程中。
            if !stages.is_object() {
                *stages = Value::Object(Map::new());
            }
            let stages = stages.as_object_mut().unwrap();
            let slot = stages.entry(stage).or_insert_with(|| Value::Object(Map::new()));
            let mut merged = slot.as_object().cloned().unwrap_or_default();
            for (k, v) in u {
                merged.insert(k.clone(), v.clone());
            }
            *slot = Value::Object(merged);
            root.insert("updated_at".into(), Value::from(millis_now()));
        }

        xs.push((id, p.clone()));
        while xs.len() > CAP {
            xs.remove(0);
        }
        p
    };

    // 设置页没开就没人收，这是正常情形（多数听写发生在设置页关着的时候）。
    let _ = app.emit_to(
        crate::settingswin::LABEL,
        "localless://pipeline-update",
        pipeline,
    );
}

/// 设置页刚打开时补上此前错过的。Electron 版是 `Array.from(pipelines.values())`，
/// 页面那侧照单全收再逐条 acceptPipeline，所以顺序（旧 → 新）要保持。
#[tauri::command]
pub fn pipeline_latest() -> Vec<Value> {
    PIPELINES.lock().iter().map(|(_, v)| v.clone()).collect()
}

/// Date.now()。只当作「哪条更新」的序号用，页面不显示它。
fn millis_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 把命令里那段纯逻辑单独摘出来测：上面的 pipeline_update 要 AppHandle，
    /// 测试里造不出来。合并规则和淘汰规则必须和它逐字一致。
    fn merge(xs: &mut Vec<(String, Value)>, update: &Map<String, Value>) -> Option<Value> {
        let id = update.get("audio_id")?.as_str().filter(|s| !s.is_empty())?.to_string();
        let stage = update.get("stage")?.as_str().filter(|s| !s.is_empty())?.to_string();
        let mut p = match xs.iter().position(|(k, _)| *k == id) {
            Some(i) => xs.remove(i).1,
            None => json!({ "audio_id": id, "stages": {} }),
        };
        {
            let root = p.as_object_mut().unwrap();
            let stages = root.get_mut("stages").unwrap().as_object_mut().unwrap();
            let slot = stages.entry(stage).or_insert_with(|| json!({}));
            let mut merged = slot.as_object().cloned().unwrap_or_default();
            for (k, v) in update {
                merged.insert(k.clone(), v.clone());
            }
            *slot = Value::Object(merged);
        }
        xs.push((id, p.clone()));
        while xs.len() > CAP {
            xs.remove(0);
        }
        Some(p)
    }

    fn up(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    /// 同一个环节报两次，后一次不能把前一次的字段冲掉。引擎就是这么报的：
    /// running 那条只有 model，completed 那条才有 elapsed_ms。
    #[test]
    fn 同一环节的两次更新是浅合并() {
        let mut xs = Vec::new();
        merge(&mut xs, &up(json!({"audio_id":"a","stage":"asr","status":"running","model":"qwen3"})));
        let p = merge(
            &mut xs,
            &up(json!({"audio_id":"a","stage":"asr","status":"completed","elapsed_ms":820})),
        )
        .unwrap();
        let asr = &p["stages"]["asr"];
        assert_eq!(asr["model"], json!("qwen3"), "前一次的 model 被冲掉了");
        assert_eq!(asr["status"], json!("completed"));
        assert_eq!(asr["elapsed_ms"], json!(820));
        assert_eq!(xs.len(), 1, "同一个 audio_id 不该变成两条");
    }

    /// 更新过的那条要挪到末尾，不然它会先于刚来的新听写被淘汰——表现成
    /// 「一次长听写还没完，流程就从界面上消失了」。
    #[test]
    fn 更新会把记录挪到末尾() {
        let mut xs = Vec::new();
        merge(&mut xs, &up(json!({"audio_id":"a","stage":"asr"})));
        merge(&mut xs, &up(json!({"audio_id":"b","stage":"asr"})));
        merge(&mut xs, &up(json!({"audio_id":"a","stage":"completed"})));
        assert_eq!(xs.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["b", "a"]);
    }

    /// 超出上限从最旧的开始丢。
    #[test]
    fn 只留最近五十条() {
        let mut xs = Vec::new();
        for i in 0..CAP + 7 {
            merge(&mut xs, &up(json!({"audio_id": i.to_string(), "stage": "asr"})));
        }
        assert_eq!(xs.len(), CAP);
        assert_eq!(xs[0].0, "7");
        assert_eq!(xs[CAP - 1].0, (CAP + 6).to_string());
    }

    /// 缺 audio_id 或缺 stage 的更新整条丢掉，不能建出一条空壳记录——
    /// 空壳会占掉 50 条里的一格，还会在时间线上渲染成一行全「等待」。
    #[test]
    fn 缺字段的更新直接丢掉() {
        let mut xs = Vec::new();
        assert!(merge(&mut xs, &up(json!({"stage":"asr"}))).is_none());
        assert!(merge(&mut xs, &up(json!({"audio_id":"a"}))).is_none());
        assert!(merge(&mut xs, &up(json!({"audio_id":"","stage":"asr"}))).is_none());
        assert!(xs.is_empty());
    }
}

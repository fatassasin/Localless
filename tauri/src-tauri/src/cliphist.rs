// Win+V 剪贴板历史的顺序。只在「自动清理剪贴板」关着时用。
//
// 关着的意思是：听写结果要在 Win+V 里留一条记录。可 Win+V 按写入先后排，听写
// 那一条一写进去，用户原来复制的东西就退到了第二位——用户明确不要这个。
//
// 历史的顺序没有接口能直接改。实测过两条路：
// - SetHistoryItemAsContent（Win+V 里点一条的效果）：当前剪贴板换回去了，但列表
//   顺序一动不动，旧条目还在第二位。
// - 把旧内容再写一次（留痕）：它会回到最上面，但 Win+V 不去重，原来那条还在
//   下面，同一份东西出现两遍。
// 所以只能走第二条，再把下面那条旧的删掉。结果是「旧内容、听写、……」。
//
// 删之前必须确认删的就是它。当前剪贴板上不带「我是历史里哪一条」的标记（实测
// 连从 Win+V 里点出来的那份也不带），只能比内容：有文本比文本，没文本的图片/
// 文件只比类型。类型比对是弱的，所以顺序定死为「先写新的、确认新的真进了历史、
// 再删旧的」——新的没出现（比如超过 Win+V 4MB 的单条上限），旧的就一概不动。

use crate::paste::Snapshot;
use windows::core::HSTRING;
use windows::ApplicationModel::DataTransfer::{
    Clipboard, ClipboardHistoryItem, ClipboardHistoryItemsResultStatus, StandardDataFormats,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};

/// Clipboard 这个 WinRT 类只肯在单线程套间里激活，多线程套间里一律
/// 0x8000001D「不支持从 MTA 中激活单线程类」——而 Tauri 的后台线程都是 MTA。
/// 借来的线程也不能顺手改成 STA：线程池会把它拿去干别的。所以每次现起一条
/// 短命的 STA 线程，历史条目这些对象只在这条线程里活，不往外带。
fn sta<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|s| {
        s.spawn(|| unsafe {
            let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let out = f();
            if hr.is_ok() {
                CoUninitialize();
            }
            out
        })
        .join()
        .expect("剪贴板历史线程崩了")
    })
}

fn items() -> Option<Vec<ClipboardHistoryItem>> {
    let r = Clipboard::GetHistoryItemsAsync().ok()?.get().ok()?;
    if r.Status().ok()? != ClipboardHistoryItemsResultStatus::Success {
        return None;
    }
    let v = r.Items().ok()?;
    let n = v.Size().ok()?;
    Some((0..n).filter_map(|i| v.GetAt(i).ok()).collect())
}

fn norm(s: &str) -> String {
    s.replace("\r\n", "\n").trim_end().to_string()
}

/// 这一条历史是不是快照里那份内容。
fn same(item: &ClipboardHistoryItem, snap: &Snapshot) -> bool {
    let Ok(view) = item.Content() else { return false };
    let has = |f: windows::core::Result<HSTRING>| f.and_then(|f| view.Contains(&f)).unwrap_or(false);
    if let Some(text) = snap.text() {
        if !has(StandardDataFormats::Text()) {
            return false;
        }
        return view
            .GetTextAsync()
            .and_then(|op| op.get())
            .is_ok_and(|got| norm(&got.to_string_lossy()) == norm(&text));
    }
    if snap.has_image() {
        return has(StandardDataFormats::Bitmap());
    }
    if snap.has_files() {
        return has(StandardDataFormats::StorageItems());
    }
    false
}

/// 借剪贴板之前调：当前剪贴板的内容是不是就是 Win+V 最上面那条。是就返回它的
/// Id，还的时候凭它把这条删掉；不是（历史关着、内容本来就不进历史、对不上）
/// 就是 None，还的时候不碰历史。
pub fn top_if_same(snap: &Snapshot) -> Option<HSTRING> {
    if snap.hides_itself() {
        return None;
    }
    sta(|| {
        let top = items()?.into_iter().next()?;
        same(&top, snap).then(|| top.Id().ok()).flatten()
    })
}

/// 等听写那一条真进了 Win+V 再往回还。历史服务是收到剪贴板变更通知之后异步去
/// 读的，读之前内容就被换回去的话，它读到的是原内容，听写那条就记丢了——
/// 关掉自动清理的人要的恰恰是这条记录。最多等一秒；历史关着就不等。
pub fn wait_recorded(text: &str) {
    sta(|| wait_recorded_sta(text))
}

fn wait_recorded_sta(text: &str) {
    let want = norm(text);
    for _ in 0..20 {
        let Some(list) = items() else { return };
        let got = list.first().and_then(|top| {
            top.Content().ok()?.GetTextAsync().ok()?.get().ok()
        });
        if got.is_some_and(|t| norm(&t.to_string_lossy()) == want) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// 旧内容已经重新写回剪贴板（留痕）之后调：等它在 Win+V 顶上出现，再删掉
/// 下面那条旧的。历史服务是异步记的，所以要等；等不到就什么都不删。
pub fn drop_older(snap: &Snapshot, old: &HSTRING) -> bool {
    sta(|| drop_older_sta(snap, old))
}

fn drop_older_sta(snap: &Snapshot, old: &HSTRING) -> bool {
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let Some(list) = items() else { return false };
        let Some(top) = list.first() else { continue };
        if top.Id().is_ok_and(|id| &id == old) || !same(top, snap) {
            continue;
        }
        let Some(stale) = list.iter().skip(1).find(|i| i.Id().is_ok_and(|id| &id == old)) else {
            return false;
        };
        return Clipboard::DeleteItemFromHistory(stale).unwrap_or(false);
    }
    false
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Win+V 最上面 n 条的文本，没文本的记成空串。
    pub fn texts(n: usize) -> Vec<String> {
        sta(|| texts_sta(n))
    }

    fn texts_sta(n: usize) -> Vec<String> {
        items()
            .unwrap_or_default()
            .iter()
            .take(n)
            .map(|i| {
                i.Content()
                    .and_then(|v| v.GetTextAsync())
                    .and_then(|op| op.get())
                    .map(|h| h.to_string_lossy())
                    .unwrap_or_default()
            })
            .collect()
    }

    pub fn purge(prefix: &str) {
        sta(|| purge_sta(prefix))
    }

    fn purge_sta(prefix: &str) {
        for i in items().unwrap_or_default() {
            let t = i.Content().and_then(|v| v.GetTextAsync()).and_then(|op| op.get());
            if t.is_ok_and(|t| t.to_string_lossy().starts_with(prefix)) {
                let _ = Clipboard::DeleteItemFromHistory(&i);
            }
        }
    }
}


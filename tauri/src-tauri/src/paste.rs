// 粘贴转写结果，以及把文字写进剪贴板而不留进 Win+V 历史。
//
// 为什么写剪贴板这件事必须自己做 Win32，不能用现成的剪贴板库：
//
//   Windows 的剪贴板历史（Win+V）按写入次数记条目，覆盖当前剪贴板删不掉已经
//   记下的那一条。要让听写内容「能 Ctrl+V 但不进历史」，唯一的办法是在写入时
//   就挂上 CanIncludeInClipboardHistory 这个剪贴板格式，而且它必须和文本在
//   **同一次** OpenClipboard/CloseClipboard 里设置，否则不算同一份内容。
//   一般的剪贴板封装每次写入都会先清空再写，两次调用挂不到一起。
//
// 用 SendInput 注入 Shift+Insert（Windows 通用的粘贴键，等价于 Ctrl+V）。
//
// 为什么不用 Ctrl+V —— 这条路走了两轮，两次的解释都不对，第三次才测出真相：
//
//   最早是 SendKeys('^v')，为了躲开拼音输入法要先关输入法再开回来，而「关掉再
//   开回来」本身必然出 bug（Close 和 Restore 各自重新取一次 GetForegroundWindow，
//   中间焦点一动就恢复到别人身上；抛异常时恢复整个被跳过）。对用户就是
//   「每听写完一次输入法变英文」。
//
//   改成 SendInput 注入 Ctrl+V 之后不用碰输入法了，但裸 v 还在偶发。当时归咎于
//   低级钩子超时丢事件，于是加了 dwExtraInfo 戳让钩子认戳直接放行。裸 v 变少了，
//   没有消失。
//
//   实测（42 次注入 + 插队对照）证明真正的机制是：四条事件是一次 SendInput 提交
//   进输入队列没错，但**队列里的事件之间可以被别的进程插队**。只要有一条外来的
//   Ctrl 抬起落在我们的 Ctrl↓ 和 V↓ 中间，V 就以普通字符键的身份到达目标，微软
//   拼音把它当拼音，屏幕上留下字面的 v。本机这种流量近乎为零，所以是「偶尔」；
//   一旦连上远程控制软件（UU远程 / GameViewer 之类），它会持续把客户端的修饰键
//   状态同步到主机、包括「这个键其实没按住」的 keyup，于是变成「大多数时间」。
//   顺带说明，旧注释里「超过 LowLevelHooksTimeout 就丢事件」在这台机器上不成立：
//   该值被设成了 25000ms，慢钩子只会被等，不会被丢。
//
// 换 Shift+Insert 不是因为它更不容易被插队——插队一样会发生——而是因为失手的
// 后果完全不同。Shift 丢了，Insert 不是字符键，最坏只是切一下改写模式，绝不会
// 往用户的文字里掺东西。对照实验里人为插队 8/8 次都是「什么都没粘上」，零次掺
// 字符；同样的插队下 Ctrl+V 是 4/4 次把 v 写进了文档。
//
// 代价是粘贴仍可能静默地什么都没做（这条通道没有回执，SendInput 只保证事件进了
// 队列，不保证目标收下）。丢一次要用户重说一遍，比在文档里留下垃圾字符轻。

use crate::win::{key, send, wide};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardFormatNameW, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, VK_INSERT, VK_SHIFT,
};

const CF_UNICODETEXT: u32 = 13;

/// 剪贴板堆块必须是 GMEM_MOVEABLE。SetClipboardData 成功之后所有权归系统，
/// 我们不能再 GlobalFree，否则就是释放别人的内存。
unsafe fn alloc(bytes: &[u8]) -> Option<HGLOBAL> {
    let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).ok()?;
    let p = GlobalLock(h);
    if p.is_null() {
        return None;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
    let _ = GlobalUnlock(h);
    Some(h)
}

/// 剪贴板是全机共享的，别的程序可能正开着。抢不到就等一下再试。
unsafe fn open_retry() -> bool {
    for _ in 0..12 {
        if OpenClipboard(None).is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

/// 读当前剪贴板里的文本。粘贴前要先存下用户原来的内容，粘完还回去。
pub fn read_text() -> Option<String> {
    unsafe {
        if !open_retry() {
            return None;
        }
        let out = (|| {
            let h = GetClipboardData(CF_UNICODETEXT).ok()?;
            let p = GlobalLock(HGLOBAL(h.0)) as *const u16;
            if p.is_null() {
                return None;
            }
            let mut n = 0usize;
            while *p.add(n) != 0 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
            let _ = GlobalUnlock(HGLOBAL(h.0));
            Some(s)
        })();
        let _ = CloseClipboard();
        out
    }
}

/// 一次写入要留下多少痕迹。
///
/// 留痕那一档不是「省事的退路」，是「自动清理剪贴板」关掉时用户明确要的行为：
/// 这一条听写要出现在 Win+V 里，供他等会儿再粘一次。所以不能无条件不留痕——
/// 那是把开关做反了。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Trace {
    /// 照常：进 Win+V，剪贴板监听程序（远控同步、剪贴板管理器）也都看得见。
    Record,
    /// 不进 Win+V、不上云剪贴板。别的监听程序照样看得见——药丸上那颗「复制」
    /// 走这档：用户主动复制的东西，远控那头也许正等着它同步过去。
    NoHistory,
    /// 借剪贴板粘贴的那一下：谁都别理它。
    ///
    /// 只挡 Win+V 不够。GameViewer 这类远控软件、Logi Flow 这类跨机同步，都不认
    /// CanIncludeInClipboardHistory；它们把听写结果同步走，回头再原样写回本机，
    /// 这次写入不带任何标记——Win+V 里就又多出一条听写，用户原来复制的东西被挤到
    /// 第二位。实测历史里两条听写结果的时间戳和听写那一刻分秒不差，就是这么来的。
    /// ExcludeClipboardContentFromMonitorProcessing 是微软给所有剪贴板监听程序定的
    /// 「这份别处理」，Clipboard Viewer Ignore 是剪贴板管理器们（Ditto 等）认的那个。
    Hidden,
}

impl Trace {
    fn flags(self) -> &'static [&'static str] {
        match self {
            Trace::Record => &[],
            Trace::NoHistory => &["CanIncludeInClipboardHistory", "CanUploadToCloudClipboard"],
            Trace::Hidden => &[
                "CanIncludeInClipboardHistory",
                "CanUploadToCloudClipboard",
                "ExcludeClipboardContentFromMonitorProcessing",
                "Clipboard Viewer Ignore",
            ],
        }
    }
}

fn register(name: &str) -> u32 {
    let w = wide(name);
    unsafe { RegisterClipboardFormatW(windows::core::PCWSTR(w.as_ptr())) }
}

/// 在已经打开的剪贴板上挂标记格式。值都是 DWORD 0。设不上不算致命——内容已经
/// 进去了，粘贴照常，只是会多留一条痕。
unsafe fn put_flags(trace: Trace) {
    for name in trace.flags() {
        let fmt = register(name);
        if fmt == 0 {
            continue;
        }
        if let Some(h) = alloc(&[0u8; 4]) {
            if SetClipboardData(fmt, Some(HANDLE(h.0))).is_err() {
                let _ = GlobalFree(Some(h));
            }
        }
    }
}

/// 写剪贴板。标记格式必须和文本在**同一次** Open/Close 里挂上，见文件头。
pub fn set_text(text: &str, trace: Trace) -> bool {
    unsafe {
        let units: Vec<u16> = wide(text);
        let bytes = std::slice::from_raw_parts(units.as_ptr() as *const u8, units.len() * 2);
        let h_text = match alloc(bytes) {
            Some(h) => h,
            None => return false,
        };
        if !open_retry() {
            let _ = GlobalFree(Some(h_text));
            return false;
        }
        let _ = EmptyClipboard();
        let ok = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(h_text.0))).is_ok();
        if ok {
            put_flags(trace);
        } else {
            let _ = GlobalFree(Some(h_text));
        }
        let _ = CloseClipboard();
        ok
    }
}

// ── 整份剪贴板的快照 ───────────────────────────────────────────────
//
// 以前借剪贴板前只记 read_text()，还的时候也只还文本。用户复制的要是截图、文件、
// 带格式的网页文字，read_text 拿到的是 None，还回去的是一段空文本——听写完
// Ctrl+V 什么都粘不出来，只能去 Win+V 里翻。所以现在借之前把每一种格式都抄下来，
// 还的时候原样摆回去。

const CF_TEXT: u32 = 1;
const CF_DIB: u32 = 8;
const CF_HDROP: u32 = 15;
const CF_DIBV5: u32 = 17;

/// 装的不是 HGLOBAL 的格式：位图/调色板/图元文件是 GDI 句柄，DSP 系列和私有段
/// 是程序自己的东西，GlobalSize 读它们要么读错要么崩。位图不会因此丢——系统在
/// CF_BITMAP 和 CF_DIB 之间自动互转，抄 CF_DIB 那份就够了。
fn not_memory(fmt: u32) -> bool {
    matches!(fmt, 2 | 3 | 9 | 14 | 0x80 | 0x82 | 0x83 | 0x8E) || (0x200..=0x3FF).contains(&fmt)
}

/// OLE 剪贴板自己的簿记：一个指向数据对象所在窗口，一个是它那份格式清单。
/// 原样抄回去就是指着一个早就不在的对象，OleGetClipboard 会照着它去要数据。
const OLE_BOOKKEEPING: [&str; 2] = ["DataObject", "Ole Private Data"];

/// 快照里的格式全抄下来最多这么大。4K 截图的 CF_DIB 和 CF_DIBV5 各 33MB，
/// 再加一份 PNG，一百来兆；超过这个数就当抄不下，退回「不还原」。
const MAX_BYTES: usize = 256 << 20;

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Snapshot {
    items: Vec<(u32, Vec<u8>)>,
}

impl Snapshot {
    #[cfg(test)]
    pub fn of_text(text: &str) -> Self {
        let units = wide(text);
        let bytes = units.iter().flat_map(|u| u.to_le_bytes()).collect();
        Snapshot { items: vec![(CF_UNICODETEXT, bytes)] }
    }

    fn get(&self, fmt: u32) -> Option<&[u8]> {
        self.items.iter().find(|(f, _)| *f == fmt).map(|(_, b)| b.as_slice())
    }

    pub fn text(&self) -> Option<String> {
        let b = self.get(CF_UNICODETEXT)?;
        let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        Some(String::from_utf16_lossy(&units[..end]))
    }

    pub fn has_image(&self) -> bool {
        self.get(CF_DIB).is_some() || self.get(CF_DIBV5).is_some()
    }

    pub fn has_files(&self) -> bool {
        self.get(CF_HDROP).is_some()
    }

    /// 原来那份内容自己就声明过别进历史（密码管理器都这么干）。这种东西还回去
    /// 可以，但绝不能为了挪顺序把它重新写进 Win+V。
    pub fn hides_itself(&self) -> bool {
        let no = self
            .get(register("CanIncludeInClipboardHistory"))
            .is_some_and(|b| b.len() >= 4 && b[..4] == [0, 0, 0, 0]);
        no || self.get(register("ExcludeClipboardContentFromMonitorProcessing")).is_some()
    }
}

fn format_name(fmt: u32) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClipboardFormatNameW(fmt, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// 抄下当前剪贴板的每一种格式。打不开剪贴板或者太大就是 None——调用方据此
/// 决定不还原，总比还回去一份残缺的强。剪贴板本来就是空的是 Some(空快照)。
pub fn snapshot() -> Option<Snapshot> {
    unsafe {
        if !open_retry() {
            return None;
        }
        let out = (|| {
            let mut items = Vec::new();
            let mut total = 0usize;
            let mut fmt = 0u32;
            loop {
                fmt = EnumClipboardFormats(fmt);
                if fmt == 0 {
                    break;
                }
                if not_memory(fmt) || OLE_BOOKKEEPING.contains(&format_name(fmt).as_str()) {
                    continue;
                }
                // 这两种都能从 CF_UNICODETEXT 自动转出来，抄了反而可能和它对不上。
                if matches!(fmt, CF_TEXT | 7) && items.iter().any(|(f, _)| *f == CF_UNICODETEXT) {
                    continue;
                }
                let Ok(h) = GetClipboardData(fmt) else { continue };
                let h = HGLOBAL(h.0);
                let size = GlobalSize(h);
                if size == 0 {
                    continue;
                }
                total += size;
                if total > MAX_BYTES {
                    return None;
                }
                let p = GlobalLock(h) as *const u8;
                if p.is_null() {
                    continue;
                }
                items.push((fmt, std::slice::from_raw_parts(p, size).to_vec()));
                let _ = GlobalUnlock(h);
            }
            Some(Snapshot { items })
        })();
        let _ = CloseClipboard();
        out
    }
}

/// 把快照原样摆回剪贴板，再按 `trace` 挂标记。空快照 = 清空剪贴板。
pub fn restore(snap: &Snapshot, trace: Trace) -> bool {
    unsafe {
        if !open_retry() {
            return false;
        }
        let _ = EmptyClipboard();
        let mut ok = true;
        for (fmt, bytes) in &snap.items {
            let Some(h) = alloc(bytes) else {
                ok = false;
                continue;
            };
            if SetClipboardData(*fmt, Some(HANDLE(h.0))).is_err() {
                let _ = GlobalFree(Some(h));
                ok = false;
            }
        }
        if !snap.items.is_empty() {
            put_flags(trace);
        }
        let _ = CloseClipboard();
        ok
    }
}

/// 编辑键区那颗 Insert 是扩展键，必须带 KEYEVENTF_EXTENDEDKEY。不带的话它跟
/// 小键盘的 0 共用虚拟键码，程序会按 Num Lock 的状态去解释，行为不一致。
pub fn press_paste() -> u32 {
    let buf = [
        key(VK_SHIFT.0, Default::default()),
        key(VK_INSERT.0, KEYEVENTF_EXTENDEDKEY),
        key(VK_INSERT.0, KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP),
        key(VK_SHIFT.0, KEYEVENTF_KEYUP),
    ];
    send(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(s: &Snapshot) -> Vec<String> {
        s.items
            .iter()
            .map(|(f, b)| {
                let n = format_name(*f);
                format!("{}:{}", if n.is_empty() { f.to_string() } else { n }, b.len())
            })
            .collect()
    }

    /// 动真剪贴板，默认不跑。先在外面往剪贴板里放点东西，再跑这个看抄回来的格式。
    #[test]
    #[ignore]
    fn 快照原样还原() {
        let before = snapshot().expect("抄不下");
        println!("抄到: {:?}", names(&before));
        restore(&before, Trace::Hidden);
        let after = snapshot().expect("抄不下");
        println!("还原后: {:?}", names(&after));
    }
}

// 录音链路。搬自 Electron 版 preload.cjs:571-1695 的 RECORDER 块。
//
// 这一份是逐段移植，判据、常量、注释全部照搬——那些注释记的是一次次查出来的坑，
// 不是装饰。只有两类地方改了，都在下面写明：
//
// ── 改了什么 ──────────────────────────────────────────────────────
//
// 1. `window.locallessNative.X()` 全部变成 `invoke('x', {...})`，而且**全是异步的**。
//    Electron 那边有几个是 contextBridge 的同步调用（getContext / micPref /
//    addLearnedWord），这边只能 await。受影响的三处都已经改成 async 函数。
//    Tauri 的参数名走 camelCase（rawText → raw_text），和 settings.html 一致。
//
// 2. `ipcRenderer.send/on/invoke` 换成 `invoke` 和 `listen`：
//      localless:mute            → invoke('mute_set', {on})
//      localless:recorder-state  → invoke('recorder_state', {detail})
//      localless:device-state    → invoke('device_state', {device})
//      localless:encoder-pids    → invoke('encoder_pids', {pids})
//      localless:remote-session  → listen('localless://remote-session')
//      localless:remote-mic-now  → invoke('remote_mic_now')
//      window.__llToggle 的返回值 → listen('localless://toggle') 里自己回报
//                                   （Tauri 的 eval 没有返回值，理由见 recorder.rs 头部）
//
// 3. saveAudioOnly 改走原始字节。Tauri 默认把 Uint8Array 序列化成 JSON 数字数组，
//    一段 60 秒的听写（1.9 MB wav）会变成 ~7 MB 文本，卡在用户刚说完话的那一刻。
//    格式 [u32 LE 元数据长度][元数据 JSON][wav]，见 history.rs 的 split_audio_body。
//
// 其余一字未动。
(function () {
  if (window.__locallessRecorder) return;
  const invoke = (c, a) => window.__TAURI__.core.invoke(c, a);
  const listen = (n, f) => window.__TAURI__.event.listen(n, f);
  // 观测日志是纯旁路，失败了也绝不该把听写带下去。
  const dictLog = (line) => { try { invoke('dict_log', { line }).catch(() => {}); } catch {} };

  const SR = 16000;
  let stream = null, ctx = null, src = null, proc = null;
  let activeRecording = null, startingRecording = null, micReady = false;
  let ws = null, wsReady = false;
  const jobs = new Map();
  const cancelledIds = new Set();
  // thinking 卡住时的出口。显存占满那一轮基本就是干等，等满整个超时预算毫无意义
  // ——预算随录音长度放大，两分钟以上的听写要等好几分钟才轮到超时兜底。
  // thinking 满 30 秒还没结果，药丸右侧就冒出一个"存音频"按钮：点它＝放弃这次
  // 生成、把录音落进历史，回头在历史页重新转录。
  // 固定 30 秒而不是按超时取比例：越长的录音预算越大，按比例算等得越久，
  // 正好和"让我早点叫停"的诉求反着来。
  const SAVE_OFFER_MS = 30000;
  let saveOffer = null;
  // job 上现在挂着两个定时器，每条收尾路径都得两个一起清。漏掉 saveTimer 的话，
  // 一个早就结束的 job 会在 30 秒后把"存音频"按钮弹回屏幕上。
  function clearJobTimers(job) {
    if (!job) return;
    for (const t of [job.timer, job.saveTimer]) clearTimeout(t);
    if (saveOffer === job.id) saveOffer = null;
  }
  let pendingEdit = null, learnTimer = null;
  let currentContext = { settings: {}, history: [], corrections: [], focused: {} };

  // 自动清理剪贴板：只管听写结果在 Win+V 里留不留一条记录。两种模式下粘贴
  // 借用完都把用户原来复制的东西原样还回去——Ctrl+V 永远是它（见 deliver.rs）。
  function clipboardAutoClean(job) {
    const st = (job && job.context && job.context.settings) || currentContext.settings || {};
    return st.clipboardAutoCleanEnabled !== false;
  }

  // 光标不在可写输入框里时的收尾：把全文摆在药丸上让用户自己复制。
  // 顺序很重要 —— CustomEvent 是同步派发的，必须先 draw 再 result，否则
  // done 分支会在同一帧把 result 的 class 覆盖掉并 hideLater(0)，药丸刚
  // 建好就被抹掉（这正是"最近没有跳出过"的原因）。
  function showResultPill(text, job) {
    // 开关关着时在 Win+V 里留一条记录，但剪贴板本身还给用户原来的内容。
    // 要这段文字就点药丸上的复制按钮。
    if (!clipboardAutoClean(job)) invoke('clipboard_record', { text }).catch(() => {});
    draw({ t: 'done', s: '' });
    window.dispatchEvent(new CustomEvent('localless:result', { detail: text }));
  }

  // 粘贴落空时的正确反应是马上再补一枪，不是摆药丸——用户要的是字落进输入
  // 框，不是自己动手复制。补一次就够：第一枪落空多半是焦点/输入法插了一脚，
  // 补枪时那一下已经过去了；再补也救不回来的，多半是这个程序压根不吃这个
  // 组合键，试第三次只是让药丸多等两秒。
  const PASTE_ATTEMPTS = 2;

  // 发完键不等于对方已经收下。paste.ps1 一退出 runPaste 就 resolve，那只说明键
  // 发出去了；目标程序处理 WM_PASTE 还要一会儿——同一个文件里 paste-release 为此
  // 专门等了 250ms，偏偏校验这一读一直是发完就读。
  //
  // 读早了会正好落进 pasteLanded 唯一判失败的那一格：控件还是那个、内容看得见、
  // 就是没有我们的字——而真相只是「还没到」。于是补一枪，字落两遍。日志里这事
  // 分得干干净净：chrome 稳定 ~1700ms（跑满两轮），claude ~870ms（一轮），
  // 差的正好是一整轮，慢的那个程序每次都中。
  //
  // 所以「没落地」必须等出来：先给对方一点时间，读到就收工；读不到再等一次、
  // 再读一次，两次都没有才认定真没落地。多花的时间只落在慢目标身上，而补错
  // 一枪的代价是用户得自己去删掉重复的一整段话——这笔账不用算就知道该怎么选。
  const PASTE_SETTLE_MS = 250;
  const PASTE_CONFIRMS = 2;
  // 最后一次校验读到的插入点情况，只给日志用：null=这一读什么都没拿到，
  // true=拿到了插入点前面那段文字，false=控件不给（SupportedTextSelection=None）。
  // 判定落没落地和药丸出不出来都不看它，它只负责让下次复现能一眼看出是哪条证据
  // 在说话——这条链路的每个故障都只有事后读日志一条路。
  let lastCaretSeen = null;
  async function pasteSettled(text, before) {
    // pasteLanded 的那几条放行分支（读不到、控件不暴露文本、焦点换了地方）第一次
    // 就返回真，不会在这儿空等——只有真正可判的那一格才值得等第二眼。
    for (let k = 0; k < PASTE_CONFIRMS; k++) {
      await new Promise(r => setTimeout(r, PASTE_SETTLE_MS));
      let after = null;
      try { after = await invoke('read_focused'); } catch {}
      lastCaretSeen = after && typeof after.caret === 'string' ? after.caret.length > 0 : null;
      if (pasteLanded(text, before, after)) return true;
    }
    return false;
  }

  // 投递去哪儿，只看**这一刻**光标在哪儿。
  //
  // 以前这里判的是开录那一刻的快照，于是有两种拧巴：开录时光标不在输入框、
  // 中途点进去了，照样只给药丸；开录时在 A 的输入框、说完换到 B 的输入框，
  // 也只给药丸。用户的心智模型比这简单——文字出来的时候光标在哪儿就写哪儿。
  //
  // 只有实时读数缺席时才退回开录快照。UIA 会沉默（Chromium 的无障碍树要等第一次
  // 请求之后才建起来），沉默不该让正常听写整个退化成药丸。
  async function pasteOrPill(job, text) {
    let now = null;
    try { now = await invoke('read_focused'); } catch {}
    const known = now && Object.keys(now).length > 0;
    const writable = known ? targetWritable({ context: { focused: now } })
                           : targetWritable(job);
    // ── 投递观测线 ──────────────────────────────────────────────────────────
    // 「没点进任何输入框，说完却什么也没跳出来」只可能出在这个岔路口上，而且两边
    // 都会静悄悄地吞掉文字：判成可写就去粘贴，粘贴键落进一个不收字的地方，
    // pasteLanded 在看不见对方内容时又一律算成功，于是字既没进输入框也没上药丸。
    // targetWritable 是故意偏向"可写"的（沉默不算只读，否则浏览器里第一次听写就
    // 蹦药丸），所以判错的方向天然就是这一边。把判据原样记下来：下次复现直接读
    // 日志就知道是哪个 controlType 骗过了它，不用再猜。
    const f = known ? now : ((job && job.context && job.context.focused) || null);
    dictLog('deliver ' + job.id.slice(0, 8)
      + ' 焦点=' + (known ? '现读' : (f ? '开录快照' : '读不到'))
      + '[' + (f ? ((f.processName || '?') + ' ' + (f.controlType || '?')
                    + ' editable=' + f.editable + ' hasText=' + f.hasText
                    + ' hasValue=' + f.hasValue + ' 只读=' + f.valueReadOnly
                    + ' 可聚焦=' + f.keyboardFocusable)
                 : '无') + ']'
      + ' 判定=' + (writable ? '可写→粘贴' : '不可写→药丸'));
    // 光标不在可写输入框里：只摆药丸，绝不发粘贴键——那一下粘贴无处可去，
    // 还可能被对方程序当成快捷键。药丸永远不丢字。
    if (!writable) { showResultPill(text, job); return; }
    draw({ t: 'done', s: '' });
    const before = known ? now : null;
    let landed = false, shots = 0;
    lastCaretSeen = null;
    try {
      for (let i = 0; i < PASTE_ATTEMPTS && !landed; i++) {
        try {
          if (i === 0) await invoke('paste_begin', { text, noHistory: clipboardAutoClean(job) });
          else await invoke('paste_again');
        } catch { continue; }
        shots++;
        landed = await pasteSettled(text, before);
      }
    } finally {
      // 剪贴板是借的：成没成都得还。不还就等于每听写一次顶掉用户一次复制。
      invoke('paste_release', { hide: clipboardAutoClean(job) }).catch(() => {});
    }
    // 补枪也没落地才认输。这里不动剪贴板——上面那句无论如何都会还，
    // 药丸上本来就有复制按钮，没必要为了兜底再往剪贴板里塞一份。
    // landed=是 才是真正需要怀疑的那一行：pasteLanded 拿不到正面反证时一律算成功，
    // 所以"落地"既可能是真落进了输入框，也可能是字掉进了一个看不见的地方。
    dictLog('deliver ' + job.id.slice(0, 8)
      + ' 粘贴=' + (landed ? '判定落地（可能是查不出反证）' : '落空→药丸')
      + ' 发了' + shots + '枪'
      + ' 插入点=' + (lastCaretSeen === null ? '没读到' : (lastCaretSeen ? '读到了' : '空')));
    if (!landed) showResultPill(text, job);
  }

  // 粘贴这条链路本来是开环的：SendInput 的返回值没人看，powershell 退出也不
  // 等于按键落进了目标控件。从"决定粘贴"到"按键真的落下"中间有近两秒，焦点
  // 在这段时间里跑掉、或者对方程序把这个组合键自己吃了，用户就什么都拿不到
  // ——药丸早收了，文字只剩在剪贴板里，得手动贴回去。所以粘完再读一次目标
  // 控件，眼见为实。
  //
  // 判定一律偏向"算成功"：只有拿到**正面证据说明没落地**才报失败。读不到、
  // 控件根本不暴露自己的文本、焦点已经换了地方，统统放过。宁可漏掉一次药丸，
  // 也不能每次粘贴成功之后还多蹦一个药丸出来——那是上一版刚修掉的毛病。
  //
  // 「证据」现在有两手。先是插入点前面那一小段（uia-helper 的 caret 字段）：
  // 粘贴成功的话，刚落下的字必然紧挨在光标前面，这条不受下面那个 10000 字截断
  // 的影响。拿不到插入点时才退回比正文，外加一条"粘完和粘前一个字都没变"。
  //
  // 少了这两手就会漏掉一整类故障：最后一刻把光标移出输入框，粘贴键落进浏览器
  // 的页面正文——正文长过 10000 字就直接走了"读不全→算成功"那条路，于是字既没
  // 进输入框、也没上药丸，用户那头是彻底的无事发生。
  function pasteLanded(text, before, after) {
    if (!after || Object.keys(after).length === 0) return true;
    // 两个 pattern 都没有 = 自绘控件、终端、canvas 编辑器，它的内容我们看不见。
    if (after.hasValue !== true && after.hasText !== true) return true;
    // 焦点已经不在刚才那个控件上了：这一读说的是别人的事，不能拿来定罪。
    const sameControl = !before
      || (after.processId === before.processId
          && (after.automationId || '') === (before.automationId || '')
          && (after.controlType || '') === (before.controlType || ''));
    if (!sameControl) return true;
    // 换行在剪贴板和控件之间会在 LF / CRLF 之间来回转，空格也可能被对方规整
    // 掉，逐字比对必然误报。去掉所有空白再比。只比一小段：整段比对经不起对方
    // 程序的任何加工（自动补全、标点规范化、maxlength 截断），一处不同就误报
    // 成失败；十来个字已经足够独特了。
    const flat = s => (typeof s === 'string' ? s : '').replace(/\s+/g, '');
    const whole = flat(text);
    if (!whole) return true;
    const head = whole.slice(0, 12);
    // 插入点那一段比的是**末尾**：粘贴成功的话，光标就停在最后一个字后面，而
    // 开头那十来个字可能远在 400 字的取样窗口之外（一段长听写足够跑出去）。
    const tail = whole.slice(-12);
    const body = typeof after.text === 'string' ? after.text : '';
    const caret = typeof after.caret === 'string' ? after.caret : '';

    // ── 正面证据 ──
    if (flat(caret).includes(tail) || flat(body).includes(head)) return true;

    // ── 反面证据 ①：插入点 ──
    // 这是最贴身的一手材料，而且不受 DocumentRange 只取前 10000 字的影响。
    // 拿到了、却没有我们的字，那就是真没落地——最典型的是光标在最后一刻被移出
    // 输入框：粘贴键落进浏览器的页面正文，一个字都没进去。
    if (caret) return false;

    // ── 反面证据 ②：整段正文一个字都没变 ──
    // 长文档里 needle 可能落在截断之外，但"粘完和粘前完全一样"本身就是证据。
    const was = before && typeof before.text === 'string' ? flat(before.text) : null;
    if (was !== null && was === flat(body)) return false;

    // uia-helper 的 TextPattern 分支只取前 10000 字。长文档里粘在末尾的内容
    // 压根不在这一段里，读不到不代表没粘上。
    if (body.length >= 9000) return true;
    return false;
  }

  // UIA 说"不可写"的证据分两种：明确的（ValuePattern 自称只读、控件类型就
  // 不是文本控件）和沉默的（两个 pattern 都拿不到）。沉默不能当证据——
  // Chromium 的无障碍树要等第一次 UIA 请求之后才建起来，首次听写进浏览器/
  // Electron 应用时读到的往往是个光秃秃的顶层窗口，判成不可写就会"明明光标
  // 在输入框里，却蹦出药丸"。所以只在拿到明确证据时才拒写，其余一律照旧。
  const NON_TEXT_CONTROLS = new Set([
    'Button','CheckBox','RadioButton','MenuItem','MenuBar','Menu','Tab','TabItem',
    'List','ListItem','Tree','TreeItem','Slider','ProgressBar','ScrollBar','Image',
    'Hyperlink','Separator','StatusBar','ToolBar','TitleBar','Table','DataGrid',
    'DataItem','Header','HeaderItem','Thumb','SplitButton','Calendar','Group',
    'Window','Pane',
  ]);
  function targetWritable(job) {
    const f = job && job.context && job.context.focused;
    // 读不到焦点信息时按"可写"处理：保持原有行为，不因为读失败就退化。
    if (!f) return true;
    // 先认可写的证据，再认只读的。顺序反过来就会重演这个 bug：Chromium/WebView2
    // 的根 Document 即使页面里全是可编辑输入框，也照样报 ValuePattern
    // IsReadOnly=true（实测 msedgewebview2 返回 editable:true + valueReadOnly:true）。
    // 拿 valueReadOnly 先判就会把整个浏览器系的输入框全判成不可写。
    if (f.editable) return true;
    if (f.hasText === true && f.keyboardFocusable !== false) return true;
    // 走到这里才说明没有任何可写迹象，只读标记这时才算数。
    if (f.valueReadOnly === true) return false;
    // 桌面、按钮、列表项这些是真的没处可写；Custom/Edit/Document/未知一律放行。
    if (f.controlType && NON_TEXT_CONTROLS.has(f.controlType)) return false;
    return true;
  }

  function draw(state) {
    const detail = { ...state };
    window.dispatchEvent(new CustomEvent('localless:ui', { detail }));
    // 归一化后的副本喂给悬浮麦克风那颗图标。失败也不能影响药丸本身——
    // 这只是个状态镜像，不在听写的关键路径上。
    invoke('recorder_state', { detail }).catch(() => {});
  }

  // 「能不能存音频」这个状态位分散在几个调用点上，各自裸 draw 会把它抹掉——
  // 比如 loading 结束回到等待态时，把已经冒出来的存音频按钮又收回去。统一从这里出。
  function drawRefine() {
    draw({ t: 'refine', save: !!(saveOffer && jobs.has(saveOffer)) });
  }

  async function readContext() {
    // Electron 那边 getContext 是同步的 contextBridge 调用，这边只能 await。
    // 两个请求互不依赖，并发发出去省掉一个来回。
    const [base, focused] = await Promise.all([
      invoke('get_context').catch(() => ({})),
      // 输入框已有文字现在是提示词里可自由插入的 {{focused}}，永远读一次
      //（密码框由 native 侧挡掉）
      invoke('read_focused').catch(() => ({})),
    ]);
    return { settings: {}, history: [], corrections: [], ...base, focused: focused || {} };
  }

  function stopWatch() { clearInterval(learnTimer); learnTimer = null; pendingEdit = null; }

  // 提交一次学习：写入 corrections 表，并让引擎判断是否值得存成词汇。
  function commitLearning(changed) {
    if (!pendingEdit) return;
    const { id, text: pasted } = pendingEdit;
    invoke('save_correction', { id, wrong: pasted, right: changed }).catch(() => {});
    if (currentContext.settings.learnNamesFromEdits && ws && wsReady)
      ws.send(JSON.stringify({type:'review_learning',request_id:'learn-'+Date.now(),history_id:id,wrong:pasted,right:changed}));
    stopWatch();
  }

  // 「改完之后的文字，还认得出我们粘进去的那段吗」。看相邻两字组成的二元组重合了
  // 多少，按短的那边算。单字重合不行：两句毫不相干的英文随手就能凑出六七成相同字母。
  function looksEdited(pasted, changed) {
    const grams = s => {
      const a = [...String(s).replace(/\s+/g, '')], g = new Set();
      for (let i = 0; i + 1 < a.length; i++) g.add(a[i] + a[i + 1]);
      return g;
    };
    const A = grams(pasted), B = grams(changed);
    if (!A.size || !B.size) return true;   // 短到没有二元组，判不了，放行
    let hit = 0;
    for (const g of B) if (A.has(g)) hit++;
    return hit / Math.min(A.size, B.size) >= 0.3;
  }

  async function checkPendingEdit() {
    if (!pendingEdit) return;
    if (Date.now() - pendingEdit.at > 5 * 60 * 1000) {
      // 超时：把最后观察到的修改结算掉，别白丢；然后一定要停掉轮询。
      if (pendingEdit.lastChanged) commitLearning(pendingEdit.lastChanged); else stopWatch();
      return;
    }
    let focused = {};
    try { focused = (await invoke('read_focused')) || {}; } catch {}
    const sameField = focused.processId === pendingEdit.focused.processId
                   && focused.automationId === pendingEdit.focused.automationId;
    const after = String(focused.text || ''), before = pendingEdit.before, pasted = pendingEdit.text;
    // 用户按回车发送 / 切走焦点后输入框就空了——这时最后一次快照就是最终版本，必须立刻结算，
    // 否则轮询只会读到空串，学习永远不会发生（历史上 59 次听写、0 条 corrections 就是这么来的）。
    if (!sameField || !after) { if (pendingEdit.lastChanged) commitLearning(pendingEdit.lastChanged); return; }
    // 粘贴后输入框应为 before + pasted（可能替换了选区，无法精确定位时用全文差分）。
    // 取 before/after 的最长公共前后缀，中间就是用户最终保留/修改的文本。
    let pre = 0; while (pre < before.length && pre < after.length && before[pre] === after[pre]) pre++;
    let post = 0; while (post < before.length - pre && post < after.length - pre && before[before.length-1-post] === after[after.length-1-post]) post++;
    const changed = after.slice(pre, after.length - post).trim();
    if (!changed || changed === pasted || changed.length > Math.max(200, pasted.length * 1.6)) {
      pendingEdit.lastChanged = null; pendingEdit.stable = 0; return;
    }
    // 上面那条「输入框空了就结算」拦不住会显示占位文字的输入框：Claude Code 空着时
    // 是 "Type / for commands"，UIA 把占位读成字段内容，after 就永远不为空。一按回车，
    // 占位文字便成了「用户的最终版本」，被学进 corrections，再当成改写范例喂回给识别。
    // 所以判据不是「空不空」，是「我们写进去的那段还认得出来吗」：认不出来就不是修改，
    // 是这一格被清掉换了别的东西——按发送处理，结算之前记下的那一版。
    if (!looksEdited(pasted, changed)) {
      if (pendingEdit.lastChanged) commitLearning(pendingEdit.lastChanged);
      else { pendingEdit.stable = 0; }
      return;
    }
    // 输入法未上屏的拼音（"zhi'xing"）也会被 UIA 读成字段内容。撇号连接的纯小写串
    // 就是拼音分隔符，直接丢弃——真正的专名不长这样。
    if (/^[a-z]+('[a-z]+)+$/.test(changed)) { pendingEdit.stable = 0; return; }
    // 连续三次读到同一个值才算"改完了"（≈6s 静止），2 次不够：用户在候选框上挑字
    // 停 4 秒就会把半成品拼音学进词库。
    pendingEdit.stable = changed === pendingEdit.lastChanged ? (pendingEdit.stable || 0) + 1 : 0;
    pendingEdit.lastChanged = changed;
    if (pendingEdit.stable >= 2) commitLearning(changed);
  }

  function watchPendingEdit(item) {
    clearInterval(learnTimer);
    pendingEdit = item;
    // 粘贴完成后继续读取：2 秒一次，最多 5 分钟；改动稳定或输入框清空即结算并停止。
    learnTimer = setInterval(checkPendingEdit, 2000);
    setTimeout(checkPendingEdit, 1200);
  }

  // 结果落进输入框之后就开始盯着它：用户手改的部分才是要学的词。
  // before 用录音开始时读到的那份快照——那时候输入框里还没有我们写的东西。
  function armLearning(job, finalText) {
    if (!finalText || !currentContext.settings.learnNamesFromEdits) return;
    const f = job.context && job.context.focused;
    if (!f || !f.processId) return;
    watchPendingEdit({ id: job.id, text: finalText, before: String(f.text || ''),
                       focused: f, at: Date.now() });
  }

  function learnPending(focused) {
    // 开始下一次录音时再补查一次；主要流程由上面的延迟轮询完成。
    if (pendingEdit) checkPendingEdit();
  }

  function sendStart(rec) {
    if (!rec || rec.started || !ws || !wsReady) return false;
    const context = rec.context;
    try {
      ws.send(JSON.stringify({ type: 'start_audio', audio_id: rec.id, mode: 'transcript',
        history: context.history, corrections: context.corrections,
        focused_text: String(context.focused?.text || '').slice(-500),
        target_app: String(context.focused?.processName || ''),
        audio_metadata: { audio_format: 'wav', audio_sample_rate: SR, audio_channels: 1 } }));
      rec.started = true;
      return true;
    } catch {
      return false;
    }
  }

  // 托盘要显示模型此刻在显存还是内存，而主进程根本不连引擎——只能靠这条常驻
  // ws 定时问一次再转给它。4 秒够跟上自动升降舱（引擎那边采样本身就是秒级），
  // 又不至于把引擎的消息循环搅得太勤。
  const DEVICE_POLL_MS = 4000;
  // 余量抖动多大才值得重建一次托盘菜单。
  const VRAM_BUCKET_MB = 256;
  const bucketVramMb = (v) => (typeof v === 'number' ? Math.round(v / VRAM_BUCKET_MB) : v);
  let devicePollTimer = null, lastDeviceKey = null;
  function stopDevicePoll() {
    // 重连时必须清掉：不清的话每断一次就多叠一个定时器，问的次数越来越密。
    if (devicePollTimer) { clearInterval(devicePollTimer); devicePollTimer = null; }
  }

  function ensureWs() {
    if (ws && (ws.readyState === 0 || ws.readyState === 1)) return;
    ws = new WebSocket('ws://127.0.0.1:8765/ws/rt_voice_flow');
    ws.binaryType = 'arraybuffer';
    ws.onopen = () => {
      wsReady = true; sendStart(activeRecording);
      stopDevicePoll();
      const poll = () => {
        // readyState 要现查。ws 可能已经在关闭中，这时候 send 会抛。
        if (!ws || ws.readyState !== 1) return;
        try { ws.send(JSON.stringify({ type: 'ping' })); } catch {}
      };
      poll();                                   // 别让托盘等满一个周期才有值
      devicePollTimer = setInterval(poll, DEVICE_POLL_MS);
    };
    ws.onclose = () => {
      wsReady = false; ws = null;
      stopDevicePoll();
      // 引擎断了就把托盘上的档位显示作废——留着上一次的值会让用户以为还在跑。
      lastDeviceKey = null;
      invoke('device_state', { device: null }).catch(() => {});
      // 引擎断了就问不出编码会话了。这里必须显式作废：不发的话主进程只能等 TTL 过期，
      // 那段时间里它拿着旧读数，正好是引擎重启、用户最需要图标的时候。
      invoke('encoder_pids', { pids: null }).catch(() => {});
      for (const job of jobs.values()) {
        clearJobTimers(job);
        job.status = 'error';
        keepAudio(job.id, job.chunks, '引擎重启');
      }
      if (jobs.size) draw({ t: 'done', s: '引擎已重启' });
      jobs.clear();
      setTimeout(ensureWs, 1500);
    };
    ws.onerror = () => { wsReady = false; };
    ws.onmessage = (ev) => {
      try {
        const m = JSON.parse(ev.data);
        // 调试钩子：渲染进程 console 里随时能看最后一条 ws 消息
        window.__llLastWs = { t: m.type, at: Date.now() };
        if (m.type === 'pong' && m.device) {
          // 只在真的变了才发。托盘菜单改一次就得整份重建，4 秒一次白重建没必要。
          // 空闲显存是个一直在动的数，原样进 key 就等于每 4 秒重建一次菜单——
          // 菜单正开着的时候重建，在 Windows 上会把它闪掉。所以 key 里按 256 MB
          // 分桶：档位这类状态一变立刻推，余量只在真挪了一档时才推。
          // 注意分桶只用于比较，**发出去的还是原值**，托盘显示的得是真数。
          const key = JSON.stringify({ ...m.device, freeMb: bucketVramMb(m.device.freeMb) });
          if (key !== lastDeviceKey) {
            lastDeviceKey = key;
            invoke('device_state', { device: m.device }).catch(() => {});
          }
          // 编码会话是另一路信号（喂悬浮麦克风的远程探测），和档位无关，所以单独发。
          // **故意不去重**：档位去重是因为改一次就要重建整份托盘菜单，而这个只喂一个
          // 布尔值，重算是免费的；反过来，主进程要靠"每 4 秒都收到一次"来判断读数
          // 有多新，去重就等于把心跳掐了——引擎挂掉后它会抱着最后一份读数一直当真。
          // undefined（旧引擎没这个字段）要归一成 null = 问不出来，不能当成空数组。
          invoke('encoder_pids', { pids: m.encoders === undefined ? null : m.encoders }).catch(() => {});
          return;
        }
        if (m.type === 'pipeline_update') {
          invoke('pipeline_update', { update: m }).catch(() => {});
          const job = jobs.get(m.audio_id);
          if (job) job.status = m.status === 'queued' ? 'queued' : m.stage;
          // 冷加载权重（首次听写、刚换过模型）会卡好几秒，明说在装模型，
          // 别让用户对着不动的药丸猜是不是死了。
          if (m.stage === 'loading') {
            if (m.status === 'running') draw({ t: 'loading' });
            else if (jobs.has(m.audio_id)) drawRefine();
          }
        }
        if (m.type === 'learning_reviewed' && m.add && m.term) {
          // addLearnedWord 在 Electron 那边是同步的，这边是个 Promise。整段包进
          // then 里，顺序和判据一字未动。
          invoke('add_learned_word', { term: m.term, tagName: m.tag }).then(added => {
            if (!added) return;
            invoke('note_learned', { historyId: m.history_id, term: m.term,
              tagName: added.tagName, wrong: m.wrong, right: m.right }).catch(() => {});
            window.dispatchEvent(new CustomEvent('localless:learned', {detail:{id:added.id,term:m.term,tagName:added.tagName,ms:5000}}));
          }).catch(() => {});
        }
        if (m.type === 'refine_completed') {
          if (cancelledIds.delete(m.audio_id)) return;
          const job = jobs.get(m.audio_id);
          if (!job) return;
          clearJobTimers(job);
          jobs.delete(m.audio_id);
          invoke('save_result', { id: m.audio_id, rawText: m.raw_text,
            refinedText: m.refined_text, duration: m.duration }).catch(() => {});
          // 三条投递路径（锚点替换 / 键盘写入 / 剪贴板粘贴）都要开始盯改动，
          // 否则自学习永远不会触发——watchPendingEdit 之前压根没人调用。
          armLearning(job, m.refined_text);
          // 这一行就是「有没有东西转写出来」的答案，和上面那两组电平并排读。
          dictDone(job.snap, m.refined_text ? ('转出 ' + m.refined_text.length + ' 字') : emptyWhy(job.snap, false));
          if (m.refined_text) {
            // 转成功的也留一份。以前只有失败路径存盘，录音文件夹里于是只剩
            // 事故现场；想回头听听某句当时到底是怎么说的，没有。
            // 理由留空：saveAudioOnly 撞上 saveResult 刚写下的那一行时只补
            // 路径和时长，不动 status 也不动 debug_info，这条仍然是 completed。
            keepAudio(job.id, job.chunks, '');
            // 可不可写交给 pasteOrPill 在这一刻现读现判，不看开录时的快照：
            // 录音时光标不在输入框、说完才点进去，也应该照样写进去。
            pasteOrPill(job, m.refined_text);
          } else {
            // 转不出字也是一次失败，凭什么它不留音频。超时、协议错误、引擎重启、
            // 手动中断都进了历史，唯独"说了话、一个字都没出来"什么都不剩——而这
            // 恰恰是眼下最常撞上的一种。存下来，历史页那条「重新转录」才有东西可跑，
            // 回放也才能听出当时到底是人没说清还是麦克风没收到。
            // 理由用长句那一版：历史行上显示的就是 debug_info，带着实测峰值。
            keepAudio(job.id, job.chunks, emptyWhy(job.snap, false));
            // 药丸上也得分开说。用户看到的那句话就是他下一步去查什么的唯一线索，
            // 说"没听清"会把他支去改识别设置，而真正该看的是麦克风。
            // 后缀短到 4 个字：.ll-text 是 nowrap+ellipsis，长了先被切掉的正是尾巴。
            draw({ t: 'done', s: emptyWhy(job.snap, true) + ' · 可重转' });
          }
        }
        if (m.type === 'audio_cancelled') {
          cancelledIds.delete(m.audio_id);
          const job = jobs.get(m.audio_id);
          if (job) {
            clearJobTimers(job);
            jobs.delete(m.audio_id);
          }
        }
        if (m.type === 'protocol_error') {
          const job = m.audio_id && jobs.get(m.audio_id);
          if (job) {
            clearJobTimers(job);
            jobs.delete(m.audio_id);
            keepAudio(job.id, job.chunks, '协议错误');
            dictDone(job.snap, '协议错误');
          }
          draw({ t: 'done', s: '协议错误: ' + (m.error || '未知') });
        }
        // 引擎重启/出错后可能回 error 而不是 refine_completed——同样要解锁，否则 Alt 永久失效
        if (m.type === 'error') {
          const job = m.audio_id && jobs.get(m.audio_id);
          if (job) {
            clearJobTimers(job);
            jobs.delete(m.audio_id);
            keepAudio(job.id, job.chunks, '引擎错误');
            dictDone(job.snap, '引擎错误');
          }
          if (activeRecording) { activeRecording = null; invoke('mute_set', { on: false }).catch(() => {}); }
          draw({ t: 'done', s: '引擎错误: ' + (m.message || m.error || '未知') });
        }
      } catch {}
    };
  }

  // 挑一支真的在出声的麦克风，别再问"默认是谁"。
  //
  // 这台机器上同时挂着三个采集设备：PD100X（桌上的实体麦）、UU远程虚拟音频设备
  // （远程会话把 iPad 的麦克风送过来）、Virtual Desktop Audio（没人推流时恒送数字
  // 零）。谁当"系统默认"由这些虚拟驱动轮流抢，实测二十分钟内就换了一次：22:06 默认
  // 还是 PD100X，22:24 已经变成 Virtual Desktop Audio。所以"默认设备"根本不是"用户
  // 的声音从哪来"的答案——把两条流绑到同一个默认设备上，只是让它们一起错。
  //
  // 能问出真话的是另一个问题：这支设备此刻在送采样吗。真麦克风永远带底噪，实测
  // 最轻的一次（人在 iPad 那头、桌上那支隔着一个房间）整段峰值也有 0.0017，不是零；
  // 而没人推流的虚拟设备送的是一串严格的数字零（Virtual Desktop Audio 实测整段
  // rms 0.0000~0.0000）。这一问不需要用户出声就能答，所以开录时问得起。
  //
  // 三个开关必须写死成关，不能省着不写——不写等于用 Chromium 的默认值，而它默认全开。
  // 覆盖层的波形轨和设置页的预览轨一直是关着的，只有录音轨吃默认值，于是「波在跳、
  // 设置里测试也听得见、偏偏转不出字」三件事同时成立：你看见和听见的是生信号，送进
  // 模型的那一条被处理过。
  //
  // 本机 USB 麦克风开着三件套照样转得出字（PD100X 实测多次），所以它们不是天生有毒；
  // 出事的是远程：AEC 要靠采集和播放共用一个时钟、延迟稳定才收敛得了，而远程虚拟音频
  // 设备的声音是从手机走网络灌进来的，独立时钟还在漂，AEC 拿一份对不齐的回声去减，
  // 能量减不掉（峰值 0.30~0.41、波形照跳）话却被减花了，模型 220ms 就吐结束符。
  //
  // 关掉不心疼：降噪、增益、限幅引擎侧自己全做了一遍（process/_noise_gate/normalize），
  // 留着 Chromium 那套只是让两边互相打架——AGC 正在跟用户自己调的 micGainDb 对着拉。
  const MIC_AUDIO = { channelCount: 1, sampleRate: SR, echoCancellation: false, noiseSuppression: false, autoGainControl: false };
  const MIC_PROBE_MS = 140;
  // 排头那支多给些时间。远程会话把手机/iPad 的麦克风送过来走的是虚拟音频设备，而它
  // 恰恰是远程时唯一能听见你的一支；虚拟设备未必在开流那一刻就开始送采样，140ms 的
  // 窗口有可能正好卡在它热身的当口，把唯一听得见你的那支判成哑的。具体要多久没测过，
  // 600ms 是留的余量——每一支探了多久、判成什么都会写进日志，下次不用再猜。
  const MIC_PROBE_FIRST_MS = 600;
  // 预热用的窗口。预热不在用户的等待路径上，等得起；真录时贴着边是没办法的事，
  // 人在等。日志实测远程虚拟音频设备接进来后要 532ms 才吐出第一个非零采样，
  // 600ms 正好贴边——预热时给足，免得把一支正在热身的设备记成哑的，等用户按键
  // 时它已经沉到候选队尾，顺位落到桌上那支几乎听不见人的实体麦。
  const MIC_PROBE_WARM_MS = 2500;
  // 刚被证实是哑的那几支。只记在内存里：设备的死活本来就跟着远程会话来去变，
  // 过一阵就该重新给它一次机会，而不是记一辈子。
  const deadMics = new Map();
  const MIC_DEAD_TTL_MS = 120000;
  function micDead(id) { const t = deadMics.get(id); return t != null && Date.now() - t < MIC_DEAD_TTL_MS; }

  // 悠悠远控（GameViewer）装的那支虚拟麦克风，完整名字形如
  // 「麦克风阵列 (UU远程虚拟音频设备)」。前半截会跟着主机的声卡变，认后半截。
  const UU_MIC = 'UU远程虚拟音频设备';
  function uuMic(real) {
    const d = real.find(x => (x.label || '').includes(UU_MIC));
    return d ? d.deviceId : null;
  }
  // 人在不在远程那头。主进程推过来的，跟悬浮麦克风图标是同一个判据（推流日志 +
  // 显卡上的 NVENC 会话），这里只是接住那个结论，不自己再判一次。
  let remoteSession = false;

  // 用户在设置里指名的那支。空着就是跟随系统默认。
  // 记 id 也记名字：远程虚拟音频设备每断开重连一次 id 就换一个，只认 id 的话，用户
  // 明明选好了，下一次远程连进来还是挑不中它，等于没选。
  function micPrefer(real, pref) {
    if (!pref || (!pref.id && !pref.label)) return null;
    const byId = pref.id && real.find(d => d.deviceId === pref.id);
    if (byId) return byId.deviceId;
    // 标签前面可能挂着 Chromium 加的 'Default - '（从占位条目或轨道上抄来的名字就带），
    // 比对前先剥掉，否则同一支设备的两种写法会被当成两支。
    const bare = s => String(s || '').replace(/^(Default|Communications)\s+-\s+/, '');
    const want = bare(pref.label);
    if (!want) return null;
    const hit = real.find(d => d.label && bare(d.label) === want);
    return hit ? hit.deviceId : null;
  }

  async function micCandidates() {
    const out = [];
    // shun：明知道不该用、但也不能剔除的那支（当下只有一种——不在远程时的 UU
    // 虚拟麦克风）。排到最后，全都哑着的时候它仍然兜得住底。
    let pref = null, shun = null;
    try {
      const ins = (await navigator.mediaDevices.enumerateDevices()).filter(d => d.kind === 'audioinput');
      const dflt = ins.find(d => d.deviceId === 'default');
      // 'default' 只是个占位条目，拿它去开流等于什么都没说；它靠 groupId 指向那条真
      // 设备，要换成真条目的 id 才算指名道姓。
      const real = ins.filter(d => d.deviceId && d.deviceId !== 'default' && d.deviceId !== 'communications');
      const defId = dflt ? (real.find(d => d.groupId && d.groupId === dflt.groupId) || {}).deviceId : null;
      // 排头永远是系统默认那支。换不出真 id 时压进 null——那是占位的"给我默认的"，
      // 不指名道姓但至少是对的那支（设置页那个麦克风测试就是这么开的，它一直好使）。
      // 绝不能因为换不出来就退化成"枚举顺序里的第一个"：按列表位置挑设备是瞎猜，
      // 实测就是这一步把人在远程那头、桌上那支几乎听不见他的 PD100X 挑了出来。
      //
      // 用户指名的那支排在系统默认前面。排头的位置是决定性的，因为 openLiveMic 探到
      // 活的就收工：任何一支通着电的实体麦克风都有底噪，一探就"活"，排在它后面的设备
      // 永远试不到。远程时人在手机那头，桌上那支 PD100X 每次都抢先过关，录下来自然
      // "没听清"——这就是波在跳却听不清的最后一层。探测答得出"这支哑不哑"，答不出
      // "你在对着哪支说话"；后面这一问只有用户自己答得了，所以设置里给了个选择框。
      try { pref = micPrefer(real, await invoke('mic_pref')); } catch {}
      // 自动挡（设置里选的是「自动」）在远程会话里要改口：该录的是 UU 那支虚拟
      // 麦克风，不是桌上这支实体麦克风——人在手机那头，桌上这支只收得到房间的
      // 空气声。当成「用户指名的那支」压进排头，顺带也免了被哑名单下沉：远程
      // 虚拟麦克风本来就是断断续续的，刚才哑过恰恰是它的常态。
      //
      // 反方向同样要管。UU 那支是常驻设备，不远程时它照样在列表里，而且**通着电
      // 却只送零采样**——轨道是 live 的，探测得等满一整轮才判得出哑。直接沉到
      // 队尾，省掉那一轮，也免得它正好是系统默认时抢在实体麦克风前面。
      //
      // 用户自己指名过设备就一概不插手：那是他的选择，自动挡才轮得到我们做主。
      const uu = uuMic(real);
      if (!pref && uu) { if (remoteSession) pref = uu; else shun = uu; }
      if (pref) out.push(pref);
      // 指名的那支正好就是系统默认时别推两遍，白探一轮。反过来，没指名时这一句必须照推
      // 不误——两边都是 null 的时候写成 defId !== pref，那个占位的"给我默认的"就没了。
      if (!(pref && defId === pref)) out.push(defId || null);
      for (const d of real) if (d.deviceId !== defId && d.deviceId !== pref) out.push(d.deviceId);
    } catch {}
    // 枚举不出来（没权限、接口不在）就退回一句"给我默认的"，不比改动前更糟。
    if (!out.length) return [null];
    // 哑过的排到最后，但绝不剔除：远程会话一接进来它随时会活过来；而且万一全都哑着，
    // 也总得有个东西可开——那时药丸会老实说"没录到声音"。
    // 用户指名的那支不参与下沉：远程虚拟麦克风本来就是断断续续的，刚才哑过恰恰是它的
    // 常态，要是因此被排到最后，用户选了等于没选。
    // shun 要判 shun 非空再比：空着时 shun===null，而 out 里那个占位的「给我
    // 默认的」也是 null，一个不小心就把它沉到底了。
    const sink = x => ((shun && x === shun) || (micDead(x) && x !== pref)) ? 1 : 0;
    return out.sort((a, b) => sink(a) - sink(b));
  }

  // 听 budget 毫秒，看有没有任何一个非零采样。活着的麦克风通常第一次取样就返回，
  // 所以正常情况下这里只花一次取样的工夫；只有真哑的设备才会等满。
  async function micAlive(st, budget) {
    let node = null, gain = null;
    try {
      const an = ctx.createAnalyser(); an.fftSize = 1024;
      node = ctx.createMediaStreamSource(st);
      // 没接到 destination 的子图不保证会被驱动，取到的会是一片零——那和"设备是哑的"
      // 长得一模一样。串一个 gain=0 接出去，既驱动得起来又不会真出声。
      gain = ctx.createGain(); gain.gain.value = 0;
      node.connect(an); an.connect(gain); gain.connect(ctx.destination);
      const buf = new Float32Array(an.fftSize), deadline = performance.now() + budget;
      for (;;) {
        an.getFloatTimeDomainData(buf);
        for (let i = 0; i < buf.length; i++) if (buf[i] !== 0) return true;
        if (performance.now() >= deadline) return false;
        await new Promise(r => setTimeout(r, 20));
      }
    } catch {
      // 探不动就别拦着：把一支好麦克风判成哑的，比漏判一支哑的更糟。
      return true;
    } finally {
      try { if (node) node.disconnect(); } catch {}
      try { if (gain) gain.disconnect(); } catch {}
    }
  }

  // firstMs 是排头那支的探测窗口。参数化是为了让预热能给一份长的：同一套挑选
  // 逻辑，只有"愿意等多久"这一处不同，复制一份出来迟早会和真录的那份走散。
  async function openLiveMic(firstMs = MIC_PROBE_FIRST_MS, why = '挑选') {
    let fallback = null, chosen = null;
    // 挑麦克风这件事必须能在日志里复盘。"波在跳却没听清"查了两轮，两轮都卡在同一处：
    // 日志只写得出最后录的是哪支，写不出另外几支为什么被淘汰——于是每一次都只能猜。
    const seen = [];
    const cands = await micCandidates();
    for (let i = 0; i < cands.length; i++) {
      const id = cands[i];
      const tag = (i + 1) + ' ' + (id ? id.slice(0, 6) : '默认');
      let st = null;
      // exact 指定的设备可能在枚举和开流之间消失（远程断开正好制造这一幕），
      // 那就跳过它接着试下一支，别让整次听写以"麦克风失败"收场。
      try { st = await navigator.mediaDevices.getUserMedia({ audio: id ? Object.assign({ deviceId: { exact: id } }, MIC_AUDIO) : MIC_AUDIO }); }
      catch { seen.push('[' + tag + ' 开不出来]'); continue; }
      const tr = st.getAudioTracks()[0];
      const t0 = performance.now();
      const ok = await micAlive(st, i === 0 ? firstMs : MIC_PROBE_MS);
      seen.push('[' + tag + ' ' + ((tr && tr.label) || '无名') + ' 探=' + Math.round(performance.now() - t0) + 'ms '
                + (ok ? '活' : '哑') + ']');
      if (ok) { if (id) deadMics.delete(id); chosen = st; break; }
      if (id) deadMics.set(id, Date.now());
      // 留第一条当兜底：全都哑着的时候总得录下点什么，药丸才有依据说"没录到声音"。
      if (fallback) { try { st.getTracks().forEach(t => t.stop()); } catch {} } else fallback = st;
    }
    if (!chosen && fallback) { chosen = fallback; seen.push('全哑，退回第一条'); }
    if (!chosen) { chosen = await navigator.mediaDevices.getUserMedia({ audio: MIC_AUDIO }); seen.push('一条都没开成，退回默认'); }
    else if (fallback && fallback !== chosen) { try { fallback.getTracks().forEach(t => t.stop()); } catch {} }
    dictLog('mic ' + why + ' ' + seen.join(' ') + ' → 用 ['
      + ((chosen.getAudioTracks()[0] || {}).label || '无名') + ']');
    return chosen;
  }

  // 端口一换，第一次听写就贵。日志实测：切到远程后第一次 mic 挑选 探了 532ms
  // （另一次 613ms 判哑、顺位落到桌面麦），第二次起一律 41ms。贵出来的那半秒
  // 整个花在 startRec 的 await ensureMic() 上，而 chunks 要等流开出来才有接收者
  // ——按下键顺手说的那半句话不是被丢弃，是根本没有地方接。短句整段丢在里面。
  //
  // 两种失败形状同一个根子：挑麦被放在了按键路径上。挪走——拓扑一变就在后台先
  // 探一轮，把设备叫醒、把哑名单按新拓扑重写，等用户真按键时只剩稳态那 41ms。
  let micWarm = null;
  function warmMic(why) {
    // 已经有一轮在飞就跟着它，别并发开两条流去抢同一支设备。
    if (micWarm) return micWarm;
    micWarm = (async () => {
      try {
        if (!ctx) ctx = new AudioContext({ sampleRate: SR });
        if (ctx.state === 'suspended') { try { await ctx.resume(); } catch {} }
        const st = await openLiveMic(MIC_PROBE_WARM_MS, '预热(' + why + ')');
        // 预热只为把设备叫醒、把哑名单写对，流本身立刻还回去：留着它会一直占住
        // 设备，远程那头要用同一支虚拟麦克风时会被顶掉。
        try { st.getTracks().forEach(t => t.stop()); } catch {}
      } catch (e) {
        dictLog('mic 预热失败 ' + ((e && e.message) || e));
      } finally { micWarm = null; }
    })();
    return micWarm;
  }

  // 在这之前，全应用只有设置页挂过 devicechange，录音链路一个都没有——也就是说
  // 端口切换时没有任何东西去作废旧状态，那次失败的听写本身就是作废动作。
  let micTopoTimer = null;
  try {
    navigator.mediaDevices.addEventListener('devicechange', () => {
      // 一支设备的增删在 Windows 里不是原子的，一次切换会连发好几个事件；
      // 防抖到最后一个再动手，否则会开出好几轮预热互相抢设备。
      clearTimeout(micTopoTimer);
      micTopoTimer = setTimeout(() => {
        // 哑名单是按上一套拓扑记的，换了一套就一条都不算数——留着只会让排序继续
        // 错下去（TTL 有两分钟，够毁掉好几次听写）。
        deadMics.clear();
        // 正在录、正在起录时不开预热流：那是在跟正在录的那条抢同一支设备。
        if (activeRecording || startingRecording) return;
        warmMic('设备变化');
      }, 800);
    });
  } catch {}

  // 远程接入/断开。主进程在算图标显隐的同一个函数里发这条，共用同一个判据，
  // 所以「该换麦克风了」和「图标出现了」永远是同一刻，不会错开一拍。
  //
  // 收到就立刻重探一轮，不等下一次按键：哑名单是按上一套拓扑记的，换了一头说话
  // 的人就一条都不算数；而端口刚换那一次的挑选要花五六百毫秒，那半秒正好吃掉
  // 按下键顺口说的那半句话。
  //
  // Tauri 的事件回调收的是 `{payload}`，Electron 那边是剥过 event 的 `(on)`。
  // 两种写法都极容易把布尔值接错位置，接错的症状是判出来恒为「不远程」，于是
  // 每次都在第一行 return，整条链路静悄悄地什么都不做。
  //
  // 换口这件事有两个触发点（开机问一次、之后主进程推），动作是同一套，写一处。
  // 哑名单必须清：它是按上一套拓扑记的，换了一头说话的人就一条都不算数。
  function setRemote(on) {
    const next = !!on;
    if (next === remoteSession) return false;
    remoteSession = next;
    deadMics.clear();
    return true;
  }
  try {
    listen('localless://remote-session', (e) => {
      if (!setRemote(e && e.payload)) return;
      dictLog('mic 远程' + (remoteSession ? '接入' : '断开') + '，自动挡重挑设备');
      // 正在录、正在起录时不开预热流：那是在跟正在录的那条抢同一支设备。
      if (activeRecording || startingRecording) return;
      warmMic(remoteSession ? '接入远程' : '离开远程');
    });
    // 开机对齐。主进程那条推送带去重，推过一次就不再重发，而药丸起得比它晚——
    // 开机那一条多半发给了一个还没有听众的频道，判据不变就永远不会补第二次。
    // 这里不预热：药丸起来本来就要热一次，再热一次是在跟它抢同一支设备。
    invoke('remote_mic_now').then(on => {
      if (!setRemote(on)) return;
      dictLog('mic 远程开机对齐：' + (remoteSession ? '在远程那头' : '在本机'));
    }).catch(() => {});
  } catch (err) {
    dictLog('mic 远程判定接不上：' + err);
  }

  // 每次录音都现开一条新流，绝不复用上一轮的。
  //
  // 旧写法是开一次缓存到进程结束，只要轨道还 readyState==='live' 就接着用。这个
  // 守卫问的是"轨道还活着吗"，问不出"轨道还出声吗"——而这两件事是能分开的：远程
  // 会话把虚拟麦克风顶成系统默认设备，流就在那一刻开好并缓存下来；会话结束后虚拟
  // 设备在 Windows 里依然 ACTIVE、轨道依然 live，只是送出来的采样全是零。
  //
  // AudioContext 和 ScriptProcessor 留着复用，只换流和接在流上的那个源节点。
  async function ensureMic() {
    if (!ctx) ctx = new AudioContext({ sampleRate: SR });
    // 自动播放策略会让 AudioContext 停在 suspended，停着就一个采样都不会进来。
    // 这一句必须排在探测之前：停着的 ctx 取到的全是零，会把每一支麦克风都判成哑的。
    if (ctx.state === 'suspended') { try { await ctx.resume(); } catch {} }
    // 预热正在飞就等它跑完再开。两条 getUserMedia 并发去开同一支虚拟麦克风，被抢
    // 的那一条会直接开不出来；等一下比抢一下便宜——而且预热跑完，这里的探测就是
    // 热的那 41ms，正是整件事要的东西。
    const warming = micWarm;
    if (warming) { try { await warming; } catch {} }
    // 先开新的再停旧的：反过来的话，开流失败就把上一条能用的流也搭进去了，而这里
    // 抛出去的异常上游是当成"麦克风失败"收场的，不该顺手弄坏下一次重试。
    const fresh = await openLiveMic();
    if (stream) { try { stream.getTracks().forEach(t => t.stop()); } catch {} }
    stream = fresh;
    // 公布录音实际拿到的那支，药丸的电平流照着它开。给的是"实际拿到的"而不是"我想要
    // 的"：真正要对齐的是被录下来的那条音频，中间但凡退过一次路，也得跟着退。
    const tr0 = fresh.getAudioTracks()[0];
    window.__llMicDeviceId = (tr0 && tr0.getSettings ? tr0.getSettings().deviceId : null) || null;
    if (src) { try { src.disconnect(); } catch {} }
    src = ctx.createMediaStreamSource(stream);
    if (!proc) {
      proc = ctx.createScriptProcessor(4096, 1, 1);
      proc.onaudioprocess = e => {
        const rec = activeRecording;
        if (!rec) return;
        rec.chunks.push(new Float32Array(e.inputBuffer.getChannelData(0)));
      };
      proc.connect(ctx.destination);
    }
    src.connect(proc);
    micReady = true;
  }

  function wavHeader(total) {
    const h = new ArrayBuffer(44), dv = new DataView(h);
    const s = (o, str) => { for (let i = 0; i < str.length; i++) dv.setUint8(o + i, str.charCodeAt(i)); };
    s(0,'RIFF'); dv.setUint32(4, 36+total*4, true); s(8,'WAVE'); s(12,'fmt ');
    dv.setUint32(16,16,true); dv.setUint16(20,3,true); dv.setUint16(22,1,true);
    dv.setUint32(24,SR,true); dv.setUint32(28,SR*4,true); dv.setUint16(32,4,true); dv.setUint16(34,32,true);
    s(36,'data'); dv.setUint32(40,total*4,true);
    return h;
  }

  // 存档用的 WAV 跟发给引擎的那份不是一回事：上面 wavHeader 出的是 float32
  // （fmt tag 3），引擎认，但 Chromium 的 <audio> 解不了，历史页里点播放会是
  // 一片死寂。存档一律转成 16 位整数 PCM——浏览器和引擎都吃这个。
  function wavPcm16(x) {
    const n = x.length, buf = new ArrayBuffer(44 + n * 2), dv = new DataView(buf);
    const s = (o, str) => { for (let i = 0; i < str.length; i++) dv.setUint8(o + i, str.charCodeAt(i)); };
    s(0,'RIFF'); dv.setUint32(4, 36+n*2, true); s(8,'WAVE'); s(12,'fmt ');
    dv.setUint32(16,16,true); dv.setUint16(20,1,true); dv.setUint16(22,1,true);
    dv.setUint32(24,SR,true); dv.setUint32(28,SR*2,true); dv.setUint16(32,2,true); dv.setUint16(34,16,true);
    s(36,'data'); dv.setUint32(40,n*2,true);
    for (let i = 0; i < n; i++) {
      const v = Math.max(-1, Math.min(1, x[i]));
      dv.setInt16(44 + i*2, v < 0 ? v*0x8000 : v*0x7fff, true);
    }
    return buf;
  }

  // 转写没走完就把音频落盘，历史里留一条"有声音、没文字"的记录，之后能重转。
  // 这段 PCM 原本只活在内存里：2026-09-05 一条 161.8 秒的听写就是这么没的——
  // 引擎那边 60 秒的硬上限先到，客户端发完 cancel_audio，chunks 跟着清空，从
  // 日志到数据库一点痕迹都没留，用户说了两分半话，什么都没剩下。
  // 存整段 chunks 而不是 stopRec 里那个 tail：tail 只是尾巴，拿它当存档
  // 等于只存了最后一小截。
  //
  // body 的格式见文件头第 3 条：[u32 LE 元数据长度][元数据 JSON][wav]。
  function keepAudio(id, chunks, why) {
    try {
      const n = chunks && chunks.reduce((s, c) => s + c.length, 0);
      if (!n) return;
      const x = new Float32Array(n);
      let o = 0; for (const c of chunks) { x.set(c, o); o += c.length; }
      const wav = new Uint8Array(wavPcm16(x));
      const meta = new TextEncoder().encode(JSON.stringify({ id, duration: n / SR, reason: why || '' }));
      const body = new Uint8Array(4 + meta.length + wav.length);
      new DataView(body.buffer).setUint32(0, meta.length, true);
      body.set(meta, 4);
      body.set(wav, 4 + meta.length);
      invoke('save_audio_only', body).catch(e => console.warn('[localless] keepAudio failed:', e));
    } catch (e) { console.warn('[localless] keepAudio failed:', e); }
  }

  function encodeAudioFrame(id, payload) {
    const aid = new TextEncoder().encode(id);
    if (!aid.length || aid.length > 255) throw new Error('invalid audio_id');
    const body = new Uint8Array(payload), frame = new Uint8Array(6 + aid.length + body.length);
    frame.set([76,76,65,70,1,aid.length], 0);
    frame.set(aid, 6);
    frame.set(body, 6 + aid.length);
    return frame.buffer;
  }

  async function startRec() {
    if (activeRecording || startingRecording) return;
    // 第一次 await 之前就占住启动状态。远程触控很容易产生双击；若不先上锁，
    // 两次调用会同时申请麦克风并创建两段录音。启动期间再次切换则取消本次启动。
    const attempt = { cancelled: false };
    startingRecording = attempt;
    ensureWs();
    let context = currentContext;
    try {
      currentContext = await readContext();
      context = currentContext;
      learnPending(context.focused);
    } catch (e) {
      console.error('[localless] readContext failed — 历史/输入框/自学习将为空:', e);
    }
    if (attempt.cancelled || startingRecording !== attempt) {
      if (startingRecording === attempt) startingRecording = null;
      return;
    }
    try {
      await ensureMic();
    } catch (e) {
      if (startingRecording === attempt) startingRecording = null;
      if (!attempt.cancelled) draw({ t: 'ready', s: '麦克风失败: ' + e.message });
      return;
    }
    if (attempt.cancelled || startingRecording !== attempt) {
      if (startingRecording === attempt) startingRecording = null;
      return;
    }
    const id = 'rec-' + (globalThis.crypto?.randomUUID?.() || (Date.now() + '-' + Math.random().toString(36).slice(2,8)));
    const rec = { id, chunks: [], context, cancelled: false, started: false };
    activeRecording = rec;
    startingRecording = null;
    if (context.settings.muteWhileRecording) invoke('mute_set', { on: true }).catch(() => {});
    draw({ t: 'rec', s: '' });
    sendStart(rec);
    dictLog('start ' + id.slice(0, 8));
  }

  // ── 听写观测线 ──────────────────────────────────────────────────────────
  // 一次听写在日志里留两行：start 和 done。只有 start 没有 done，就是"确认那一
  // 下压根没走到收尾"；两行都在，done 里的数说明这一轮到底卡在哪。
  //
  // 关键是并排放的这两组电平。药丸的柱子由 pill.html 每次录音现开一条新流驱动，
  // 而真正录下来的音频走的是 ensureMic() 那条。两条流现在绑同一个设备 id，本该
  // 永远一致；这两组数并排留着，就是为了在它们再次分家时当场看见。
  // 两条流唯一一处系统性的差别：以前 ensureMic() 用的是 Chromium 默认约束，
  // 回声消除/降噪/自动增益三样全开；波形那条把三样全关。远程会话里扬声器
  // 一直在放声音，AEC 拿它当参考往下减，减过头就会把整段人声抹成零——而关掉
  // 这三样的波形流照样听得见。现在两条都写死成关（见 MIC_AUDIO），真正生效的
  // 处理档位仍然记下来：下次再分家，日志里当场看得见。
  function procTag(s) {
    if (!s) return '?';
    return [s.echoCancellation && 'AEC', s.noiseSuppression && 'NS', s.autoGainControl && 'AGC']
      .filter(Boolean).join('+') || '无';
  }
  function snapDict(rec) {
    let peak = 0, sum = 0, n = 0;
    for (const c of rec.chunks) {
      for (let i = 0; i < c.length; i++) {
        const v = c[i], a = v < 0 ? -v : v;
        if (a > peak) peak = a;
        sum += v * v; n++;
      }
    }
    const t = stream && stream.getAudioTracks()[0];
    const st = t && t.getSettings ? t.getSettings() : null;
    const L = window.__llLevelStats || {};
    return { id: rec.id.slice(0, 8), sec: n / SR, peak, rms: n ? Math.sqrt(sum / n) : 0,
             dev: st ? st.deviceId : null,
             track: t ? (t.label || '(无名)') + ' ' + t.readyState + (t.muted ? ' MUTED' : '')
                        + ' 处理=' + procTag(st) : '(无轨道)',
             lvN: L.n || 0, lvMin: L.n ? L.rmsMin : 0, lvMax: L.rmsMax || 0,
             lvVoiced: L.voiced || 0, lvBar: L.aMax || 0, lvTrack: L.label || '(未启动)' };
  }
  // -80 dBFS。任何真麦克风的底噪都比这高一个数量级（实测有人说话时峰值 0.15~0.99），
  // 所以低于它只有一种解释：这一路压根没有信号，不是"声音小"。留一点余量而不是死比
  // 零，是因为静音的虚拟设备有时送的是抖动噪声而不是干净的零。
  const SILENT_PEAK = 1e-4;
  // "没录到声音"和"没听清"是两件事，处置也完全不同：前者要去看麦克风和设备，
  // 后者才是识别问题。以前两者共用一句"没听清"，于是一次设备故障伪装成识别问题
  // 查了大半夜——这个判断存在的全部理由就是别再让它伪装一次。
  function silentSnap(snap) { return !!snap && snap.peak < SILENT_PEAK; }
  // 录到了，只是轻到 ASR 不可能转出东西。实测失败的三次整段峰值是
  // 0.0420 / 0.0105 / 0.0017，成功的几次是 0.9042 / 0.9052 / 0.9960——中间隔着一个
  // 数量级还多，不存在够不着的中间地带。这一档说的是"收音的麦克风离你很远"。
  const QUIET_PEAK = 0.05;
  function quietSnap(snap) { return !!snap && !silentSnap(snap) && snap.peak < QUIET_PEAK; }
  // 一段音频"为什么什么也没转出来"只有三个互斥的答案，而它们该去查的地方完全不同：
  // 压根没信号 → 查设备；有信号但轻得离谱 → 收音的那支麦克风离你很远；音量正常
  // → 这才真是识别问题。三种合成一句"没听清"，就是让设备故障伪装成识别问题。
  //
  // 药丸上那一句只报事实，不报猜测。以前轻到转不出来的那一档写的是「录到的声音
  // 太轻，可能收错了麦克风」——「可能收错了麦克风」是这边替用户下的结论，而这边
  // 只量得到峰值，量不到他对着哪支说话。猜错的时候他会照着这句去翻设备列表，翻
  // 完一圈发现设备没错。所以两档都只写「没录到」；到底是一点信号都没有，还是有
  // 信号但轻到转不出字，留在下面那条长的里（它带着实测峰值，进历史）。
  function emptyWhy(snap, short) {
    if (silentSnap(snap)) return short ? '没录到' : '空 — 没录到声音（整段峰值低于静音阈值）';
    if (quietSnap(snap)) return short ? '没录到'
                                      : '空 — 录到的声音太轻（整段峰值 ' + snap.peak.toFixed(4) + '，远低于正常说话）';
    return short ? '没听清，什么也没转出来' : '空 — 没听清，什么也没转出来';
  }
  // 快照挂在 rec/job 上而不是模块变量上：上一轮还在转写时又录了一轮，
  // 模块变量会被后一轮覆盖，超时那条日志就写成了别人的数。
  function dictDone(snap, outcome) {
    if (!snap || snap.logged) return;
    snap.logged = true;
    // 录完发现整段是静音，就把这支记下来，下一次开录先跳过它。开录前那 140 ms 的
    // 探测只问得出"此刻在不在送采样"，一支靠抖动噪声蒙混过关的设备照样能骗过它；
    // 一整段录音才是最硬的证据，白拿的，不用就浪费了。
    if (silentSnap(snap) && snap.dev) deadMics.set(snap.dev, Date.now());
    const f = v => Number(v).toFixed(4);
    dictLog('done ' + snap.id + ' 确认=是'
      + ' 音频=' + snap.sec.toFixed(1) + 's'
      + ' 录到peak/rms=' + f(snap.peak) + '/' + f(snap.rms)
      + ' 录音轨=[' + snap.track + ']'
      + ' 波形帧=' + snap.lvN + ' rms=' + f(snap.lvMin) + '~' + f(snap.lvMax)
      + ' 过闸=' + snap.lvVoiced + ' 柱高max=' + Number(snap.lvBar).toFixed(2)
      + ' 波形轨=[' + snap.lvTrack + ']'
      + ' 结果=' + outcome);
  }

  async function stopRec() {
    clearTail();
    const rec = activeRecording;
    if (!rec) return;
    activeRecording = null;
    // 必须在任何 draw() 之前取：紧接着的状态切换会让药丸收掉电平流，
    // 晚一步 stream 可能已经不是这一轮的了。
    const snap = snapDict(rec);
    if (rec.context.settings.muteWhileRecording) invoke('mute_set', { on: false }).catch(() => {});
    // 实时模式下前面的采样已经发过了，这里只补剩下的，不然音频会重复一遍。
    const tail = rec.chunks.slice(rec.sent);
    const total = tail.reduce((n,c) => n+c.length, 0);
    const x = new Float32Array(total);
    let off = 0; for (const c of tail) { x.set(c, off); off += c.length; }
    if (rec.cancelled) {
      if (rec.started) try { ws && ws.send(JSON.stringify({type:'cancel_audio',audio_id:rec.id})); } catch {}
      dictDone(snap, '已取消');
      draw({ t: 'done', s: '已取消' });
      return;
    }

    if (!rec.started && !sendStart(rec)) {
      keepAudio(rec.id, rec.chunks, '引擎未连接');
      dictDone(snap, '引擎未连接（sendStart 失败）');
      draw({ t: 'done', s: '引擎未连接（音频已存进历史）' });
      return;
    }
    // chunks 挂到 job 上，是为了让那些拿不到 rec 的失败分支（引擎重启、协议
    // 错误、引擎错误）也能把音频存下来。录音这时已经停了（activeRecording 早
    // 就置空），不会再有人往里追加，留个引用既安全也不多占内存。
    const job = { id:rec.id, context:rec.context, status:'processing',
                  timer:null, chunks: rec.chunks, snap };
    jobs.set(rec.id, job);
    drawRefine();
    if (ws && wsReady) {
      const allSamples = rec.chunks.reduce((n,c) => n+c.length, 0);
      if (total) {
        const buf = await new Blob([wavHeader(total), x.buffer], { type: 'audio/wav' }).arrayBuffer();
        ws.send(encodeAudioFrame(rec.id, buf));
      }
      ws.send(JSON.stringify({ type: 'end_audio', audio_id: rec.id, mode: 'transcript',
        total_duration: allSamples/SR, send_time: Date.now() }));
    } else {
      jobs.delete(rec.id);
      keepAudio(rec.id, rec.chunks, '引擎未连接');
      dictDone(snap, '引擎未连接（ws 未就绪）');
      draw({ t: 'done', s: '引擎未连接（音频已存进历史）' });
      return;
    }
    // 上限必须随录音长度放大。原先是个跟音频无关的常数（60 秒），于是"说得越长
    // 越容易被判成引擎没响应"——CPU 上光 ASR 就要 0.3~0.7 倍音频时长，两分半的
    // 话必然超。2026-09-05 那条 161.8 秒的听写就是被这个计时器掐掉的，引擎当时
    // 还在正常转写。基数 180 秒，另按每秒音频加 3 秒，封顶 30 分钟。
    // （基数原先读设置里的「精修超时」，那一项随精修链路一起删了。）
    const recSeconds = rec.chunks.reduce((n,c) => n+c.length, 0) / SR;
    const waitS = Math.min(1800, Math.max(15, 180 + 3 * recSeconds));
    job.timer = setTimeout(() => {
      if (!jobs.delete(rec.id)) return;
      clearJobTimers(job);
      keepAudio(rec.id, rec.chunks, '转写超时');
      dictDone(snap, '转写超时');
      draw({ t: 'done', s: '转写超时（音频已存进历史，可重新转录）' });
      try { ws && ws.send(JSON.stringify({type:'cancel_audio',audio_id:rec.id})); } catch {}
    }, waitS * 1000);
    // thinking 干等满 30 秒就把"存音频"按钮亮出来，让用户自己叫停这一轮。
    job.saveTimer = setTimeout(() => {
      if (!jobs.has(rec.id)) return;
      saveOffer = rec.id;
      drawRefine();
    }, SAVE_OFFER_MS);
  }

  // 松开触发键那一刻，最后一两个字常常还在路上：嘴上刚说完，声音还没走完，
  // 手已经抬起来了。立刻停采就把尾音切掉，转出来少几个字。所以停止请求先挂
  // 一小会儿——麦克风照常往 activeRecording.chunks 里灌，到点了再真的收尾。
  // 默认 1 秒，设置里可调，调成 0 就是老行为（立刻停）。
  //
  // 多录的这一秒是**后台**的事，界面不该跟着等：松键当下就把状态切走。不然
  // 录音条要多跳一秒才转 thinking，看着像卡了一下，还会让人以为按键没生效。
  let tailTimer = null;
  function clearTail() { if (tailTimer) { clearTimeout(tailTimer); tailTimer = null; } }
  function stopRecSoon() {
    // 尾巴还没走完又按了一次：当成"我说完了，别磨蹭"，立刻收尾。
    if (tailTimer) { clearTail(); stopRec(); return; }
    if (!activeRecording) { stopRec(); return; }
    const st = activeRecording.context.settings || {};
    const ms = Math.max(0, Math.min(3000, Number(st.recordTailMs ?? 1000) || 0));
    if (!ms) { stopRec(); return; }
    // 和 stopRec 收尾时画的是同一个状态，一秒后它会再画一次，重复无害。
    drawRefine();
    tailTimer = setTimeout(() => { tailTimer = null; stopRec(); }, ms);
  }

  function cancelRec() {
    // 取消不等尾巴：用户要的是立刻作废。
    clearTail();
    if (activeRecording) {
      activeRecording.cancelled = true;
      stopRec();
      return;
    }
    const job = Array.from(jobs.values()).at(-1);
    if (!job) return;
    clearJobTimers(job);
    jobs.delete(job.id);
    cancelledIds.add(job.id);
    try { ws && wsReady && ws.send(JSON.stringify({type:'cancel_audio',audio_id:job.id})); } catch {}
    dictDone(job.snap, '已取消（思考中）');
    draw({ t: 'done', s: '已取消' });
  }

  // "中断思考并存音频"。和取消只差最后一步：收尾流程完全一样（停两个计时器、
  // 告诉引擎别算了、撤掉输入框里的占位符），区别在于录音要留下来，
  // 于是历史里多一条 audio_only，回头能在历史页点"重新转录"。
  function saveJobAudio() {
    const job = (saveOffer && jobs.get(saveOffer)) || Array.from(jobs.values()).at(-1);
    if (!job) return;
    clearJobTimers(job);
    jobs.delete(job.id);
    // 引擎那边可能已经在回结果的路上了，标记成已取消，免得音频都存进历史了
    // 又被一个迟到的 refine_completed 敲进输入框。
    cancelledIds.add(job.id);
    try { ws && wsReady && ws.send(JSON.stringify({type:'cancel_audio',audio_id:job.id})); } catch {}
    keepAudio(job.id, job.chunks, '手动中断');
    dictDone(job.snap, '手动中断（音频存进历史）');
    draw({ t: 'done', s: '已存进历史，可重新转录' });
  }

  window.addEventListener('localless:rec-start', startRec);
  window.addEventListener('localless:rec-stop', stopRecSoon);
  window.addEventListener('localless:rec-cancel', cancelRec);
  window.addEventListener('localless:rec-save', saveJobAudio);
  window.__llToggle = () => {
    if (activeRecording || tailTimer) { stopRecSoon(); return 'stop'; }
    if (startingRecording) {
      startingRecording.cancelled = true;
      draw({ t: 'done', s: '' });
      return 'stop';
    }
    if (jobs.size) return 'thinking';
    startRec(); return 'start';
  };
  window.__llCancel = () => cancelRec();
  window.__llSaveAudio = () => saveJobAudio();

  // 快捷键 / 悬浮麦克风那一下。Electron 版是主进程 executeJavaScript 拿返回值，
  // 这边反过来——主进程发事件，页面自己把该回报的那一种回报上去。
  // 'thinking' 那句话和 main.js:664 一字不差：悬浮麦克风要变黄并说明为什么没开录。
  // 这里**故意不走 draw()**：draw 会同时派发 localless:ui，把药丸上正在转写的
  // 提示替换掉；而这条只是给悬浮麦看的一句旁注，药丸那侧什么都不该变。
  listen('localless://toggle', () => {
    let out = 'error';
    try { out = window.__llToggle(); }
    catch (e) {
      console.error('[localless] toggle failed:', e);
      invoke('recorder_state', { detail: { state: 'error', message: '无法切换录音' } }).catch(() => {});
      return;
    }
    if (out === 'thinking')
      invoke('recorder_state', { detail: { state: 'busy', message: '上一段仍在处理' } }).catch(() => {});
  });

  // 给测试用的把手。Electron 版的 test_controls.js 是拿 fs.readFileSync 把
  // preload.cjs 读成字符串、正则抠出函数体再 eval——函数一改名、一换成箭头函数、
  // 大括号一多一层，测试就找不着人了，而它报的错是「找不到 xxx」，看不出是
  // 逻辑坏了还是测试坏了。这里直接把那几个纯函数挂出来，测试跑的就是线上这一份。
  //
  // 只挂纯函数（同样的入参必得同样的出参，不碰麦克风/WS/DOM）。有副作用的一概
  // 不挂：那是给测试开后门，不是让它看得见。
  window.__llPure = {
    pasteLanded, targetWritable, looksEdited, micPrefer, uuMic,
    emptyWhy, silentSnap, quietSnap, procTag, bucketVramMb,
    wavHeader, wavPcm16, encodeAudioFrame,
  };

  ensureWs();
  window.__locallessRecorder = true;
  // 起来就先热一轮麦克风。Electron 那边靠 devicechange / 远程推送触发第一次预热，
  // 冷启动后的第一次听写因此要自付那 500~600ms 的挑选开销——正好吃掉按下键顺口
  // 说的头半句。这边开机就热，代价是一次后台探测。
  warmMic('启动');
  invoke('selfcheck', { line: '录音链路就绪' }).catch(() => {});
})();

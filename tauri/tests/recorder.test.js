// tauri/src/recorder.js 里那几个纯判据的单元测试。
//
// 为什么重写一份：Electron 版的 app/test_controls.js 断言的是**源码文本**——
// fs.readFileSync 读出 preload.cjs，正则抠出函数体，再 eval 成函数来跑。
// 那套写法有两个毛病，转成 Tauri 之后会一起爆掉：
//   1. 函数改个名、换成箭头函数、外面多包一层大括号，测试就找不着人，而它报的
//      错是「找不到 xxx」——看不出是逻辑坏了还是测试坏了。
//   2. 抠出来的函数体脱离了原文件的闭包，模块级常量（SR / QUIET_PEAK / UU_MIC）
//      全得在测试里再抄一遍。抄错、或者源码那边改了数没同步，测试照样绿。
//
// 这一份反过来：把整个 recorder.js 在打了桩的浏览器环境里真跑一遍，然后从
// window.__llPure 上取那几个函数。跑的就是线上装进药丸页面的那一份，常量也是
// 它自己的。函数改名 → 取不到 → 报的是「__llPure 里没有 xxx」，一眼是哪种坏。
//
// 跑法：node tauri/tests/recorder.test.js

const fs = require('fs');
const path = require('path');
const vm = require('vm');
const assert = require('assert');

// ── 浏览器环境的桩 ────────────────────────────────────────────────
// 只桩到「recorder.js 能加载完、不抛」为止。麦克风一律返回空列表并拒绝开流，
// WebSocket 永远停在 CONNECTING——纯函数不碰这两样，而让它们真连上只会在测试
// 进程里留下计时器和句柄。

const invoked = [];
const tauriListeners = new Map();

function stubInvoke(cmd) {
  switch (cmd) {
    case 'get_context': return { settings: {}, history: [], corrections: [] };
    case 'read_focused': return {};
    case 'mic_pref': return {};
    case 'remote_mic_now': return false;
    default: return null;
  }
}

const win = {
  addEventListener() {},
  removeEventListener() {},
  dispatchEvent() { return true; },
  __TAURI__: {
    core: {
      invoke(cmd, args) { invoked.push([cmd, args]); return Promise.resolve(stubInvoke(cmd)); },
    },
    event: {
      listen(name, fn) { tauriListeners.set(name, fn); return Promise.resolve(() => {}); },
    },
  },
};

class FakeWebSocket {
  constructor(url) { this.url = url; this.readyState = 0; this.binaryType = ''; }
  send() {}
  close() { this.readyState = 3; }
}

class FakeAudioContext {
  constructor() { this.sampleRate = 16000; this.destination = {}; }
  createAnalyser() { return { fftSize: 1024, getFloatTimeDomainData() {}, connect() {}, disconnect() {} }; }
  createGain() { return { gain: { value: 0 }, connect() {}, disconnect() {} }; }
  createMediaStreamSource() { return { connect() {}, disconnect() {} }; }
  createScriptProcessor() { return { connect() {}, disconnect() {}, onaudioprocess: null }; }
  close() { return Promise.resolve(); }
}

class FakeCustomEvent {
  constructor(type, init) { this.type = type; this.detail = init && init.detail; }
}

const navStub = {
  mediaDevices: {
    enumerateDevices() { return Promise.resolve([]); },
    getUserMedia() { return Promise.reject(new Error('测试环境没有麦克风')); },
    addEventListener() {},
  },
};

Object.assign(globalThis, {
  window: win,
  WebSocket: FakeWebSocket,
  AudioContext: FakeAudioContext,
  CustomEvent: FakeCustomEvent,
});
// Node 21 起 navigator 是只读全局，直接赋值会静默失败。
Object.defineProperty(globalThis, 'navigator', { value: navStub, configurable: true, writable: true });

const SRC = path.join(__dirname, '..', 'src', 'recorder.js');
vm.runInThisContext(fs.readFileSync(SRC, 'utf8'), { filename: SRC });

const P = win.__llPure;
assert.ok(P, 'recorder.js 没挂出 window.__llPure —— 加载时就抛了，或者把手被删了');
for (const k of ['pasteLanded', 'targetWritable', 'looksEdited', 'micPrefer', 'uuMic',
                 'emptyWhy', 'silentSnap', 'quietSnap', 'procTag', 'bucketVramMb',
                 'wavHeader', 'wavPcm16', 'encodeAudioFrame'])
  assert.strictEqual(typeof P[k], 'function', `__llPure 里没有 ${k}`);

// 加载过程本身就是一条断言：药丸页面一装上 recorder.js，就该看到这一句。
assert.ok(invoked.some(([c]) => c === 'selfcheck'), '加载完没有报 selfcheck，启动链路半路断了');

const tests = [];
const t = (name, fn) => tests.push([name, fn]);

// ── pasteLanded ───────────────────────────────────────────────────
// 立场：只有拿到**正面证据说明没落地**才报失败。读不到一律放过。
// 反过来会重演上一版的毛病——每次粘贴成功之后还多蹦一个药丸出来。

t('pasteLanded：读不到控件 → 算成功', () => {
  assert.strictEqual(P.pasteLanded('你好世界', null, null), true);
  assert.strictEqual(P.pasteLanded('你好世界', null, {}), true);
});

t('pasteLanded：控件不暴露自己的文本 → 算成功', () => {
  // 终端、canvas 编辑器、自绘控件：两个 pattern 都没有。
  const after = { hasValue: false, hasText: false, text: '' };
  assert.strictEqual(P.pasteLanded('你好世界', null, after), true);
});

t('pasteLanded：焦点已经换了控件 → 算成功', () => {
  const before = { processId: 100, automationId: 'a', controlType: 'Edit' };
  const after = { processId: 200, automationId: 'b', controlType: 'Edit', hasText: true, text: '' };
  assert.strictEqual(P.pasteLanded('你好世界', before, after), true);
});

t('pasteLanded：命中就是成功，没命中才是失败', () => {
  const f = { processId: 1, automationId: 'x', controlType: 'Edit', hasText: true };
  assert.strictEqual(P.pasteLanded('今天天气不错', f, { ...f, text: '前文今天天气不错后文' }), true);
  assert.strictEqual(P.pasteLanded('今天天气不错', f, { ...f, text: '完全不相干的内容' }), false);
});

t('pasteLanded：CRLF / 空格差异不算失败', () => {
  const f = { processId: 1, hasValue: true };
  // 剪贴板里是 LF，控件读回来是 CRLF，外加对方程序补的缩进。
  assert.strictEqual(P.pasteLanded('第一行\n第二行', f, { ...f, text: '  第一行\r\n  第二行' }), true);
});

t('pasteLanded：只比前 12 字，后面被对方加工过照样算成功', () => {
  const f = { processId: 1, hasText: true };
  const text = '这是一段足够长的听写结果用来验证前缀比对';
  // 自动补全 / 标点规范化把后半截改了，整段比对会误报。
  assert.strictEqual(P.pasteLanded(text, f, { ...f, text: '这是一段足够长的听写结果，用来验证。' }), true);
});

t('pasteLanded：长文档读不全 → 算成功', () => {
  // uia-helper 的 TextPattern 分支只取前 10000 字，粘在末尾的内容不在这一段里。
  const f = { processId: 1, hasText: true };
  assert.strictEqual(P.pasteLanded('末尾的新内容', f, { ...f, text: '甲'.repeat(9000) }), true);
  assert.strictEqual(P.pasteLanded('末尾的新内容', f, { ...f, text: '甲'.repeat(8999) }), false);
});

t('pasteLanded：全空白的文本没什么可比 → 算成功', () => {
  const f = { processId: 1, hasText: true };
  assert.strictEqual(P.pasteLanded('   \n  ', f, { ...f, text: '别的内容' }), true);
});

// ── pasteLanded 的插入点证据 ──────────────────────────────────────
// 这几条盯的是「最后一刻把光标移出输入框」那一类：粘贴键落进浏览器的页面正文，
// 一个字都没进去，而正文长过 10000 字就正好走了上面「读不全 → 算成功」那条路，
// 于是字既没进输入框、也没上药丸。插入点那一段不受那个截断影响。

t('pasteLanded：插入点前面就是刚粘的字 → 成功', () => {
  const f = { processId: 1, hasText: true };
  // 正文读不全（长文档），但光标紧跟在落下的字后面。
  assert.strictEqual(P.pasteLanded('末尾的新内容', f,
    { ...f, text: '甲'.repeat(9000), caret: '……前文末尾的新内容' }), true);
});

t('pasteLanded：插入点比的是末尾，不是开头', () => {
  const f = { processId: 1, hasText: true };
  // 一整段长听写：开头那十来个字早跑出 400 字的取样窗口了，只有末尾贴着光标。
  const text = '开头这一句' + '中间的废话'.repeat(120) + '收尾这一句';
  assert.strictEqual(P.pasteLanded(text, f,
    { ...f, text: '甲'.repeat(9000), caret: '中间的废话中间的废话收尾这一句' }), true);
});

t('pasteLanded：读到插入点却没有我们的字 → 落空', () => {
  const f = { processId: 1, hasText: true };
  // 光标掉回了浏览器的页面正文。正文长得读不全，但插入点这一段能定案。
  assert.strictEqual(P.pasteLanded('末尾的新内容', f,
    { ...f, text: '甲'.repeat(9000), caret: '页面正文里的某一段话' }), false);
});

t('pasteLanded：长文档一个字都没变 → 落空', () => {
  const f = { processId: 1, hasText: true, text: '甲'.repeat(9000) };
  // 控件不给插入点（SupportedTextSelection=None），但粘完和粘前完全一样。
  assert.strictEqual(P.pasteLanded('末尾的新内容', f, { ...f }), false);
  // 变了一点点、又找不到我们的字：真不知道，按老规矩放过。
  assert.strictEqual(P.pasteLanded('末尾的新内容', f,
    { ...f, text: '甲'.repeat(8990) + '乙'.repeat(10) }), true);
});

// ── targetWritable ────────────────────────────────────────────────

t('targetWritable：读不到焦点 → 按可写处理', () => {
  assert.strictEqual(P.targetWritable(null), true);
  assert.strictEqual(P.targetWritable({ context: {} }), true);
});

const job = f => ({ context: { focused: f } });

t('targetWritable：editable 要先于 valueReadOnly 判（浏览器系的回归点）', () => {
  // msedgewebview2 的根 Document 实测就是这一组：页面里全是可编辑输入框，
  // 它照样报 IsReadOnly=true。拿 valueReadOnly 先判就会把整个浏览器系判成不可写。
  assert.strictEqual(P.targetWritable(job({ editable: true, valueReadOnly: true })), true);
});

t('targetWritable：有 TextPattern 且能拿焦点 → 可写', () => {
  assert.strictEqual(P.targetWritable(job({ hasText: true })), true);
  assert.strictEqual(P.targetWritable(job({ hasText: true, keyboardFocusable: true })), true);
  // 明确说了拿不到键盘焦点，才轮得到只读标记说话。
  assert.strictEqual(P.targetWritable(job({ hasText: true, keyboardFocusable: false, valueReadOnly: true })), false);
});

t('targetWritable：沉默不算证据', () => {
  // Chromium 的无障碍树要等第一次 UIA 请求之后才建起来，首次听写读到的就是这个。
  assert.strictEqual(P.targetWritable(job({})), true);
  assert.strictEqual(P.targetWritable(job({ controlType: 'Edit' })), true);
  assert.strictEqual(P.targetWritable(job({ controlType: 'Document' })), true);
  assert.strictEqual(P.targetWritable(job({ controlType: 'Custom' })), true);
});

t('targetWritable：明确的只读证据才拒写', () => {
  assert.strictEqual(P.targetWritable(job({ valueReadOnly: true })), false);
  assert.strictEqual(P.targetWritable(job({ controlType: 'Button' })), false);
  assert.strictEqual(P.targetWritable(job({ controlType: 'ListItem' })), false);
  assert.strictEqual(P.targetWritable(job({ controlType: 'Window' })), false);
});

// ── looksEdited ───────────────────────────────────────────────────

t('looksEdited：原样和小改动都认得出', () => {
  assert.strictEqual(P.looksEdited('今天天气不错', '今天天气不错'), true);
  assert.strictEqual(P.looksEdited('今天天气不错', '今天天气不错。'), true);
  assert.strictEqual(P.looksEdited('我叫张伟今天来开会', '我叫章炜今天来开会'), true);
});

t('looksEdited：换成一句不相干的话就不认了', () => {
  assert.strictEqual(P.looksEdited('今天天气不错适合出门', '明晚把报销单发给财务部'), false);
});

t('looksEdited：二元组而不是单字——同样的字母换个顺序不算', () => {
  // 单字重合率 100%，二元组重合率 0。这正是当初非用二元组不可的理由。
  assert.strictEqual(P.looksEdited('abcdef', 'fedcba'), false);
});

t('looksEdited：短到没有二元组就放行', () => {
  assert.strictEqual(P.looksEdited('好', '嗯'), true);
  assert.strictEqual(P.looksEdited('', '随便什么'), true);
});

// ── micPrefer / uuMic ─────────────────────────────────────────────

const devs = [
  { deviceId: 'id-a', label: '麦克风 (PD100X)' },
  { deviceId: 'id-b', label: '麦克风阵列 (UU远程虚拟音频设备)' },
];

t('micPrefer：没指名就跟随系统默认', () => {
  assert.strictEqual(P.micPrefer(devs, null), null);
  assert.strictEqual(P.micPrefer(devs, {}), null);
  assert.strictEqual(P.micPrefer(devs, { id: '', label: '' }), null);
});

t('micPrefer：id 还在就按 id', () => {
  assert.strictEqual(P.micPrefer(devs, { id: 'id-a', label: '随便写的旧名字' }), 'id-a');
});

t('micPrefer：id 失效了退回按名字（远程重连换 id 的那条）', () => {
  // 远程虚拟音频设备每断开重连一次 id 就换一个。只认 id 的话，用户明明选好了，
  // 下一次远程连进来还是挑不中它。
  assert.strictEqual(P.micPrefer(devs, { id: '上次的旧 id', label: '麦克风阵列 (UU远程虚拟音频设备)' }), 'id-b');
});

t('micPrefer：剥掉 Chromium 加的 Default - 前缀再比', () => {
  assert.strictEqual(P.micPrefer(devs, { id: '', label: 'Default - 麦克风 (PD100X)' }), 'id-a');
  assert.strictEqual(P.micPrefer(devs, { id: '', label: 'Communications - 麦克风 (PD100X)' }), 'id-a');
});

t('micPrefer：两样都对不上就当没指名', () => {
  assert.strictEqual(P.micPrefer(devs, { id: 'nope', label: '一支早就拔掉的麦克风' }), null);
});

t('uuMic：认后半截，前半截跟着主机声卡变', () => {
  assert.strictEqual(P.uuMic(devs), 'id-b');
  assert.strictEqual(P.uuMic([{ deviceId: 'x', label: '扬声器 (UU远程虚拟音频设备)' }]), 'x');
  assert.strictEqual(P.uuMic([devs[0]]), null);
  assert.strictEqual(P.uuMic([{ deviceId: 'y' }]), null);
});

// ── 空结果的三种解释 ──────────────────────────────────────────────
// 这三档存在的全部理由：别再让一次设备故障伪装成识别问题。

const snap = peak => ({ peak });

// 药丸上那句（short=true）只有两种：「没录到」和「没听清」。故意不猜原因——
// 「可能听错了麦克风」这种话在没录到的时候是瞎猜，而且 .ll-text 是 nowrap +
// 省略号，写长了也只显示前四个字。真正的区分留给 short=false 那句，它带实测峰值。
t('没信号 / 太轻 / 没听清 是三个互斥的答案', () => {
  assert.strictEqual(P.silentSnap(snap(0)), true);
  assert.strictEqual(P.quietSnap(snap(0)), false);
  assert.match(P.emptyWhy(snap(0), true), /没录到/);
  assert.match(P.emptyWhy(snap(0), false), /没录到声音/);

  // 2026-09-21 实测那一条：整段峰值 0.0139。
  assert.strictEqual(P.silentSnap(snap(0.0139)), false);
  assert.strictEqual(P.quietSnap(snap(0.0139)), true);
  assert.match(P.emptyWhy(snap(0.0139), true), /没录到/);
  assert.match(P.emptyWhy(snap(0.0139), false), /太轻/);
  assert.match(P.emptyWhy(snap(0.0139), false), /0\.0139/);

  // 两档药丸文案一样，长文案必须还能分得开——否则查历史时「没录到」和
  // 「录到了但太轻」就成了同一条，设备故障又一次伪装成识别问题。
  assert.notStrictEqual(P.emptyWhy(snap(0), false), P.emptyWhy(snap(0.0139), false));

  // 成功那几次是 0.9042 / 0.9052 / 0.9960，这一档才真是识别问题。
  assert.strictEqual(P.silentSnap(snap(0.9042)), false);
  assert.strictEqual(P.quietSnap(snap(0.9042)), false);
  assert.match(P.emptyWhy(snap(0.9042), true), /没听清/);
});

t('两条阈值都是严格小于', () => {
  assert.strictEqual(P.silentSnap(snap(1e-4)), false, 'SILENT_PEAK 上那一点不算静音');
  assert.strictEqual(P.silentSnap(snap(9.9e-5)), true);
  assert.strictEqual(P.quietSnap(snap(0.05)), false, 'QUIET_PEAK 上那一点不算太轻');
  assert.strictEqual(P.quietSnap(snap(0.0499)), true);
});

t('没有快照时一概不下结论', () => {
  assert.strictEqual(P.silentSnap(null), false);
  assert.strictEqual(P.quietSnap(undefined), false);
  assert.match(P.emptyWhy(null, true), /没听清/);
});

// ── procTag ───────────────────────────────────────────────────────

t('procTag：三样处理的开关状态要能一眼看见', () => {
  assert.strictEqual(P.procTag(null), '?');
  assert.strictEqual(P.procTag({ echoCancellation: false, noiseSuppression: false, autoGainControl: false }), '无');
  assert.strictEqual(P.procTag({ echoCancellation: true, noiseSuppression: true, autoGainControl: true }), 'AEC+NS+AGC');
  assert.strictEqual(P.procTag({ echoCancellation: true, autoGainControl: true }), 'AEC+AGC');
});

// ── bucketVramMb ──────────────────────────────────────────────────

t('bucketVramMb：分桶是为了别让显存数字每 4 秒刷一次托盘', () => {
  assert.strictEqual(P.bucketVramMb(0), 0);
  assert.strictEqual(P.bucketVramMb(100), 0);
  assert.strictEqual(P.bucketVramMb(200), 1);
  assert.strictEqual(P.bucketVramMb(5000), 20);
  // 同一桶里的抖动要被吃掉，跨桶才算变。
  assert.strictEqual(P.bucketVramMb(5001), P.bucketVramMb(5100));
});

t('bucketVramMb：非数字原样透传（断线时发的就是 null）', () => {
  assert.strictEqual(P.bucketVramMb(null), null);
  assert.strictEqual(P.bucketVramMb(undefined), undefined);
});

// ── 两份 WAV 头 ───────────────────────────────────────────────────
// 它们不是一回事，写混了的后果是单向的：发给引擎的那份写成 int16，引擎照样转；
// 存档那份写成 float32，历史页点播放是一片死寂（Chromium 的 <audio> 解不了）。

const rd = buf => {
  const dv = new DataView(buf);
  const str = o => String.fromCharCode(dv.getUint8(o), dv.getUint8(o + 1), dv.getUint8(o + 2), dv.getUint8(o + 3));
  return {
    riff: str(0), wave: str(8), fmt: str(12), dataTag: str(36),
    riffSize: dv.getUint32(4, true), fmtSize: dv.getUint32(16, true),
    tag: dv.getUint16(20, true), channels: dv.getUint16(22, true),
    rate: dv.getUint32(24, true), byteRate: dv.getUint32(28, true),
    align: dv.getUint16(32, true), bits: dv.getUint16(34, true),
    dataSize: dv.getUint32(40, true),
  };
};

t('wavHeader：发给引擎的那份是 float32（fmt tag 3）', () => {
  const h = rd(P.wavHeader(1000));
  assert.strictEqual(h.riff, 'RIFF');
  assert.strictEqual(h.wave, 'WAVE');
  assert.strictEqual(h.fmt, 'fmt ');
  assert.strictEqual(h.dataTag, 'data');
  assert.strictEqual(h.tag, 3, 'fmt tag 必须是 3（IEEE float）');
  assert.strictEqual(h.bits, 32);
  assert.strictEqual(h.channels, 1);
  assert.strictEqual(h.rate, 16000);
  assert.strictEqual(h.byteRate, 16000 * 4);
  assert.strictEqual(h.align, 4);
  assert.strictEqual(h.dataSize, 4000);
  assert.strictEqual(h.riffSize, 36 + 4000);
});

t('wavPcm16：存档那份是 16 位整数（fmt tag 1）', () => {
  const buf = P.wavPcm16(new Float32Array(100));
  const h = rd(buf);
  assert.strictEqual(h.tag, 1, 'fmt tag 必须是 1（PCM），否则历史页放不出声');
  assert.strictEqual(h.bits, 16);
  assert.strictEqual(h.channels, 1);
  assert.strictEqual(h.rate, 16000);
  assert.strictEqual(h.byteRate, 16000 * 2);
  assert.strictEqual(h.align, 2);
  assert.strictEqual(h.dataSize, 200);
  assert.strictEqual(buf.byteLength, 44 + 200);
});

t('wavPcm16：样点换算与削幅', () => {
  const buf = P.wavPcm16(new Float32Array([0, 1, -1, 0.5, -0.5, 9, -9]));
  const dv = new DataView(buf);
  const at = i => dv.getInt16(44 + i * 2, true);
  assert.strictEqual(at(0), 0);
  assert.strictEqual(at(1), 0x7fff);
  assert.strictEqual(at(2), -0x8000);
  assert.strictEqual(at(3), Math.round(0.5 * 0x7fff) - 1); // 0x7fff/2 向零截断
  assert.strictEqual(at(4), -0x4000);
  assert.strictEqual(at(5), 0x7fff, '超出 ±1 必须夹住，不能回绕');
  assert.strictEqual(at(6), -0x8000);
});

// ── encodeAudioFrame ──────────────────────────────────────────────

t('encodeAudioFrame：LLAF 魔数 + 版本 + 变长 id', () => {
  const payload = new Uint8Array([9, 8, 7]);
  const frame = new Uint8Array(P.encodeAudioFrame('rec-abc', payload));
  assert.deepStrictEqual([...frame.slice(0, 6)], [76, 76, 65, 70, 1, 7]);
  assert.strictEqual(Buffer.from(frame.slice(6, 13)).toString('utf8'), 'rec-abc');
  assert.deepStrictEqual([...frame.slice(13)], [9, 8, 7]);
  assert.strictEqual(frame.length, 6 + 7 + 3);
});

t('encodeAudioFrame：id 按 UTF-8 字节数算，不是字符数', () => {
  const frame = new Uint8Array(P.encodeAudioFrame('录音', new Uint8Array(0)));
  assert.strictEqual(frame[5], 6, '两个汉字是 6 字节');
});

t('encodeAudioFrame：id 长度越界就当场抛', () => {
  // 长度字段只有 1 字节，超了会悄悄截断——引擎那边拿到的就是另一段音频的 id。
  assert.throws(() => P.encodeAudioFrame('', new Uint8Array(1)), /invalid audio_id/);
  assert.throws(() => P.encodeAudioFrame('x'.repeat(256), new Uint8Array(1)), /invalid audio_id/);
  assert.doesNotThrow(() => P.encodeAudioFrame('x'.repeat(255), new Uint8Array(1)));
});

// ── 跑 ────────────────────────────────────────────────────────────

let pass = 0, fail = 0;
for (const [name, fn] of tests) {
  try { fn(); pass++; }
  catch (e) { fail++; console.error(`✗ ${name}\n  ${e.message}`); }
}
console.log(`${pass} 过 / ${fail} 挂`);
process.exit(fail ? 1 : 0);

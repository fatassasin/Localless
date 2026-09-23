// 页面侧的几条「不许这么写」。
//
// 这一份和 recorder.test.js 的立场相反，是故意的：那边断言的是**行为**，
// 因为那些是纯函数，跑一遍就知道对不对；这边断言的是**源码文本**，因为这里的
// 缺陷本身就是文本性质的——「这个标识符不许出现在这个文件里」，没有能跑的函数
// 可以证伪它。拿行为测会绕一大圈（起 WebView2、开设置窗、发假事件），还测不到
// 真正想拦的那一下。
//
// 跑法：node tauri/tests/pages.test.js

const fs = require('fs');
const path = require('path');

const SRC = path.join(__dirname, '..', 'src');
const pages = fs.readdirSync(SRC).filter(f => f.endsWith('.html') || f.endsWith('.js'));

const tests = [];
const t = (name, fn) => tests.push([name, fn]);

// ── window.confirm 是坏的 ─────────────────────────────────────────
// tauri-plugin-dialog 2.7.3 注入的 init-iife.js 把 window.confirm 覆盖成调
// plugin:dialog|confirm，而这一版的 Rust 侧只剩 message / open / save——confirm
// 那条命令早删了，注入脚本没跟着改。于是 window.confirm 返回的是一个 rejected
// Promise：
//   - 不 await：if(promise) 恒为真，「确定/取消」两条路走的是同一条；
//   - await 了：直接抛 "dialog.confirm not allowed. Command not found"。
// 两种写法都是错的，加权限也救不回来（dialog:allow-confirm 在这一版只是
// allow-message 的废弃别名）。实测过：删模型那颗按钮连框都不弹就把模型删了。
//
// 能用的是 window.__TAURI__.dialog.confirm——它走还活着的 message 命令。
// settings.html 里包了一层 askOk()，同时负责「拿不到就返回 false」。

t('没有一页在用 window.confirm', () => {
  const bad = [];
  for (const f of pages) {
    const src = fs.readFileSync(path.join(SRC, f), 'utf8');
    src.split('\n').forEach((line, i) => {
      // 注释里提它是允许的——上面那一大段注释本身就得提。只抓真调用：
      // confirm(…) 前面不是 . 也不是别的标识符字符。
      if (/(^|[^.\w])confirm\s*\(/.test(line.replace(/\/\/.*$/, '').replace(/\/\*.*?\*\//g, '')))
        bad.push(`${f}:${i + 1}`);
    });
  }
  if (bad.length) throw new Error(`window.confirm 在这版插件里是坏的，改用 askOk()：\n  ${bad.join('\n  ')}`);
});

// 反过来也要验一条：askOk 得真在 settings.html 里，且删模型那处得 await 它。
// 少了这条，上面那条只要有人把 confirm 整个删掉就"绿"了。
t('删模型那处走的是 await askOk', () => {
  const src = fs.readFileSync(path.join(SRC, 'settings.html'), 'utf8');
  if (!/const\s+askOk\s*=/.test(src)) throw new Error('settings.html 里没有 askOk');
  if (!/if\s*\(\s*await\s+askOk\(/.test(src))
    throw new Error('删模型那处没有 await askOk(…)——不 await 的话判的是 Promise，恒为真');
});

// ── 药丸上不许有 title= ───────────────────────────────────────────
// title 会让 WebView2 弹 Windows 原生提示气泡。在普通窗口里那是正常交互，在
// 药丸上不是：药丸是个贴着屏幕的置顶小窗，而原生气泡是**另一个 OS 窗口**——
// 不受药丸窗口边界约束（违反「只遮药丸大小」），也不受本页任何 z-index 管。
// 表现是点叉的一瞬间旁边闪出一块带边框的条，且只在光标停够约 500 ms 再点才
// 赶得上，所以偶发、难复现。用 aria-label，读屏器拿到的信息一样，不画东西。
//
// 只管 pill.html。settings.html 是普通窗口，那边的 title 该留着。

t('pill.html 里没有 title=', () => {
  const src = fs.readFileSync(path.join(SRC, 'pill.html'), 'utf8');
  const bad = [];
  src.split('\n').forEach((line, i) => {
    if (/\stitle\s*=/.test(line)) bad.push(`pill.html:${i + 1}  ${line.trim()}`);
  });
  if (bad.length) throw new Error(`药丸上的 title 会弹原生气泡，改 aria-label：\n  ${bad.join('\n  ')}`);
});

// 反向：别让上一条被「把提示整个删光」糊弄过去。叉和勾至少得有 aria-label。
t('药丸的叉和勾留着 aria-label', () => {
  const src = fs.readFileSync(path.join(SRC, 'pill.html'), 'utf8');
  for (const cls of ['ll-x', 'll-ok']) {
    if (!new RegExp(`class="${cls}"[^>]*aria-label=`).test(src))
      throw new Error(`.${cls} 没有 aria-label——读屏器只会读到一个空按钮`);
  }
});

// ── 跑 ────────────────────────────────────────────────────────────

let pass = 0, fail = 0;
for (const [name, fn] of tests) {
  try { fn(); pass++; }
  catch (e) { fail++; console.error(`✗ ${name}\n  ${e.message}`); }
}
console.log(`${pass} 过 / ${fail} 挂`);
process.exit(fail ? 1 : 0);

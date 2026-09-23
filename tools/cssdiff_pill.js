// 把 Electron 版 preload.cjs 里那段 OVERLAY 的 CSS（字符串拼接）和 Tauri 版
// tauri/src/pill.html 的 <style> 逐条声明比一遍。「一比一复原」这句话，眼睛看
// 不出的错字只有这样才认得出来。
//
//   node tools/cssdiff_pill.js
//
// 差异 0 处才算搬对。html,body 那条是 Tauri 独有的——Electron 版是注进别人的
// 页面，没有自己的 body 可设。
const fs = require('fs');

function decls(css) {
  css = css.replace(/\/\*[\s\S]*?\*\//g, '');
  const out = new Map();
  // @keyframes / @media 整块当一条比
  const atRe = /@(keyframes|media)[^{]*\{(?:[^{}]*\{[^{}]*\})*[^{}]*\}/g;
  let m;
  while ((m = atRe.exec(css))) out.set(m[0].replace(/\s+/g, ''), '(整块)');
  css = css.replace(atRe, '');
  for (const rule of css.split('}')) {
    const i = rule.indexOf('{');
    if (i < 0) continue;
    const sel = rule.slice(0, i).replace(/\s+/g, ' ').trim();
    if (!sel) continue;
    const body = rule.slice(i + 1).split(';')
      .map(s => s.replace(/\s+/g, '').trim()).filter(Boolean).sort().join(';');
    out.set(sel, body);
  }
  return out;
}

const pre = fs.readFileSync('app/preload.cjs', 'utf8');
const TAIL = "transition:height .03s linear;}'";
const seg = pre.slice(pre.indexOf('style.textContent ='));
const block = seg.slice(0, seg.indexOf(TAIL) + TAIL.length);
// 行注释整行丢掉，剩下的单引号字符串拼回一整份 CSS
const strRe = new RegExp("'(?:[^'\\\\]|\\\\.)*'", 'g');
const parts = block.split('\n').filter(l => !l.trim().startsWith('//')).join('\n').match(strRe) || [];
const electronCss = parts.map(s => s.slice(1, -1)).join('');

const html = fs.readFileSync('tauri/src/pill.html', 'utf8');
const tauriCss = html.slice(html.indexOf('<style>') + 7, html.indexOf('</style>'));

const A = decls(electronCss), B = decls(tauriCss);
const SKIP = new Set(['html,body']);

let bad = 0;
for (const [sel, body] of A) {
  if (!B.has(sel)) { console.log('缺少      ' + sel); bad++; continue; }
  if (B.get(sel) !== body) {
    console.log('不一致    ' + sel + '\n  electron: ' + body + '\n  tauri   : ' + B.get(sel));
    bad++;
  }
}
for (const sel of B.keys())
  if (!A.has(sel) && !SKIP.has(sel)) { console.log('多出来    ' + sel); bad++; }

console.log('\nelectron ' + A.size + ' 条规则 · tauri ' + B.size + ' 条 · 差异 ' + bad + ' 处');
process.exit(bad ? 1 : 0);

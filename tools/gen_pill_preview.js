// 把 preload.cjs 里真正的 OVERLAY 源码抠出来，包成一个可以在浏览器里打开的页面，
// 用来肉眼验收药丸的样子。刻意不复制粘贴样式：复制一份就等于在测另一份代码，
// 药丸改了预览还是老样子，看着"过了"其实什么都没验。
const fs = require('fs');

const src = fs.readFileSync('app/preload.cjs', 'utf8');
const start = src.indexOf('const OVERLAY = `');
if (start < 0) throw new Error('找不到 OVERLAY');
const from = src.indexOf('`', start) + 1;
const to = src.indexOf('\n`;', from);
if (to < 0) throw new Error('找不到 OVERLAY 的收尾反引号');
const raw = src.slice(from, to);
if (raw.includes('${')) throw new Error('OVERLAY 里有 ${} 插值，不能这样直接求值');
// 用模板字面量本身的语义还原转义（\\n -> \n 等），比手写 replace 靠谱。
const body = new Function('return `' + raw + '`')();

const page = `<!doctype html>
<meta charset="utf-8">
<title>药丸预览</title>
<style>
  html,body{margin:0;height:100%;background:#5b5f66;font:14px -apple-system,Segoe UI,Arial,sans-serif}
  #bar{position:fixed;top:0;left:0;right:0;padding:10px;background:#25272b;color:#ddd;z-index:99}
  button{font:13px inherit;margin-right:6px;padding:5px 10px;border-radius:6px;border:1px solid #555;background:#33363c;color:#eee;cursor:pointer}
  #log{position:fixed;top:52px;left:10px;color:#ffd479;font:12px monospace;z-index:99}
</style>
<div id="bar">
  <button onclick="ui({t:'refine',hard:false,save:false})">thinking（未满 30 秒）</button>
  <button onclick="ui({t:'refine',hard:false,save:true})">thinking + 存音频按钮</button>
  <button onclick="ui({t:'refine',hard:true,save:true})">thinking hard + 存音频按钮</button>
  <button onclick="ui({t:'done',s:'已存进历史，可重新转录'})">点完之后</button>
</div>
<div id="log"></div>
<div id="root"></div>
<script>
  // 覆盖层原样跑，只把它依赖的两个外部对象换成桩。
  var ipcRenderer = { send: function(){} };
  window.locallessNative = {
    copyText: function(){}, undoLearnedWord: function(){}
  };
  window.ui = function (d) {
    window.dispatchEvent(new CustomEvent('localless:ui', { detail: d }));
    document.getElementById('log').textContent = '当前状态: ' + JSON.stringify(d);
  };
  window.addEventListener('localless:rec-save', function () {
    document.getElementById('log').textContent = '按钮点了 → 发出了 localless:rec-save';
  });
</script>
<script>
${body}
</script>
<script>ui({ t: 'refine', hard: false, save: true });</script>
`;

fs.writeFileSync('app/_pill_preview.html', page);
console.log('已生成 app/_pill_preview.html（OVERLAY 源码 ' + body.length + ' 字符）');

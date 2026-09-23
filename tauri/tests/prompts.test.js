// prompts.json 是默认提示词的唯一真源，engine.py 和设置页共读一份。
// 但迁移用的那条「忽略转写原文占位槽」正则不得不在两边各写一次（一个 Python 一个 JS）——
// 正是刚修掉的那类隐患，所以这里逐字比对，谁先改谁就得让测试通过。
//
// 转 Tauri 之后多了一道：frontendDist 会被打进二进制，设置页只能 fetch 同级路径，
// 所以 tauri/src/ 下必须有一份 prompts.json 的副本。副本就会漂，见第 0 条。
//
// 跑法：node tauri/tests/prompts.test.js

const fs = require('fs');
const path = require('path');
const assert = require('assert');

const ROOT = path.join(__dirname, '..', '..');
const APP = path.join(ROOT, 'app');
const SRC = path.join(ROOT, 'tauri', 'src');

const html = fs.readFileSync(path.join(SRC, 'settings.html'), 'utf8');
const py = fs.readFileSync(path.join(APP, 'engine.py'), 'utf8');
const raw = fs.readFileSync(path.join(APP, 'prompts.json'), 'utf8');
const file = JSON.parse(raw);

// 0. 两份副本必须逐字节一致
//
// engine.py 读的是 app/prompts.json（`Path(__file__).with_name`），设置页读的是打进
// 二进制的 tauri/src/prompts.json。改了一边不改另一边，症状是「设置页显示的默认提示词
// 和引擎实际用的不是同一段」——两边都不报错，只是对不上，而且页面那份还得重新
// cargo build 才会更新，最容易漏。model-registry.json 同理。
for (const name of ['prompts.json', 'model-registry.json']) {
  const a = fs.readFileSync(path.join(APP, name), 'utf8');
  const b = fs.readFileSync(path.join(SRC, name), 'utf8');
  assert.strictEqual(b, a,
    `app/${name} 和 tauri/src/${name} 不一致——改完一份要把另一份也拷过去`);
}

// 1. 共享文件本身完整
assert.ok(file.defaults && file.legacy, 'prompts.json 缺 defaults / legacy');
for (const k of ['asrPrompt', 'learnPrompt'])
  assert.ok(typeof file.defaults[k] === 'string' && file.defaults[k], `defaults 缺 ${k}`);

// 2. 设置页必须读它，且不许再抄一份字面量回来
assert.ok(html.includes("PROMPT_FILE.defaults"), '设置页没接到 prompts.json');
for (const k of ['asrPrompt', 'learnPrompt'])
  assert.ok(!new RegExp(k + ":\\s*'").test(html), `settings.html 又硬编码了一份 ${k}`);

// 3. 两边的"忽略占位槽"正则必须逐字一致，否则同一份保存值一边认得一边认不得
const jsRe = html.match(/replace\((\/\[ \\t\][^/]*\/)g,''\)/);
assert.ok(jsRe, 'settings.html 里找不到迁移用的正则');
const pyRe = py.match(/_TEXT_SLOT_RE = re\.compile\(r"([^"]+)"\)/);
assert.ok(pyRe, 'engine.py 里找不到 _TEXT_SLOT_RE');
assert.strictEqual(jsRe[1].slice(1, -1), pyRe[1],
  'JS 与 Python 的占位槽正则不一致——同一份旧默认会一边迁移一边不迁移');

// 4. 占位槽真的被忽略掉：槽位是占位不是措辞，加没加它都该认成"没自定义过"
const bare = v => String(v || '').replace(new RegExp(pyRe[1], 'g'), '').trim();
const oldAsr = file.legacy.asrPrompt[0];
assert.strictEqual(bare(oldAsr + '\n转写原文：\n{{text}}'), bare(oldAsr),
  '占位槽没被忽略——只多了个 {{text}} 就会被当成用户自定义，永远升级不了');
assert.notStrictEqual(bare(oldAsr + '\n我自己加的。'), bare(oldAsr),
  '忽略得过头了——用户真加的话也被抹掉，会覆盖掉人家改的提示词');

// 5. 用户真实设置文件的状态，跑测试时看得见（不做断言：自定义过是完全合法的）
const SET = path.join(process.env.APPDATA || '.', 'localless', 'localless-settings.json');
let saved = {};
try { saved = JSON.parse(fs.readFileSync(SET, 'utf8')); } catch {}
for (const k of Object.keys(file.legacy)) {
  const cur = saved[k];
  if (typeof cur !== 'string') { console.log(`  ${k}: 未保存（直接用默认）`); continue; }
  const isOld = file.legacy[k].some(o => bare(cur) === bare(o));
  console.log(`  ${k}: ` + (isOld ? '旧默认，打开设置页时会自动升级'
    : bare(cur) === bare(file.defaults[k]) ? '已是新默认' : '用户自定义过，不动'));
}

console.log('ok');

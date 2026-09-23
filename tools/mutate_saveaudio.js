// 对"存音频按钮"这个新功能做变异测试：每次往 preload.cjs 里注入一个真实可能
// 犯的错，确认 test_controls.js 会红。CAUGHT = 断言有效，MISSED = 断言是摆设。
const fs = require('fs'), cp = require('child_process');
const P = 'app/preload.cjs', orig = fs.readFileSync(P, 'utf8');

const muts = [
  ['邀约时间 30 秒 -> 90 秒', s => s.replace('const SAVE_OFFER_MS = 30000;', 'const SAVE_OFFER_MS = 90000;')],
  ['邀约时间又按 waitS 取比例', s => s.replace('}, SAVE_OFFER_MS);', '}, waitS / 3 * 1000);')],
  ['收尾时不清 saveTimer', s => s.replace('for (const t of [job.timer, job.saveTimer]) clearTimeout(t);', 'clearTimeout(job.timer);')],
  ['收尾时不清邀约标记', s => s.replace('if (saveOffer === job.id) saveOffer = null;', '')],
  ['按钮恒亮', s => s.replace('save: !!(saveOffer && jobs.has(saveOffer))', 'save: true')],
  ['job 死了按钮还亮', s => s.replace('save: !!(saveOffer && jobs.has(saveOffer))', 'save: !!saveOffer')],
  ['refine 丢掉 thinking 强度', s => s.replace("draw({ t: 'refine', hard: refineHard,", "draw({ t: 'refine', hard: false,")],
  ['中断了却不存音频', s => s.replace("keepAudio(job.id, job.chunks, '手动中断');", '')],
  ['中断原因记错', s => s.replace("keepAudio(job.id, job.chunks, '手动中断');", "keepAudio(job.id, job.chunks, '已取消');")],
  // 注意：preload.cjs 是 CRLF，这些多行模式必须写 \r?\n，写 \n 会静默匹配不上，
  // 变异不生效就等于没测（上一轮 6 条 SKIP 就是这么来的）。
  // cancelRec 里有一模一样的一行，且排在前面。不锚定到 saveJobAudio 的话
  // 变异会打到 cancelRec 身上，测试当然不红——那是漏报，不是断言弱。
  ['中断后不标记已取消（迟到结果会敲进输入框）', s => {
    const at = s.indexOf('function saveJobAudio() {');
    const head = s.slice(0, at), tail = s.slice(at);
    return head + tail.replace(/    cancelledIds\.add\(job\.id\);\r?\n/, '');
  }],
  ['中断后 job 不出队',
    s => s.replace(/    jobs\.delete\(job\.id\);\r?\n(?=    \/\/ 引擎那边)/, '')],
  ['中断后不通知引擎',
    s => s.replace(/    try \{ ws && wsReady && ws\.send\(JSON\.stringify\(\{type:'cancel_audio',audio_id:job\.id\}\)\); \} catch \{\}\r?\n(?=    keepAudio)/, '')],
  ['点了按钮却存错 job（无视邀约）', s => s.replace('const job = (saveOffer && jobs.get(saveOffer)) || Array.from(jobs.values()).at(-1);', 'const job = Array.from(jobs.values()).at(-1);')],
  ['按钮在所有状态下都显示', s => s.replace("'.ll-pill.refine.cansave .ll-save{display:flex;}'", "'.ll-pill .ll-save{display:flex;}'")],
  ['按钮默认就可见', s => s.replace(".ll-save{display:none;", ".ll-save{display:flex;")],
  ['实时模式也挂邀约计时器', s => s.replace('if (!rec.realtime) job.saveTimer = setTimeout(', 'job.saveTimer = setTimeout(')],
  ['邀约计时器不查 job 是否还活着',
    s => s.replace(/      if \(!jobs\.has\(rec\.id\)\) return;\r?\n(?=      saveOffer = rec\.id;)/, '')],
  ['ui 分支不理会 d.save', s => s.replace("setThinkingText(d.hard); if (d.save) pill.classList.add('cansave');", 'setThinkingText(d.hard);')],
  ['某个收尾点漏回裸 clearTimeout',
    s => s.replace(/            clearJobTimers\(job\);(\r?\n            jobs\.delete\(m\.audio_id\);\r?\n            keepAudio\(job\.id, job\.chunks, '引擎错误'\);)/,
                   '            clearTimeout(job.timer);$1')],
  ['按钮挪到文字左边', s => s.replace(
    /(      '<span class="ll-text">单击 RightAlt 说话<\/span>' \+\r?\n)(      '<button class="ll-save".*?<\/button>' \+\r?\n)/s, '$2$1')],
];

let caught = 0, skipped = 0;
for (const [name, f] of muts) {
  const next = f(orig);
  if (next === orig) { console.log(`SKIP    ${name}  (变异没生效，模式没匹配上)`); skipped++; continue; }
  fs.writeFileSync(P, next);
  const r = cp.spawnSync('node', ['app/test_controls.js'], { encoding: 'utf8' });
  const ok = r.status !== 0;
  if (ok) caught++;
  console.log(`${ok ? 'CAUGHT' : 'MISSED'}  ${name}`);
}
fs.writeFileSync(P, orig);
console.log(`\n${caught}/${muts.length} 被抓住${skipped ? `（${skipped} 条没生效）` : ''}`);
const r = cp.spawnSync('node', ['app/test_controls.js'], { encoding: 'utf8' });
console.log('还原后基线:', r.status === 0 ? 'green' : 'RED ' + r.stderr.slice(0, 300));

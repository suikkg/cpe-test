// 分层规则与全局禁令的静态检查。
//
// 这是「结构上不可能」这条纪律在前端侧的落点：旧页出过的 UI bug **全部是纯逻辑
// bug**（角色键忽略 pair.cross、整列勾选缺分支、`-l` 被全局档位反向覆写），
// 它们之所以难测，是因为逻辑和响应式、DOM、网络搅在一起。把领域逻辑赶进一个
// 没有 vue、没有 fetch、没有 state 的 domain/ 目录，普通 Vitest 就能钉住。
//
// 规则见 .ai/PLAN-v5.0-frontend.md §4.3 与 §6.2。命中任何一条即退出码 1。
//
// 用法：node scripts/lint-arch.mjs
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { dirname, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const srcRoot = resolve(here, '..', 'src');

/** 递归收集 src/ 下的全部文件，返回相对 src/ 的 POSIX 路径。 */
function walk(dir) {
  const out = [];
  for (const entry of readdirSync(dir).sort()) {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) out.push(...walk(full));
    else out.push(relative(srcRoot, full).split(sep).join('/'));
  }
  return out;
}

const failures = [];
const fail = (file, message) => failures.push(`${file}: ${message}`);

// —— §4.3 分层规则 ——
//
// 每一层写成「这一层是谁」+「它不许 import 谁」。禁令用相对路径的**目标目录**
// 判断，所以 `../state/plan` 和 `@/state/plan` 两种写法都能认出来。
const LAYERS = [
  {
    name: 'domain',
    match: (f) => f.startsWith('domain/'),
    // domain/ 不许 import vue 是这套规则的重心：没有响应式、没有 DOM、
    // 没有网络，才能用最便宜的方式把纯逻辑钉住。
    forbid: ['vue', 'api/client', 'state/', 'components/', 'views/'],
  },
  {
    name: 'api',
    match: (f) => f.startsWith('api/'),
    forbid: ['vue', 'state/', 'components/', 'views/'],
  },
  {
    name: 'state',
    match: (f) => f.startsWith('state/'),
    forbid: ['components/', 'views/'],
  },
  {
    name: 'components',
    // 展示件只吃 props、只吐 emits。它一旦能直接读 state 或自己发请求，
    // 「这个组件在什么情况下会重画」就再也读不出来了——旧页那六处
    // 「这里绝对不能重画」的特例注释就是这么长出来的。
    match: (f) => f.startsWith('components/'),
    forbid: ['api/client', 'state/'],
  },
];

/**
 * 去掉注释，只留可执行/可渲染的部分。
 *
 * 覆盖三种写法：`/* *\/` 块注释（含 `/** *\/` 文档注释）、`<!-- -->`（.vue 模板）、
 * 以及整行的 `//`。行内 `//` 不剥——URL 里的 `//` 会被误伤，而那正是
 * 「外部主机」那条禁令要抓的东西。
 */
function stripComments(text) {
  return text
    .replace(/<!--[\s\S]*?-->/g, '')
    .replace(/\/\*[\s\S]*?\*\//g, '')
    .split('\n')
    .map((line) => (/^\s*\/\//.test(line) ? '' : line))
    .join('\n');
}

/** 从一行里抠出 import/export-from 的模块说明符。 */
function moduleSpecifiers(text) {
  const specs = [];
  const patterns = [
    /\bimport\s+[^'"]*?from\s*['"]([^'"]+)['"]/g,
    /\bimport\s*['"]([^'"]+)['"]/g,
    /\bexport\s+[^'"]*?from\s*['"]([^'"]+)['"]/g,
  ];
  for (const re of patterns) {
    let m;
    while ((m = re.exec(text)) !== null) specs.push(m[1]);
  }
  return specs;
}

/** 把说明符归一成相对 src/ 的路径；外部包原样返回。 */
function resolveSpec(spec, fromFile) {
  if (spec.startsWith('@/')) return spec.slice(2);
  if (!spec.startsWith('.')) return spec;
  const fromDir = dirname(fromFile);
  return resolve('/', fromDir, spec).slice(1);
}

const files = walk(srcRoot);

for (const file of files) {
  const text = readFileSync(join(srcRoot, file), 'utf8');
  const layer = LAYERS.find((l) => l.match(file));
  if (layer) {
    for (const spec of moduleSpecifiers(text)) {
      const target = resolveSpec(spec, file);
      for (const forbidden of layer.forbid) {
        const hit = forbidden.endsWith('/')
          ? target.startsWith(forbidden)
          : target === forbidden;
        if (hit) {
          fail(file, `${layer.name}/ 不许 import ${spec}（§4.3 分层规则）`);
        }
      }
    }
  }

  // —— §6.2 全局禁令 ——
  //
  // 先把注释剥掉再扫。禁令的**理由**必须能写在注释里——
  // 「这里不许用 v-html，因为渲染数据来自网络」这句话本身含 `v-html`，
  // 扫原文会把解释规则的注释判成违反规则，于是这套规则就没人敢写文档了。
  const code = stripComments(text);
  const bans = [
    // 2. 网络来的字符串（主机名、网卡名、错误串）一律当不可信，只走插值转义。
    [/\bv-html\b/, 'v-html 全局禁用：渲染数据来自网络，只能走插值转义（§3.4）'],
    // 3. CSP 里没有 unsafe-eval，而且不许加。
    [/(?<![.\w$])eval\s*\(/, 'eval( 会撞 CSP（没有 unsafe-eval，且不许加）'],
    [/new\s+Function\s*\(/, 'new Function( 会撞 CSP'],
    [/(?<![.\w$])import\s*\(/, '动态 import() 会被打成独立 chunk，产物必须单文件'],
    // 4. 模态框会阻塞整个页面，且在测试机上是不可关掉的干扰。
    [/\bwindow\.prompt\s*\(/, 'window.prompt 全局禁用（§3.5），用就地编辑器'],
    [/\bwindow\.confirm\s*\(/, 'window.confirm 全局禁用（§3.5）'],
    [/\bwindow\.alert\s*\(/, 'window.alert 全局禁用（§3.5）'],
    [/(?<![.\w$])prompt\s*\(/, '裸 prompt( 全局禁用（§3.5）'],
    [/(?<![.\w$])confirm\s*\(/, '裸 confirm( 全局禁用（§3.5）'],
    [/(?<![.\w$])alert\s*\(/, '裸 alert( 全局禁用（§3.5）'],
    // 5. 跑测试时机器正在灌线速：界面零持续动画，轮询不许堆叠。
    [/\bsetInterval\s*\(/, 'setInterval 会在机器忙时堆叠请求，用 setTimeout 链（§3.3）'],
    [/@keyframes\b/, '@keyframes：界面零持续动画（§3.3）'],
    [/backdrop-filter\s*:/, 'backdrop-filter 在灌线速的机器上很贵（§3.3）'],
    // 6. 运行期离线：任何外部资源都拿不到，且会撞 CSP。
    [/@font-face\b/, '@font-face：产物必须自包含，不能有外部字体（§3.2）'],
    [/url\(\s*['"]?https?:/i, 'CSS 里有外部 url()（§3.2）'],
    [/\/\/fonts\./, '外部字体主机（§3.2）'],
  ];
  for (const [re, message] of bans) {
    const hit = code.match(re);
    if (hit) fail(file, `${message}（命中 ${JSON.stringify(hit[0])}）`);
  }
}

// —— §6.2 第 7 条：文件名必须全 ASCII ——
// 溯源戳按路径的 UTF-8 字节序排序，两端要逐字一致。全 ASCII 时 Node 的默认
// sort 和 Rust 的 String 序才必然相同；混进中文文件名两边就可能排出不同顺序，
// 戳对不上却查不出原因。
for (const file of files) {
  // eslint-disable-next-line no-control-regex
  if (/[^\x20-\x7e]/.test(file)) {
    fail(file, '文件名必须全 ASCII（溯源戳要求跨语言排序一致，§6.2 第 7 条）');
  }
}

if (failures.length) {
  console.error('[lint-arch] 分层规则或全局禁令被破坏：');
  for (const line of failures) console.error(`  - ${line}`);
  process.exit(1);
}
console.log(`[lint-arch] ${files.length} 个文件，分层规则与全局禁令全部通过。`);

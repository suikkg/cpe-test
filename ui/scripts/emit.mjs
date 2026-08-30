// 把 Vite 产物落到 src/master/webui.html，并在落盘前把不变量检查一遍。
//
// 这个脚本是前端构建链的闸门。它挡的三件事都出过或差点出过事故：
//   1. 外链子资源 —— 控制台鉴权在路由之前，浏览器不给 <script src> 带自定义头，
//      外链一律 401。产物必须自包含。
//   2. eval / new Function —— CSP 里没有 'unsafe-eval'，而且不许加。
//      Vue 完整版（含运行期模板编译器）会引入它，走 SFC 预编译则不会；
//      这条检查就是防止有人不小心把 alias 指回完整版。
//   3. 体积失控 —— 产物要 include_str! 进 exe，顺手挡住误开 sourcemap。
//   4. 产物陈旧 —— 上面三条**都防不住**「改了 ui/src 忘了重新构建」：陈旧的产物
//      同样没有外链、没有 eval、体积也正常，测试全绿而用户拿到的是上个版本的
//      界面。所以要往产物里写一枚源码树的溯源戳，由 Rust 侧的
//      `the_embedded_page_was_built_from_the_current_ui_sources` 重算比对——
//      它只比源码，不比产物字节，所以不受 esbuild 版本和构建机差异影响，
//      也不要求 CI 装 Node。
//
// 用法：
//   node scripts/emit.mjs           构建后落盘
//   node scripts/emit.mjs --check   只校验，产物与仓库不一致则退出码 1（CI 用）
import { readFileSync, writeFileSync, existsSync, readdirSync, statSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { dirname, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const built = resolve(here, '..', 'dist', 'index.html');
const target = resolve(here, '..', '..', 'src', 'master', 'webui.html');
const checkOnly = process.argv.includes('--check');

// 1.5 MiB。当前手写页 ~120 KiB，Vue runtime 压缩后 ~110 KiB，留足余量的同时
// 能挡住内联 sourcemap（那会到 3 MiB 以上）。
const SIZE_CEILING = 1.5 * 1024 * 1024;

if (!existsSync(built)) {
  console.error(`[emit] 找不到构建产物 ${built}，先跑 npm run build`);
  process.exit(1);
}
// ---- 溯源戳 ----
//
// 算法两端必须**逐字一致**（另一端是 src/master/webui/tests.rs 里那条测试），
// 所以每一步都写死在这里，改动要同步改 Rust 侧和 .ai/PLAN-v5.0-frontend.md §6.3：
//
//   1. 收集文件：下面 STAMP_ROOTS 里的固定几份 + src/ 下全部文件（递归）。
//   2. 路径取相对 ui/ 的 POSIX 形式，按 UTF-8 字节升序排序。
//      （全 ASCII 由 lint-arch.mjs 第 7 条保证，所以 Node 的默认 sort 和
//      Rust 的 String 序一致。）
//   3. 内容做 CRLF -> LF 归一。Windows 检出必然带 CRLF，不归一两端必然对不上。
//   4. 拼 path + "\n" + content + "\n"，整体取 MD5，小写十六进制。
//
// 有意**不**收进来的：scripts/（构建闸门自己不进产物）、vitest.config.ts
// （只影响测试，不影响产物）、node_modules 与 dist。收进来只会让戳在与产物
// 无关的改动上变化，把「产物陈旧」这个信号淹掉。
const STAMP_ROOTS = [
  'index.html',
  'package.json',
  'package-lock.json',
  'tsconfig.json',
  'vite.config.ts',
];
const uiRoot = resolve(here, '..');
const STAMP_MARKER = 'cpe-ui-stamp: ';

/**
 * 测试专用文件**不进溯源戳**：它们进不了产物。
 *
 * `*.test.ts` 不会被 `main.ts` 引用到，vite 也就不会打包它们；
 * `__fixtures__/` 更是由 Rust 侧的契约测试生成的（`cargo test` 会重写它们）。
 * 把这些收进戳里会造成两种日常噪声：改一个单测就说产物"陈旧"、跑一次
 * `cargo test` 就让产物"陈旧"。戳要回答的是**产物是不是从当前源码来的**，
 * 而这些文件不构成产物的源码。
 *
 * **改这个正则要同步改 Rust 侧的 `is_test_only`**（`src/master/webui/tests.rs`
 * 里 `the_embedded_page_was_built_from_the_current_ui_sources` 内）。两边只要
 * 有一边多认/少认一个后缀，一份刚构建好的产物就会被那条测试判成"陈旧"——
 * 戳本身就是防两份实现漂开的，它自己更不能漂。
 */
function isTestOnly(rel) {
  return rel.includes('__fixtures__/') || /\.(test|spec)\.[cm]?[jt]sx?$/.test(rel);
}

function walkFiles(dir) {
  const out = [];
  for (const entry of readdirSync(dir).sort()) {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) {
      out.push(...walkFiles(full));
      continue;
    }
    const rel = relative(uiRoot, full).split(sep).join('/');
    if (!isTestOnly(rel)) out.push(rel);
  }
  return out;
}

function sourceStamp() {
  const files = [...STAMP_ROOTS, ...walkFiles(join(uiRoot, 'src'))];
  files.sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
  const hash = createHash('md5');
  for (const rel of files) {
    const content = readFileSync(join(uiRoot, rel), 'utf8').replace(/\r\n/g, '\n');
    hash.update(`${rel}\n${content}\n`, 'utf8');
  }
  return hash.digest('hex');
}

const stamp = sourceStamp();
// 戳放在最后一行。Vite 的产物结尾是 </html>，追加一行注释不影响解析。
const html = `${readFileSync(built, 'utf8').replace(/\s*$/, '')}\n<!-- ${STAMP_MARKER}${stamp} -->\n`;

const failures = [];

// —— 1. 零外部引用 ——
// 只认协议相对和绝对 URL；data: 是允许的（CSP 里 img-src 放行了 data:）。
const externalPatterns = [
  [/<script\b[^>]*\bsrc\s*=/i, '产物里有 <script src=...>，会被鉴权挡成 401'],
  [/<link\b[^>]*\brel\s*=\s*["']?(stylesheet|modulepreload|preload)/i, '产物里有 <link rel=stylesheet/preload>，同上'],
  [/\b(?:src|href)\s*=\s*["'](?:https?:)?\/\//i, '产物里有指向外部主机的 src/href'],
  [/@import\s+(?:url\()?["']?(?:https?:)?\/\//i, 'CSS 里有外部 @import'],
];
for (const [re, message] of externalPatterns) {
  const hit = html.match(re);
  if (hit) failures.push(`${message}（命中：${JSON.stringify(hit[0].slice(0, 80))}）`);
}

// —— 2. 无 eval / new Function ——
for (const [re, message] of [
  [/(?<![.\w$])eval\s*\(/, '产物里有 eval(，CSP 没有 unsafe-eval'],
  [/new\s+Function\s*\(/, '产物里有 new Function(，CSP 没有 unsafe-eval'],
]) {
  const hit = html.match(re);
  if (hit) {
    const at = html.indexOf(hit[0]);
    failures.push(`${message}（上下文：${JSON.stringify(html.slice(Math.max(0, at - 60), at + 60))}）`);
  }
}

// —— 3. 挂载点与体积 ——
if (!/id\s*=\s*["']app["']/.test(html)) failures.push('产物里找不到 id="app" 挂载点');
if ((html.match(new RegExp(STAMP_MARKER, 'g')) || []).length !== 1) {
  failures.push('产物里的溯源戳不是恰好一枚');
}
if (Buffer.byteLength(html, 'utf8') > SIZE_CEILING) {
  failures.push(`产物 ${Buffer.byteLength(html, 'utf8')} 字节，超过上限 ${SIZE_CEILING}`);
}

if (failures.length) {
  console.error('[emit] 产物不满足控制台的不变量：');
  for (const line of failures) console.error(`  - ${line}`);
  process.exit(1);
}

const size = Buffer.byteLength(html, 'utf8');
if (checkOnly) {
  const current = existsSync(target) ? readFileSync(target, 'utf8') : '';
  if (current !== html) {
    console.error('[emit] 前端产物与仓库里的 src/master/webui.html 不同步。');
    console.error('       在 ui/ 下跑 npm ci && npm run build，并把产物一起提交。');
    process.exit(1);
  }
  console.log(`[emit] 产物与仓库一致（${size} 字节，戳 ${stamp}），不变量全部通过。`);
} else {
  writeFileSync(target, html, 'utf8');
  console.log(`[emit] 已写入 ${target}（${size} 字节，戳 ${stamp}），不变量全部通过。`);
}

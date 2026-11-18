#!/usr/bin/env node
// Offline inventory. No registry queries, installation, or host-platform filtering.
import {spawnSync} from 'node:child_process';
import {readFileSync, writeFileSync, mkdtempSync, openSync, closeSync, rmSync} from 'node:fs';
import {dirname, resolve, relative} from 'node:path';
import {fileURLToPath} from 'node:url';
import {tmpdir} from 'node:os';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const check = process.argv.includes('--check');
if (process.argv.slice(2).some(arg => arg !== '--check')) throw new Error('Usage: node scripts/third-party.mjs [--check]');
const compare = (a, b) => a < b ? -1 : a > b ? 1 : 0;
const manifests = ['Cargo.toml'];
const npmRoots = [];
const common = new Set(['MIT', 'MIT-0', '0BSD', 'Apache-2.0', 'BSD-2-Clause', 'BSD-3-Clause', 'ISC', 'Zlib', 'BSL-1.0', 'CC0-1.0', 'Unlicense', 'Unicode-3.0']);
function unusualTokens(license) {
  const tokens = license.replace(/[()]/g, ' ').split(/[\s/]+/).filter(Boolean);
  return [...new Set(tokens.filter(t => t !== 'OR' && t !== 'AND' && t !== 'WITH' && !common.has(t)))];
}
// Every license outside the common permissive set needs one of these reviewed explanations;
// an unexplained one fails generation (and CI's --check) instead of entering the list silently.
const MPL = 'MPL-2.0，未修改';
const CHOICE = '多许可，选用 MIT 或 Apache-2.0';
const BUILD = '仅构建工具，不分发';
function note(row) {
  if (!row.license || /^(unknown|unlicensed)$/i.test(row.license)) return null;
  const unusual = unusualTokens(row.license);
  if (!unusual.length) return '';
  if (row.ecosystem === 'npm' && ![...row.scopes].some(s => s.endsWith('(生产)'))) return BUILD;
  if (/(^|\sOR\s)\(?(MIT|Apache-2\.0)\)?(\s|$)/.test(row.license) && /\sOR\s/.test(row.license)) return CHOICE;
  if (unusual.every(t => t === 'MPL-2.0')) return MPL;
  return null;
}
const cargo = new Map();
const npm = new Map();
function record(map, row) {
  const key = JSON.stringify([row.name, row.version, row.license, row.source]);
  const previous = map.get(key);
  if (previous) for (const scope of row.scopes) previous.scopes.add(scope);
  else map.set(key, row);
}
for (const manifest of manifests) {
  // File-backed stdout also works in sandboxes that disallow the socketpair used
  // by Node's synchronous child-process pipe implementation.
  const temporary = mkdtempSync(resolve(tmpdir(), 'mpmp-third-party-'));
  let metadata;
  const output = resolve(temporary, 'metadata.json');
  const fd = openSync(output, 'w');
  try {
    const result = spawnSync('cargo', ['metadata', '--offline', '--locked', '--format-version', '1', '--all-features', '--manifest-path', manifest], {
      cwd: root, stdio: ['ignore', fd, 'inherit'], timeout: 180_000,
    });
    if (result.error || result.status !== 0) throw new Error(`Cargo metadata failed for ${manifest}: ${result.error?.code ?? result.status}`);
    metadata = JSON.parse(readFileSync(output, 'utf8'));
  } finally {
    closeSync(fd);
    rmSync(temporary, {recursive: true, force: true});
  }
  const packages = new Map(metadata.packages.map(p => [p.id, p]));
  const nodes = new Map(metadata.resolve.nodes.map(n => [n.id, n]));
  const seen = new Set();
  const pending = [...metadata.workspace_members];
  while (pending.length) {
    const id = pending.pop();
    if (seen.has(id)) continue;
    seen.add(id);
    const pkg = packages.get(id);
    if (!pkg) throw new Error(`Missing Cargo package: ${id}`);
    if (pkg.source) record(cargo, {name: pkg.name, version: pkg.version, license: pkg.license || 'unknown', source: pkg.source, scopes: new Set([manifest])});
    else if (!resolve(pkg.manifest_path).startsWith(root + '/')) throw new Error(`External path dependency needs a source declaration: ${pkg.name}`);
    // Do not filter dep_kinds.target: retain every platform. Optional dependencies are
    // activated by --all-features; build and dev edges are deliberately excluded.
    for (const dep of nodes.get(id)?.deps ?? []) {
      if (dep.dep_kinds.some(k => k.kind === null || k.kind === 'normal')) pending.push(dep.pkg);
    }
  }
}
for (const prefix of npmRoots) {
  const lock = JSON.parse(readFileSync(resolve(root, prefix, 'package-lock.json'), 'utf8'));
  if (lock.lockfileVersion !== 3) throw new Error(`Expected lock v3: ${prefix}`);
  for (const [location, pkg] of Object.entries(lock.packages)) {
    if (!location) continue;
    if (pkg.link) throw new Error(`Linked npm package needs explicit handling: ${prefix}/${location}`);
    const name = pkg.name || location.split('node_modules/').at(-1);
    if (!pkg.version || !name) throw new Error(`Incomplete npm package: ${prefix}/${location}`);
    record(npm, {name, version: pkg.version, license: pkg.license || 'unknown', source: pkg.resolved || 'unknown', scopes: new Set([`${prefix} (${pkg.dev === true ? '仅构建工具' : '生产'})`])});
  }
}
const sorted = map => [...map.values()].sort((a, b) => compare(a.name, b.name) || compare(a.version, b.version) || compare(a.source, b.source));
const rustRows = sorted(cargo), npmRows = sorted(npm);
const production = npmRows.filter(r => [...r.scopes].some(s => s.endsWith('(生产)')));
const buildOnly = npmRows.filter(r => !production.includes(r));
const all = [...rustRows.map(r => ({...r, ecosystem: 'Cargo'})), ...npmRows.map(r => ({...r, ecosystem: 'npm'}))];
const unexplained = all.filter(r => note(r) === null);
if (unexplained.length)
  throw new Error(`Licenses without a reviewed explanation: ${unexplained.map(r => `${r.name}@${r.version} (${r.license})`).join(', ')}`);
const noted = all.filter(r => note(r));
const cell = value => String(value).replace(/\|/g, '&#124;').replace(/[\r\n]+/g, ' ');
function table(rows, ecosystem = false) {
  const headers = [...(ecosystem ? ['生态'] : []), 'name', 'version', 'license 表达式', '来源', '范围', ...(ecosystem ? ['说明'] : [])];
  return `| ${headers.join(' | ')} |\n|${headers.map(() => '---').join('|')}|\n` + rows.map(r => `| ${[...(ecosystem ? [r.ecosystem] : []), r.name, r.version, r.license, r.source, [...r.scopes].sort(compare).join('; '), ...(ecosystem ? [note(r)] : [])].map(cell).join(' | ')} |`).join('\n') + '\n';
}
const output = `# 第三方依赖许可清单

由 \`node scripts/third-party.mjs\` 离线生成；核对：\`node scripts/third-party.mjs --check\`。
数据来自本仓库锁文件和本地 Cargo 缓存，不联网核验许可证全文；字段仅是上游元数据声明，不是分发许可结论。

- Cargo：本仓库 workspace，\`cargo metadata --offline --locked --format-version 1 --all-features\`，从各 workspace 成员递归遍历 normal 边，包含所有 target 条件（未传 --filter-platform），排除 build/dev 边及本仓库自有 path 包。仅有 build/dev 路径可达的 Rust 工具不在本表内。
- npm：本仓库没有 npm 包。
- 说明规则：常见宽松许可（${[...common].join(', ')}）无需说明；其他许可都在“需要说明的许可”中逐项给出已审阅的说明。出现没有说明的许可时，生成与 CI 检查直接失败。
- 来源为锁文件 tarball URL 或 Cargo source ID，不读取本机绝对路径。FFmpeg 是外部夹具生成工具，仓库不分发其可执行文件，不在依赖表中。

统计（按上述合并键）：Cargo normal **${rustRows.length}**；npm 生产 **${production.length}**；npm 仅构建工具 **${buildOnly.length}**；需要说明 **${noted.length}**。

## 需要说明的许可

- **MPL-2.0，未修改**：MPL-2.0 是文件级弱 copyleft。本项目未修改这些文件，源码可从上游获取，与本项目的 MIT OR Apache-2.0 兼容；若将来修改其中文件，修改后的文件须继续以 MPL-2.0 提供源码。
- **多许可，选用 MIT 或 Apache-2.0**：上游允许在几种许可中任选，本项目选用其中的 MIT 或 Apache-2.0。
- **仅构建工具，不分发**：只在构建时使用，不进入发布的程序或主题。

${table(noted, true)}
## Cargo normal 依赖（所有平台）

${table(rustRows)}
## npm 生产依赖

${production.length ? table(production) : '本仓库没有 npm 包。\n'}
## npm 仅构建工具

${table(buildOnly)}`;
const destination = resolve(root, 'docs/THIRD-PARTY.md');
if (check) {
  if (readFileSync(destination, 'utf8') !== output) throw new Error('docs/THIRD-PARTY.md is stale; run node scripts/third-party.mjs');
} else writeFileSync(destination, output);
console.log(`${relative(root, destination)}: Cargo ${rustRows.length}, npm production ${production.length}, npm build ${buildOnly.length}, noted ${noted.length}${check ? ' (up to date)' : ''}`);

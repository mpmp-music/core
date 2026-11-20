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
const MPL = 'MPL-2.0, unmodified';
const CHOICE = 'Multiple licenses, MIT or Apache-2.0 selected';
const BUILD = 'Build tool only, not distributed';
function note(row) {
  if (!row.license || /^(unknown|unlicensed)$/i.test(row.license)) return null;
  const unusual = unusualTokens(row.license);
  if (!unusual.length) return '';
  if (row.ecosystem === 'npm' && ![...row.scopes].some(s => s.endsWith('(production)'))) return BUILD;
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
    record(npm, {name, version: pkg.version, license: pkg.license || 'unknown', source: pkg.resolved || 'unknown', scopes: new Set([`${prefix} (${pkg.dev === true ? 'build only' : 'production'})`])});
  }
}
const sorted = map => [...map.values()].sort((a, b) => compare(a.name, b.name) || compare(a.version, b.version) || compare(a.source, b.source));
const rustRows = sorted(cargo), npmRows = sorted(npm);
const production = npmRows.filter(r => [...r.scopes].some(s => s.endsWith('(production)')));
const buildOnly = npmRows.filter(r => !production.includes(r));
const all = [...rustRows.map(r => ({...r, ecosystem: 'Cargo'})), ...npmRows.map(r => ({...r, ecosystem: 'npm'}))];
const unexplained = all.filter(r => note(r) === null);
if (unexplained.length)
  throw new Error(`Licenses without a reviewed explanation: ${unexplained.map(r => `${r.name}@${r.version} (${r.license})`).join(', ')}`);
const noted = all.filter(r => note(r));
const cell = value => String(value).replace(/\|/g, '&#124;').replace(/[\r\n]+/g, ' ');
function table(rows, ecosystem = false) {
  const headers = [...(ecosystem ? ['Ecosystem'] : []), 'Name', 'Version', 'License expression', 'Source', 'Scope', ...(ecosystem ? ['Explanation'] : [])];
  return `| ${headers.join(' | ')} |\n|${headers.map(() => '---').join('|')}|\n` + rows.map(r => `| ${[...(ecosystem ? [r.ecosystem] : []), r.name, r.version, r.license, r.source, [...r.scopes].sort(compare).join('; '), ...(ecosystem ? [note(r)] : [])].map(cell).join(' | ')} |`).join('\n') + '\n';
}
const npmCoverage = npmRoots.length
  ? 'Includes every package entry in the three v3 package-lock.json files, including platform-specific optional packages. Packages are grouped by the dev flag and merged by name, version, license, and source, with scopes retained. Build-only classification follows the lockfiles. Svelte, Lucide, and others may still be bundled in the theme. Carry their notices with the built theme. See the [default theme notices](../apps/desktop/theme-default/notices/third-party-notices.txt).'
  : 'This repository has no npm packages.';
const noProduction = npmRoots.length
  ? 'All three lockfiles mark dependencies as dev. There are no production entries.'
  : npmCoverage;
const output = `# Third-party dependency licenses

Generate offline with \`node scripts/third-party.mjs\` and verify with \`node scripts/third-party.mjs --check\`.
The inventory uses the repository lockfiles and local Cargo cache without checking full license texts online. Fields report upstream metadata and do not determine whether distribution is permitted.

- **Cargo.** Reads ${manifests.length === 1 ? 'this repository’s workspace manifest' : 'three manifests'} with \`cargo metadata --offline --locked --format-version 1 --all-features\`. Traverses normal dependency edges recursively from each workspace member across all targets, without \`--filter-platform\`. Excludes build and dev edges and the repository's own path packages. Rust tools reachable only through build or dev edges are outside the inventory.
- **npm.** ${npmCoverage}
- **Review.** Common permissive licenses (${[...common].join(', ')}) require no explanation. Each other license has a reviewed explanation under “Licenses requiring explanation.” Generation and CI checks fail if an explanation is missing.
- **Sources.** Sources are lockfile tarball URLs or Cargo source IDs. The inventory does not read local absolute paths. FFmpeg is an external fixture generation tool. The repository does not distribute its executable, so it is outside the inventory.

Totals after merging are **${rustRows.length}** Cargo normal, **${production.length}** npm production, **${buildOnly.length}** npm build only, and **${noted.length}** requiring explanation.

## Licenses requiring explanation

- **${MPL}.** MPL-2.0 is a file-level weak copyleft license. This project leaves these files unmodified. Their source is available upstream, and their license is compatible with this project's MIT OR Apache-2.0. Any future modifications to these files must continue to be made available in source form under MPL-2.0.
- **${CHOICE}.** Upstream offers a choice of licenses. This project selects MIT or Apache-2.0.
- **${BUILD}.** Used only during the build and excluded from the distributed application and theme.

${table(noted, true)}
## Cargo normal dependencies (all platforms)

${table(rustRows)}
## npm production dependencies

${production.length ? table(production) : noProduction + '\n'}
## npm build-only dependencies

${table(buildOnly)}`;
const destination = resolve(root, 'docs/THIRD-PARTY.md');
if (check) {
  if (readFileSync(destination, 'utf8') !== output) throw new Error('docs/THIRD-PARTY.md is stale; run node scripts/third-party.mjs');
} else writeFileSync(destination, output);
console.log(`${relative(root, destination)}: Cargo ${rustRows.length}, npm production ${production.length}, npm build ${buildOnly.length}, noted ${noted.length}${check ? ' (up to date)' : ''}`);

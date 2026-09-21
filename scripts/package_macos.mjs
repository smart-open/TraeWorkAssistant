#!/usr/bin/env node
/** 把 macOS release 构建产物归档到根目录 release/（对齐 scripts/package_portable.mjs 的 Windows 流程）：
 *   AI Work 助手_<version>_<arch>.dmg            ← 安装包（Tauri bundle 产物原样复制）
 *   AI Work 助手_<version>_<arch>_portable.zip   ← 便携版（.app 用 ditto 压缩，保留 ad-hoc 签名与扩展属性）
 */
import { copyFileSync, existsSync, mkdirSync, readdirSync, readFileSync, statSync, rmSync } from 'node:fs';
import { basename, dirname, join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const SRC_TAURI = resolve(ROOT, 'src-tauri');
const CONF = resolve(SRC_TAURI, 'tauri.conf.json');
const OUT_DIR = resolve(ROOT, 'release');

const DMG_DIR = resolve(SRC_TAURI, 'target/release/bundle/dmg');
const APP_DIR = resolve(SRC_TAURI, 'target/release/bundle/macos');

const die = (msg) => {
  console.error(msg);
  process.exit(1);
};

const conf = JSON.parse(readFileSync(CONF, 'utf8'));
const product = conf.productName;
// 版本单源 = Cargo.toml；tauri.conf.json 里的 version 字段已移除（自动回读 Cargo.toml）
let version = conf.version;
if (!version) {
  const m = readFileSync(resolve(SRC_TAURI, 'Cargo.toml'), 'utf8')
    .match(/^version\s*=\s*"(\d+\.\d+\.\d+)"/m);
  if (!m) die('ERROR: 无法从 Cargo.toml 读取版本号');
  version = m[1];
}

/** 取目录下最新修改的匹配文件/目录 */
function latest(dir, filter) {
  if (!existsSync(dir)) return null;
  const ents = readdirSync(dir, { withFileTypes: true })
    .filter((e) => filter(e.name))
    .map((e) => join(dir, e.name))
    .filter((p) => statSync(p).isFile() || statSync(p).isDirectory());
  if (ents.length === 0) return null;
  return ents.sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs)[0];
}

// 1) 安装包 dmg：从文件名提取 arch（x64 / aarch64 / universal）
const dmg = latest(DMG_DIR, (n) => n.endsWith('.dmg'));
if (!dmg) die(`ERROR: 未找到 dmg 产物，请先执行 tauri build（期望目录: ${DMG_DIR}）`);
const arch = (basename(dmg).match(/_(aarch64|x64|universal)\.dmg$/)?.[1]) ?? 'x64';

mkdirSync(OUT_DIR, { recursive: true });
const dmgOut = join(OUT_DIR, `${product}_${version}_${arch}.dmg`);
copyFileSync(dmg, dmgOut);
console.log(`OK: ${dmgOut}  (${(statSync(dmgOut).size / 1024 / 1024).toFixed(2)} MB)`);

// 2) 便携版 zip：ditto 压缩 .app（必须用 ditto，保留代码签名元数据与扩展属性，解压后可直接运行）
const app = latest(APP_DIR, (n) => n.endsWith('.app'));
if (!app) die(`ERROR: 未找到 .app 产物（期望目录: ${APP_DIR}）`);
const zipOut = join(OUT_DIR, `${product}_${version}_${arch}_portable.zip`);
rmSync(zipOut, { force: true });
const r = spawnSync('ditto', ['-c', '-k', '--sequesterRsrc', '--keepParent', app, zipOut], {
  stdio: 'inherit',
});
if (r.status !== 0) die('ERROR: ditto 打包 zip 失败');
console.log(`OK: ${zipOut}  (${(statSync(zipOut).size / 1024 / 1024).toFixed(2)} MB)`);

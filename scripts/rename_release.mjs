#!/usr/bin/env node
/** 打包产物统一命名：把 `npm run tauri build` 产出的安装包复制到 release/，
 * 统一使用中文产品名命名（APP 名称标识）。
 *
 * 用法：node scripts/rename_release.mjs [--strict]
 *   --strict  任一产物缺失时以非零码退出（默认仅告警）
 *   环境变量 RELEASE_OUT_DIR：输出目录覆盖（默认 release/；CI 分别落 release/mac、release/windows）
 *   src-tauri/target/release/bundle/nsis/AI Work 助手_<ver>_x64-setup.exe
 *       → release/AI Work 助手_<ver>_x64-setup.exe
 *   src-tauri/target/release/bundle/msi/AI Work 助手_<ver>_x64_zh-CN.msi
 *       → release/AI Work 助手_<ver>_x64_zh-CN.msi
 *   src-tauri/target/release/bundle/dmg/AI Work 助手_<ver>_aarch64.dmg
 *       → release/AI Work 助手_<ver>_aarch64.dmg（F-75 M3-3.4）
 *   CI 显式 --target <triple> 构建落 target/<triple>/release/，dmg 扫描同样兼容（策略 A）。
 *   portable zip 由 package_portable.mjs 直接生成同名（无需重命名）。
 *
 * 产物完备性判定：Windows 上 exe/msi 缺失即 die（dmg 仅 WARN）；
 * macOS 上按 TAURI_TARGET 判定必需 dmg（universal-apple-darwin → _universal.dmg /
 * x86_64-apple-darwin → _x64.dmg / aarch64 或未设 → 按本机架构），缺失即 die
 * （反之 exe/msi 在 mac 上缺失仅 WARN）。
 */
import { createHash } from 'node:crypto';
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  writeFileSync,
} from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const SRC_TAURI = resolve(ROOT, 'src-tauri');

// 发布校验清单文件名（作为 Release 资产上传，更新器下载安装包后校验完整性，
// updater.rs fail-closed：清单缺失/损坏/版本不符/未收录资产均阻止自动更新）
const MANIFEST_NAME = 'latest.json';

const die = (msg) => {
  console.error(msg);
  process.exit(1);
};

function readVersion(conf) {
  // 版本单源 = Cargo.toml；tauri.conf.json 里的 version 字段已移除（自动回读 Cargo.toml）
  if (conf.version) return conf.version;
  const m = readFileSync(resolve(SRC_TAURI, 'Cargo.toml'), 'utf8').match(
    /^version\s*=\s*"(\d+\.\d+\.\d+)"/m,
  );
  if (!m) die('ERROR: 无法从 Cargo.toml 读取版本号');
  return m[1];
}

const conf = JSON.parse(readFileSync(resolve(SRC_TAURI, 'tauri.conf.json'), 'utf8'));
const version = readVersion(conf);
const product = conf.productName;
const outDir = resolve(ROOT, process.env.RELEASE_OUT_DIR || 'release');
mkdirSync(outDir, { recursive: true });

// dmg 产物源目录候选：本地原生构建落 target/release/；CI 显式 --target <triple> 落
// target/<triple>/release/（build-macos.yml 策略 A：arm64 runner 打 aarch64 / Intel runner 打 x64）
const dmgSrcDirs = [
  resolve(SRC_TAURI, 'target/release/bundle/dmg'),
  resolve(SRC_TAURI, 'target/aarch64-apple-darwin/release/bundle/dmg'),
  resolve(SRC_TAURI, 'target/x86_64-apple-darwin/release/bundle/dmg'),
];

// 每个 job 的源为候选路径数组：取第一个存在者（单一固定路径时数组长度为 1）
const jobs = [
  [
    [resolve(SRC_TAURI, 'target/release/bundle/nsis', `${product}_${version}_x64-setup.exe`)],
    resolve(outDir, `${product}_${version}_x64-setup.exe`),
  ],
  [
    [resolve(SRC_TAURI, 'target/release/bundle/msi', `${product}_${version}_x64_zh-CN.msi`)],
    resolve(outDir, `${product}_${version}_x64_zh-CN.msi`),
  ],
  // F-75 审查：mac 双架构 dmg（aarch64 = Apple Silicon / x64 = Intel；universal 为可选形态，
  // rustup target add x86_64-apple-darwin + tauri build --target universal-apple-darwin 产出）
  [
    dmgSrcDirs.map((d) => join(d, `${product}_${version}_aarch64.dmg`)),
    resolve(outDir, `${product}_${version}_aarch64.dmg`),
  ],
  [
    dmgSrcDirs.map((d) => join(d, `${product}_${version}_x64.dmg`)),
    resolve(outDir, `${product}_${version}_x64.dmg`),
  ],
  // F-75 审查：universal 可选形态——--target universal-apple-darwin 的 bundle 落
  // target/universal-apple-darwin/（非 target/release/）；updater 以 _universal.dmg 兜底
  [
    [resolve(SRC_TAURI, 'target/universal-apple-darwin/release/bundle/dmg', `${product}_${version}_universal.dmg`)],
    resolve(outDir, `${product}_${version}_universal.dmg`),
  ],
];

// 本机平台对应的必需产物（缺失即 die）：mac 按 TAURI_TARGET（CI 矩阵显式传入）→ 对应 dmg，
// 未设 env 的本地构建回退本机架构推断；Windows → exe/msi。
// 其余平台产物缺失仅 WARN（双平台构建机分别跑本脚本后人工汇总到 release/）
const isMac = process.platform === 'darwin';
const requiredMacDmg = () => {
  const t = process.env.TAURI_TARGET || '';
  if (t === 'universal-apple-darwin') return 'universal';
  if (t === 'x86_64-apple-darwin') return 'x64';
  if (t === 'aarch64-apple-darwin') return 'aarch64';
  return process.arch === 'x64' ? 'x64' : 'aarch64';
};
const REQUIRED_ON_THIS_HOST = isMac
  ? [`${product}_${version}_${requiredMacDmg()}.dmg`]
  : [
      `${product}_${version}_x64-setup.exe`,
      `${product}_${version}_x64_zh-CN.msi`,
    ];

let moved = 0;
const missing = [];
for (const [srcCandidates, dst] of jobs) {
  const src = srcCandidates.find((p) => existsSync(p));
  if (src) {
    copyFileSync(src, dst);
    console.log('OK:', dst);
    moved++;
  } else {
    console.error('SKIP（不存在）:', srcCandidates[0]);
    missing.push(srcCandidates[0]);
  }
}
if (moved === 0) die('未找到任何安装包产物，请先执行 npm run tauri build');

// 生成发布校验清单 latest.json：版本号 + 各资产 SHA-256。
// 更新器（updater.rs）下载安装包后与清单比对，不匹配即拒绝安装（更新包完整性校验）。
// F-75：清单收录双架构 + universal dmg 哈希（mac updater fail-closed 消费，universal 为
// Intel runner 退役后的兜底资产）；老版本 Windows updater 只找 exe/msi 键，多出的 dmg 键无害。
// 审查修复 #5（跨平台清单合并）：若 release/ 已存在 previous latest.json（另一平台的
// 构建先产出），以其 assets 为基底合并——保证最终上传的清单同时含 exe/msi/dmg 键，
// 否则单平台清单随 Release 上传会让另一平台更新器 fail-closed 阻断。
const manifestPath = join(outDir, MANIFEST_NAME);
let prevAssets = {};
if (existsSync(manifestPath)) {
  try {
    const prev = JSON.parse(readFileSync(manifestPath, 'utf8'));
    // 版本不一致的旧清单不继承（版本已变，哈希全部按当前产物重算）
    if (prev.version === version && prev.assets && typeof prev.assets === 'object') {
      prevAssets = prev.assets;
      console.log('合并既有清单资产:', Object.keys(prevAssets).length, '个');
    }
  } catch {
    console.error('WARN: 既有 latest.json 解析失败，忽略并重建');
  }
}
const assets = { ...prevAssets };
for (const name of [
  `${product}_${version}_x64-setup.exe`,
  `${product}_${version}_x64_zh-CN.msi`,
  `${product}_${version}_aarch64.dmg`,
  `${product}_${version}_x64.dmg`,
  `${product}_${version}_universal.dmg`,
  `${product}_${version}_x64_portable.zip`,
]) {
  const p = join(outDir, name);
  if (!existsSync(p)) {
    if (!REQUIRED_ON_THIS_HOST.includes(name)) {
      // 非本机平台产物（mac 上无 exe/msi、Windows 上无 dmg）与 portable zip：
      // 警告但不阻塞（需人工汇总双平台产物后发布）
      console.error('WARN（清单跳过，文件不存在）:', name);
      continue;
    }
    die(`清单生成失败：产物缺失 ${name}`);
  }
  const h = createHash('sha256').update(readFileSync(p)).digest('hex');
  assets[name] = h;
  console.log(`SHA256 ${h}  ${name}`);
}
// 与原 Python 版输出字节对齐（updater.rs 消费契约）：2 空格缩进、中文原样、无末尾换行
writeFileSync(manifestPath, JSON.stringify({ version, assets }, null, 2));
console.log('OK:', join(outDir, MANIFEST_NAME));
console.error('提示：发布时请将', MANIFEST_NAME, '作为 Release 资产一并上传');

if (missing.length) {
  console.error(`WARNING: 有 ${missing.length} 个产物缺失，上传 release 前请核对清单：`);
  for (const m of missing) console.error('  -', m);
  if (process.argv.includes('--strict')) {
    console.error('--strict：缺失产物视为失败');
    process.exit(1);
  }
}

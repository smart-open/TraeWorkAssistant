#!/usr/bin/env node
/** 把 release 构建产物打包为 portable zip：
 *   AI Work 助手_<version>_x64_portable.zip
 * 内容布局（与 Tauri 安装包一致，exe 直接读取同目录 resources/）：
 *   AI Work 助手/               ← 顶层目录用产品名（APP 显示名称）
 *     ai-work-assistant.exe     ← 主程序名取 tauri.conf.json 的 mainBinaryName
 *     resources/ps/              (来自 src-ps/；python 资源目录已随 Rust 化移除)
 *
 * zip 由 Windows 10+ 自带 tar.exe（bsdtar）按扩展名自动生成 zip 格式
 * （deflate 压缩 + UTF-8 文件名标志，正确支持中文顶层目录名），零第三方依赖。
 */
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  renameSync,
  rmSync,
  statSync,
} from 'node:fs';
import { basename, dirname, join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const SRC_TAURI = resolve(ROOT, 'src-tauri');
const CONF = resolve(SRC_TAURI, 'tauri.conf.json');
const OUT_DIR = resolve(ROOT, 'release');

// 主程序候选：优先 mainBinaryName 命名的产物，兼容旧的 cargo 包名产物
const EXE_CANDIDATES = [
  resolve(SRC_TAURI, 'target/release/ai-work-assistant.exe'),
  resolve(SRC_TAURI, 'target/release/trae-work-assistant.exe'),
];

const SKIP_DIRS = new Set(['__pycache__', '.git']);

const die = (msg) => {
  console.error(msg);
  process.exit(1);
};

/** 递归复制目录（跳过 SKIP_DIRS；保留相对结构，对齐原 walk_copy） */
function walkCopy(src, dst) {
  mkdirSync(dst, { recursive: true });
  const walk = (dir, rel) => {
    for (const ent of readdirSync(dir, { withFileTypes: true })) {
      if (ent.isDirectory()) {
        if (SKIP_DIRS.has(ent.name)) continue;
        walk(join(dir, ent.name), join(rel, ent.name));
      } else {
        const t = join(dst, rel, ent.name);
        mkdirSync(dirname(t), { recursive: true });
        copyFileSync(join(dir, ent.name), t);
      }
    }
  };
  walk(src, '');
}

/** 极简 glob（支持段内 `*` 与整段 `**`，语义对齐 Python glob.glob 的实际用法） */
function globList(pattern) {
  const segs = pattern.replace(/\//g, '\\').split('\\');
  // 最深的无通配符前缀作为遍历起点
  let fixed = 0;
  while (fixed < segs.length && !segs[fixed].includes('*')) fixed++;
  const baseDir = segs.slice(0, fixed).join('\\');
  const re = new RegExp(
    '^' +
      segs
        .map((s) =>
          s === '**'
            ? '(?:.+)'
            : s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&').replace(/\*/g, '[^\\\\]*'),
        )
        .join('\\\\') +
      '$',
  );
  if (!existsSync(baseDir)) return [];
  const out = [];
  const walk = (dir) => {
    for (const ent of readdirSync(dir, { withFileTypes: true })) {
      const p = join(dir, ent.name);
      if (re.test(p)) out.push(p);
      if (ent.isDirectory() && !SKIP_DIRS.has(ent.name)) walk(p);
    }
  };
  walk(baseDir);
  return out;
}

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
const binaryName = conf.mainBinaryName || 'ai-work-assistant';
// resources 形如 {"../src-ps/": "ps/"}（python 资源已随 Rust 化移除）
const resources = conf.bundle?.resources ?? {};

const releaseExe = EXE_CANDIDATES.find((p) => existsSync(p) && statSync(p).isFile());
if (!releaseExe) die(`ERROR: release exe 不存在，已尝试: ${EXE_CANDIDATES.join(', ')}`);

mkdirSync(OUT_DIR, { recursive: true });
// 产物文件名使用中文产品名（如 AI Work 助手_3.0.0_x64_portable.zip）
const zipPath = join(OUT_DIR, `${product}_${version}_x64_portable.zip`);

const tmpRoot = join(OUT_DIR, '_portable_stage');
rmSync(tmpRoot, { recursive: true, force: true });
const stageApp = join(tmpRoot, product);
mkdirSync(stageApp, { recursive: true });

// 1) 主程序按 mainBinaryName 命名放入产品目录
copyFileSync(releaseExe, join(stageApp, `${binaryName}.exe`));

// 2) 资源按 Tauri 布局放入 resources/：
//    resources key 既可为目录（递归复制），也可为 glob 模式（逐文件平铺复制）
const resDir = join(stageApp, 'resources');
for (const [srcRel, destRaw] of Object.entries(resources)) {
  const srcAbs = resolve(SRC_TAURI, srcRel);
  const dest = destRaw.replace(/^[/\\]+|[/\\]+$/g, '');
  if (existsSync(srcAbs) && statSync(srcAbs).isDirectory()) {
    walkCopy(srcAbs, join(resDir, dest));
    continue;
  }
  const matches = globList(srcAbs);
  if (matches.length === 0) {
    console.error('WARN: 资源目录缺失:', srcAbs);
    continue;
  }
  mkdirSync(join(resDir, dest), { recursive: true });
  for (const m of matches) {
    if (statSync(m).isDirectory()) {
      walkCopy(m, join(resDir, dest, basename(m)));
    } else {
      copyFileSync(m, join(resDir, dest, basename(m)));
    }
  }
}

// 3) 打包（保留内部目录结构，顶层为产品名文件夹）：tar -a 按扩展名 .zip 生成 zip
// Windows 显式用 System32 的 bsdtar（CI PATH 里 Git 的 GNU tar 优先且行为不一）。
// tar 的 argv 在 Windows 走 ANSI API：CI runner（en-US，ACP 1252）编不了中文产品名
// （「AI Work 助手」→「??」导致打开失败；本机 ACP 936 无此问题，具有迷惑性）。
// 规避：tar 的输出路径与目录参数全部 ASCII（临时 zip 名 + "."，目录内容由
// libarchive 宽字符遍历保证 zip 内 UTF-8 名正确），打包完用 node rename 回中文名。
const TAR =
  process.platform === 'win32' && existsSync('C:/Windows/System32/tar.exe')
    ? 'C:/Windows/System32/tar.exe'
    : 'tar';
const tmpZip = join(OUT_DIR, '_portable_tmp.zip');
rmSync(tmpZip, { force: true });
console.log('正在打包:', zipPath);
const r = spawnSync(TAR, ['-a', '-cf', tmpZip, '.'], {
  cwd: tmpRoot,
  stdio: 'inherit',
});
rmSync(tmpRoot, { recursive: true, force: true });
if (r.status !== 0) {
  rmSync(tmpZip, { force: true });
  die('ERROR: tar 打包 zip 失败');
}
renameSync(tmpZip, zipPath);
const size = statSync(zipPath).size;
console.log(`OK: ${zipPath}  (${(size / 1024 / 1024).toFixed(2)} MB)`);

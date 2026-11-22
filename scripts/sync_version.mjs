#!/usr/bin/env node
/** 版本号单源同步工具（单一来源：src-tauri/Cargo.toml）
 *
 * 用法：
 *   node scripts/sync_version.mjs              # 读取 Cargo.toml 版本，同步其余各处
 *   node scripts/sync_version.mjs 3.2.0        # 先把 Cargo.toml 改为目标版本，再同步其余各处
 *
 * 同步点（历史遗留的 6 处已收敛为 1 处 + 本脚本自动同步）：
 *   - src-tauri/Cargo.toml           单一来源（手动/参数指定）
 *   - src-tauri/Cargo.lock           自动（cargo update -p）
 *   - src-tauri/tauri.conf.json      已移除 version 字段（自动回读 Cargo.toml）
 *   - src/lib/about.ts               已移除 APP_VERSION 硬编码（运行时 getVersion()）
 *   - package.json                   本脚本写入
 *   - package-lock.json              本脚本写入（根级 + packages[""] 两处，3.9.4/3.9.5 曾遗漏）
 *   - AGENT.md 标题                  本脚本写入
 */
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const CARGO_TOML = resolve(ROOT, 'src-tauri/Cargo.toml');
const PACKAGE_JSON = resolve(ROOT, 'package.json');
const PACKAGE_LOCK = resolve(ROOT, 'package-lock.json');
const AGENT_MD = resolve(ROOT, 'AGENT.md');

const die = (msg) => {
  console.error(`[sync_version] ${msg}`);
  process.exit(1);
};

/** 只匹配 [package] 段内的 version（首个 version = "x.y.z"） */
function readCargoVersion() {
  const m = readFileSync(CARGO_TOML, 'utf8').match(/^version\s*=\s*"(\d+\.\d+\.\d+)"/m);
  if (!m) die(`${CARGO_TOML} 中未找到 version`);
  return m[1];
}

function writeCargoVersion(version) {
  const text = readFileSync(CARGO_TOML, 'utf8');
  let n = 0;
  const out = text.replace(
    /^(version\s*=\s*)"\d+\.\d+\.\d+"/m,
    (_, p1) => {
      n++;
      return `${p1}"${version}"`;
    },
  );
  if (n !== 1) die('Cargo.toml version 行替换失败');
  writeFileSync(CARGO_TOML, out);
}

function syncPackageJson(version) {
  const data = JSON.parse(readFileSync(PACKAGE_JSON, 'utf8'));
  data.version = version;
  // 与原 Python 版输出对齐：2 空格缩进 + 末尾换行，中文原样
  writeFileSync(PACKAGE_JSON, JSON.stringify(data, null, 2) + '\n');
}

/** package-lock.json 两处 version（根级 + packages[""]）必须与 package.json 一致，
 *  否则 npm ci / 部分校验报 EUSAGE lock 版本不匹配。3.9.4/3.9.5 升版曾两次遗漏，
 *  收敛进脚本后不再依赖手工同步。 */
function syncPackageLock(version) {
  const raw = readFileSync(PACKAGE_LOCK, 'utf8');
  const data = JSON.parse(raw);
  let n = 0;
  if (data.version !== undefined) {
    data.version = version;
    n++;
  }
  if (data.packages?.['']?.version !== undefined) {
    data.packages[''].version = version;
    n++;
  }
  if (n !== 2) die(`package-lock.json 结构异常：仅更新了 ${n}/2 处 version，请人工检查`);
  // 2 空格缩进 + 末尾换行（与 npm 生成格式一致）
  writeFileSync(PACKAGE_LOCK, JSON.stringify(data, null, 2) + '\n');
}

function syncAgentMd(version) {
  const text = readFileSync(AGENT_MD, 'utf8');
  let n = 0;
  const out = text.replace(
    /(^# AGENT\.md — .* v)\d+\.\d+\.\d+/m,
    (_, p1) => {
      n++;
      return p1 + version;
    },
  );
  if (n === 1) {
    writeFileSync(AGENT_MD, out);
  } else {
    console.error('[sync_version] 警告：AGENT.md 标题未匹配到版本号（跳过）');
  }
}

/** 让 Cargo.lock 与 Cargo.toml 对齐（只更新本 crate 条目，不动其他依赖）。 */
function syncCargoLock() {
  const r = spawnSync('cargo', ['update', '-p', 'ai-work-assistant'], {
    cwd: resolve(ROOT, 'src-tauri'),
    stdio: 'inherit',
  });
  if (r.status !== 0) die('cargo update -p 失败，请手动运行 cargo check 对齐 Cargo.lock');
}

const target = process.argv[2];
if (target) {
  if (!/^\d+\.\d+\.\d+$/.test(target)) die(`非法版本号: ${target}（需 x.y.z）`);
  writeCargoVersion(target);
  syncCargoLock();
}

const version = readCargoVersion();
syncPackageJson(version);
syncPackageLock(version);
syncAgentMd(version);
console.log(`[sync_version] 版本已同步为 ${version}：package.json / package-lock.json / AGENT.md / Cargo.lock 已对齐`);
console.log('[sync_version] tauri.conf.json 与 about.ts 无需改动（自动跟随 Cargo.toml）');

#!/usr/bin/env node
/** 版本号单源同步工具（单一来源：crates/aiwork-core/Cargo.toml）
 *
 * 用法：
 *   node scripts/sync_version.mjs              # 读取 Cargo.toml 版本，同步其余各处
 *   node scripts/sync_version.mjs 3.2.0        # 先把 Cargo.toml 改为目标版本，再同步其余各处
 *
 * 同步点：
 *   - crates/aiwork-core/Cargo.toml   单一来源（手动/参数指定）
 *   - crates/aiwork-server/Cargo.toml 自动（与 core 保持一致）
 *   - Cargo.lock                      自动（cargo update -p，workspace 根）
 *   - package.json                    本脚本写入
 *   - AGENT.md 标题                   本脚本写入
 */
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const CORE_TOML = resolve(ROOT, 'crates/aiwork-core/Cargo.toml');
const SERVER_TOML = resolve(ROOT, 'crates/aiwork-server/Cargo.toml');
const PACKAGE_JSON = resolve(ROOT, 'package.json');
const AGENT_MD = resolve(ROOT, 'AGENT.md');

const die = (msg) => {
  console.error(`[sync_version] ${msg}`);
  process.exit(1);
};

/** 只匹配 [package] 段内的 version（首个 version = "x.y.z"） */
function readCargoVersion() {
  const m = readFileSync(CORE_TOML, 'utf8').match(/^version\s*=\s*"(\d+\.\d+\.\d+)"/m);
  if (!m) die(`${CORE_TOML} 中未找到 version`);
  return m[1];
}

function writeCargoVersion(path, version) {
  const text = readFileSync(path, 'utf8');
  let n = 0;
  const out = text.replace(
    /^(version\s*=\s*)"\d+\.\d+\.\d+"/m,
    (_, p1) => {
      n++;
      return `${p1}"${version}"`;
    },
  );
  if (n !== 1) die(`${path} version 行替换失败`);
  writeFileSync(path, out);
}

function syncPackageJson(version) {
  const data = JSON.parse(readFileSync(PACKAGE_JSON, 'utf8'));
  data.version = version;
  // 与原版输出对齐：2 空格缩进 + 末尾换行，中文原样
  writeFileSync(PACKAGE_JSON, JSON.stringify(data, null, 2) + '\n');
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

/** 让 Cargo.lock 与 workspace 各 crate 对齐（只更新本项目 crate 条目，不动依赖）。 */
function syncCargoLock() {
  const r = spawnSync('cargo', ['update', '-p', 'aiwork-core', '-p', 'aiwork-server'], {
    cwd: ROOT,
    stdio: 'inherit',
  });
  if (r.status !== 0) die('cargo update -p 失败，请手动运行 cargo check 对齐 Cargo.lock');
}

const target = process.argv[2];
if (target) {
  if (!/^\d+\.\d+\.\d+$/.test(target)) die(`非法版本号: ${target}（需 x.y.z）`);
  writeCargoVersion(CORE_TOML, target);
  writeCargoVersion(SERVER_TOML, target);
  syncCargoLock();
}

const version = readCargoVersion();
syncPackageJson(version);
syncAgentMd(version);
console.log(`[sync_version] 版本已同步为 ${version}：package.json / AGENT.md / Cargo.lock 已对齐`);

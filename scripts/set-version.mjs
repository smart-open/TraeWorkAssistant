#!/usr/bin/env node
/**
 * 版本号一键同步脚本（单一来源：src-tauri/Cargo.toml）
 *
 * 用法：npm run set-version <x.y.z>
 *
 * 同步范围：
 *  - package.json            （前端版本导入源）
 *  - src-tauri/Cargo.toml    （Rust/打包版本源头，env!("CARGO_PKG_VERSION") 自动取）
 *  - src-tauri/Cargo.lock    （cargo update 同步）
 *  - AGENT.md / docs 头部标注（首个 vX.Y.Z 字样）
 *  - tauri.conf.json 不写 version 字段，构建时自动回退到 Cargo.toml
 *
 * 注意：CHANGELOG.md 需手动新增版本条目。
 */
import { readFileSync, writeFileSync } from 'node:fs';
import { execSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const ver = process.argv[2];
if (!ver || !/^\d+\.\d+\.\d+$/.test(ver)) {
  console.error('用法: npm run set-version <x.y.z>   例如: npm run set-version 2.7.2');
  process.exit(1);
}

const edit = (file, fn) => {
  const p = path.join(root, file);
  const before = readFileSync(p, 'utf8');
  const after = fn(before);
  if (after === before) {
    console.warn(`⚠ 未变更: ${file}`);
  } else {
    writeFileSync(p, after);
    console.log(`✓ ${file}`);
  }
};

// package.json
edit('package.json', (s) => s.replace(/("version":\s*")[^"]+"/, `$1${ver}"`));
// Cargo.toml（仅 [package] 段首个 version）
edit('src-tauri/Cargo.toml', (s) => s.replace(/^version = "[^"]+"/m, `version = "${ver}"`));
// AGENT.md 与 docs 头部（首个 vX.Y.Z）
for (const f of [
  'AGENT.md',
  'docs/tech-framework.md',
  'docs/user-manual.md',
]) {
  edit(f, (s) => s.replace(/v\d+\.\d+\.\d+/, `v${ver}`));
}
// Cargo.lock（失败不阻塞，构建时 cargo 会自动重写）
try {
  execSync('cargo update -p trae-work-assistant --quiet', {
    cwd: path.join(root, 'src-tauri'),
    stdio: 'inherit',
  });
  console.log('✓ src-tauri/Cargo.lock');
} catch {
  console.warn('⚠ Cargo.lock 同步失败（可忽略，构建时自动重写）');
}
console.log(`\n版本已同步为 ${ver}，请记得更新 CHANGELOG.md`);

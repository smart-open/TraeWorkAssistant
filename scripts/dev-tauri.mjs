// tauri CLI 包装脚本：
// - `npm run tauri dev`：在 5000-6000 范围内随机挑选一个空闲端口，
//   写入 tauri.dev.conf.json（--config 合并覆盖 devUrl）并经 VITE_PORT 传给 vite，
//   避免 5173 等常用端口被残留进程占用导致 dev 启动失败。
// - 其他子命令（如 build）：原样透传给 tauri CLI。
//
// 调用方式：用当前 node 直接执行 @tauri-apps/cli 的 tauri.js 入口，
// 不经 shell（规避 Node DEP0190「args + shell 拼接」弃用警告，也无转义风险）。
import net from 'node:net';
import fs from 'node:fs';
import { spawn } from 'node:child_process';
import path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const root = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const require = createRequire(import.meta.url);
const tauriCliJs = require.resolve('@tauri-apps/cli/tauri.js');
// --config 合并配置（生成物，不入库：见 .gitignore）
const devConfRel = 'tauri.dev.conf.json';

function isPortFree(port) {
  return new Promise((resolve) => {
    const srv = net.createServer();
    srv.once('error', () => resolve(false));
    srv.once('listening', () => srv.close(() => resolve(true)));
    srv.listen(port, '127.0.0.1');
  });
}

async function pickPort() {
  for (let i = 0; i < 300; i++) {
    const p = 5000 + Math.floor(Math.random() * 1001); // 5000..6000
    // eslint-disable-next-line no-await-in-loop
    if (await isPortFree(p)) return p;
  }
  throw new Error('5000-6000 范围内未找到空闲端口');
}

function runTauri(cliArgs, extraEnv = {}) {
  const child = spawn(process.execPath, [tauriCliJs, ...cliArgs], {
    stdio: 'inherit',
    cwd: root,
    env: { ...process.env, ...extraEnv },
  });
  child.on('exit', (code, signal) => process.exit(code ?? (signal ? 1 : 0)));
  child.on('error', (e) => {
    console.error('[dev-tauri] 执行 tauri 失败:', e);
    process.exit(1);
  });
}

const args = process.argv.slice(2);
if (args[0] === 'dev') {
  const port = await pickPort();
  fs.writeFileSync(
    path.join(root, devConfRel),
    JSON.stringify({ build: { devUrl: `http://localhost:${port}` } }, null, 2),
  );
  console.log(`[dev-tauri] 使用随机空闲端口 ${port}（Vite 与 Tauri devUrl 已同步）`);
  runTauri(['dev', '--config', devConfRel], { VITE_PORT: String(port) });
} else {
  runTauri(args);
}

// tauri CLI 包装脚本：
// - `npm run tauri dev`：在 5000-6000 范围内随机挑选一个空闲端口，
//   写入 tauri.dev.conf.json（--config 合并覆盖 devUrl）并经 VITE_PORT 传给 vite，
//   避免 5173 等常用端口被残留进程占用导致 dev 启动失败。
// - 其他子命令（如 build）：原样透传给 tauri CLI。
import net from 'node:net';
import fs from 'node:fs';
import { spawn } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
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

const args = process.argv.slice(2);
if (args[0] === 'dev') {
  const port = await pickPort();
  fs.writeFileSync(
    path.join(root, devConfRel),
    JSON.stringify({ build: { devUrl: `http://localhost:${port}` } }, null, 2),
  );
  console.log(`[dev-tauri] 使用随机空闲端口 ${port}（Vite 与 Tauri devUrl 已同步）`);
  const child = spawn('npx', ['tauri', 'dev', '--config', devConfRel], {
    shell: true,
    stdio: 'inherit',
    cwd: root,
    env: { ...process.env, VITE_PORT: String(port) },
  });
  child.on('exit', (code, signal) => process.exit(code ?? (signal ? 1 : 0)));
  child.on('error', (e) => {
    console.error('[dev-tauri] 启动 tauri 失败:', e);
    process.exit(1);
  });
} else {
  const child = spawn('npx', ['tauri', ...args], {
    shell: true,
    stdio: 'inherit',
    cwd: root,
  });
  child.on('exit', (code, signal) => process.exit(code ?? (signal ? 1 : 0)));
  child.on('error', (e) => {
    console.error('[tauri] 执行失败:', e);
    process.exit(1);
  });
}

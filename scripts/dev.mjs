// 开发入口：在 5000-6000 范围内随机挑选一个空闲端口，生成 tauri dev 配置覆盖并启动。
//
// 用法：npm run dev（等价 tauri dev + 动态端口）
//   1. 随机扫描 5000-6000 找空闲端口 P
//   2. 写 .tauri-dev-config.json（覆盖 build.devUrl = http://localhost:P）
//   3. 以 VITE_PORT=P 启动 `tauri dev --config .tauri-dev-config.json`
//   4. beforeDevCommand（npm run dev:vite）继承 VITE_PORT，vite 绑定同一端口
//
// 纯前端调试可用 `npm run dev:vite`（固定 5173）；`npm run tauri dev` 也走 5173 回退。
import net from 'node:net';
import { spawn } from 'node:child_process';
import { writeFileSync } from 'node:fs';

/** 探测端口是否空闲（bind 成功即空闲） */
const probe = (port) =>
  new Promise((resolve) => {
    const srv = net.createServer();
    srv.unref();
    srv.once('error', () => resolve(false));
    srv.once('listening', () => srv.close(() => resolve(true)));
    srv.listen(port, '127.0.0.1');
  });

const LO = 5000;
const HI = 6000;
// 随机起点顺序扫描，避免多开发者固定撞同一端口
const start = LO + Math.floor(Math.random() * (HI - LO + 1));
let port = null;
for (let i = 0; i <= HI - LO; i++) {
  const p = LO + ((start - LO + i) % (HI - LO + 1));
  if (await probe(p)) {
    port = p;
    break;
  }
}
if (!port) {
  console.error(`[dev] ${LO}-${HI} 范围内无空闲端口`);
  process.exit(1);
}

writeFileSync(
  '.tauri-dev-config.json',
  JSON.stringify({ build: { devUrl: `http://localhost:${port}` } }),
);
console.log(`[dev] 使用空闲端口 ${port}`);

const child = spawn(
  'npx',
  ['tauri', 'dev', '--config', '.tauri-dev-config.json'],
  {
    stdio: 'inherit',
    shell: true,
    env: { ...process.env, VITE_PORT: String(port) },
  },
);
child.on('exit', (code) => process.exit(code ?? 0));

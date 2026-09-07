import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    // 端口由 scripts/dev-tauri.mjs 随机挑选（5000-6000）并经 VITE_PORT 传入，
    // 与 tauri.dev.conf.json 的 devUrl 保持一致；单独跑 `npm run dev` 时回退 5173
    port: Number(process.env.VITE_PORT) || 5173,
    strictPort: true,
    // Rust 构建产物目录不应被 vite 监视，否则编译期锁定的 .exe 会触发 EBUSY 导致 dev 崩溃
    watch: {
      ignored: ['**/src-tauri/target/**'],
    },
  },
  build: {
    target: 'esnext',
    sourcemap: false,
  },
});
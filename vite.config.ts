import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    // 开发端口：由 scripts/dev.mjs 随机挑选 5000-6000 空闲端口并经 VITE_PORT 传入；
    // 未设置时回退 5173（纯前端调试 / npm run tauri dev 直启场景）
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
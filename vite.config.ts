import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    // Web 版开发端口（默认 5173）；API 走下方 proxy 到本机 aiwork-server
    port: 5173,
    strictPort: true,
    // Web 版开发代理（T9）：管理命令桥/SSE 与 OpenAI 网关转发到本机 aiwork-server，
    // 目标地址可用 VITE_API_TARGET 覆盖（默认 127.0.0.1:7864 = 网关设置端口）
    proxy: {
      '/api': { target: process.env.VITE_API_TARGET || 'http://127.0.0.1:7864' },
      '/v1': { target: process.env.VITE_API_TARGET || 'http://127.0.0.1:7864' },
    },
  },
  build: {
    target: 'esnext',
    sourcemap: false,
  },
});
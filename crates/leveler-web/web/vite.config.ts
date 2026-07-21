import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// mock 服务端地址（与 mock/server.mjs 的 PORT 一致）
const MOCK_ORIGIN = 'http://127.0.0.1:7331';

export default defineConfig({
  plugins: [react()],
  server: {
    // 与 mock/真实网关一致，只绑回环地址
    host: '127.0.0.1',
    proxy: {
      '/api': { target: MOCK_ORIGIN, changeOrigin: true },
      '/ws': { target: MOCK_ORIGIN.replace(/^http/, 'ws'), ws: true },
    },
  },
});

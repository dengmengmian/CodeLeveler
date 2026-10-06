import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import pkg from './package.json' with { type: 'json' };

// mock 服务端地址（与 mock/server.mjs 的 PORT 一致）
const MOCK_ORIGIN = 'http://127.0.0.1:7331';

// 仓库根的 testdata（Execution Presentation Contract 的共享 fixture corpus）。
// 测试直接 import 它，避免复制一份会漂移的镜像。
const REPO_ROOT = new URL('../../../', import.meta.url).pathname;

export default defineConfig({
  define: { __APP_VERSION__: JSON.stringify(pkg.version) },
  plugins: [react()],
  server: {
    // 与 mock/真实网关一致，只绑回环地址
    host: '127.0.0.1',
    fs: { allow: ['.', REPO_ROOT] },
    proxy: {
      '/api': { target: MOCK_ORIGIN, changeOrigin: true },
      '/ws': { target: MOCK_ORIGIN.replace(/^http/, 'ws'), ws: true },
    },
  },
});

import { defineConfig } from 'vitest/config';
import type { ProxyOptions } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import { readFileSync } from 'node:fs';

const pkg = JSON.parse(readFileSync(new URL('./package.json', import.meta.url), 'utf8')) as { version: string };

const daemon = 'http://127.0.0.1:47770';

// The daemon checks Origin on mutations and WS upgrades (ARCHITECTURE §13). In dev the
// browser origin is the Vite server, so present the daemon's own origin instead.
const proxy: ProxyOptions = {
  target: daemon,
  ws: true,
  configure: (p) => {
    p.on('proxyReq', (req) => req.setHeader('origin', daemon));
    p.on('proxyReqWs', (req) => req.setHeader('origin', daemon));
  },
};

export default defineConfig({
  plugins: [svelte()],
  define: { __APP_VERSION__: JSON.stringify(pkg.version) },
  server: {
    port: 5173,
    proxy: { '/api': proxy, '/auth': proxy, '/mcp': proxy },
  },
  // One chunk (~230 KB gzip, mostly xterm) served from localhost or the LAN portal; splitting buys nothing.
  build: { outDir: 'dist', target: 'es2022', emptyOutDir: true, chunkSizeWarningLimit: 1024 },
  test: { include: ['src/**/*.test.ts'], environment: 'node' },
});

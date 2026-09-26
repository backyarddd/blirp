import { defineConfig } from '@playwright/test';

// End-to-end suite against the real daemon (see e2e/global-setup.ts). Runs the installed
// Microsoft Edge by default so no browser download is needed; set BLIRP_E2E_CHANNEL
// (e.g. `chrome`, or `chromium` after `pnpm exec playwright install chromium`) elsewhere.
export default defineConfig({
  testDir: './e2e',
  globalSetup: './e2e/global-setup.ts',
  fullyParallel: false,
  workers: 1,
  timeout: 90_000,
  expect: { timeout: 15_000 },
  reporter: [['list']],
  use: {
    channel: process.env.BLIRP_E2E_CHANNEL ?? 'msedge',
    headless: true,
    viewport: { width: 1360, height: 860 },
    trace: 'retain-on-failure',
  },
});

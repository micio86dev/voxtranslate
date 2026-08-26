import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: './e2e',
  // Fail CI on a stray `.only` (which would silently skip every other e2e spec).
  forbidOnly: !!process.env.CI,
  timeout: 90_000,
  fullyParallel: false,
  workers: 1,
  // The multi-peer WebRTC specs (call/screenshare) depend on mesh renegotiation landing
  // within a timeout; under a loaded machine that occasionally overshoots. One retry
  // absorbs that timing flake without hiding a real regression (a genuine break fails
  // both attempts). Deterministic specs pass on the first try, so this costs nothing.
  retries: 1,
  reporter: [['list']],
  globalSetup: './e2e/global-setup.ts',
  globalTeardown: './e2e/global-teardown.ts',
  // Build a coverage-instrumented bundle and serve it. (The Rust backend on
  // :3001 must be running separately — it needs DEEPGRAM/GROQ keys.)
  webServer: {
    // COVERAGE/PUBLIC_WS_HOST go in `env` (not a shell prefix) so they reach BOTH
    // `npm run build` AND `astro preview`. `astro preview` re-reads astro.config, and
    // COVERAGE=1 there selects the Node standalone adapter — the Vercel adapter cannot
    // serve `astro preview`. A shell prefix would only apply to the build, leaving preview
    // on the Vercel adapter and failing to start.
    command: 'npm run build && npx astro preview --port 4321',
    env: { COVERAGE: '1', PUBLIC_WS_HOST: 'localhost:3001' },
    // 127.0.0.1, not localhost: on a machine where another project holds ::1:4321,
    // `localhost` resolves to IPv6 first and the readiness probe reads THAT server —
    // so Playwright decides ours never came up and tries to start a second one.
    url: 'http://127.0.0.1:4321',
    reuseExistingServer: true,
    timeout: 180_000,
  },
  use: {
    baseURL: process.env.E2E_BASE || 'http://localhost:4321',
    permissions: ['microphone', 'camera'],
    launchOptions: {
      args: [
        '--use-fake-device-for-media-stream',
        '--use-fake-ui-for-media-stream',
        '--disable-features=WebRtcHideLocalIpsWithMdns',
        '--no-sandbox',
      ],
    },
  },
  projects: [{ name: 'chromium', use: { browserName: 'chromium' } }],
});

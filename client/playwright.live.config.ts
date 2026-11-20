// Live Connect e2e against a REAL local stack (spec 0121). Separate from `playwright.config.ts`
// on purpose: the main suite and CI run against a guest-only backend on :3001 and know nothing
// about accounts, organisations or a mock upstream. This one is opt-in:
//
//   npm run test:e2e:live
//
// What it starts (see e2e/support/live-server.mjs for the details and the safety rules):
//   * the Rust server (the repo's already-built debug binary) on :3101, against the LOCAL
//     Postgres only, with Live Connect, billing and the Standard engine on
//   * a Node mock of the Qwen realtime upstream on :3102 (e2e/support/mock-qwen.mjs)
//   * the client under `astro dev` on 127.0.0.1:4331 with PUBLIC_WS_HOST pointing at :3101
// Prerequisites: Postgres from `docker compose up -d --wait postgres` (pgvector, :55432),
// `psql` on PATH, `npx playwright install chromium`, and a built server binary
// (`LIVE_E2E_BUILD=1` builds it first). docs/live-connect.md, "Testing", has the full recipe.
import { defineConfig } from '@playwright/test';

const ORIGIN = process.env.LIVE_E2E_CLIENT_ORIGIN || 'http://127.0.0.1:4331';
const SERVER_PORT = process.env.LIVE_E2E_SERVER_PORT || '3101';

export default defineConfig({
  testDir: './e2e',
  testMatch: /live\.spec\.ts/,
  forbidOnly: !!process.env.CI,
  timeout: 120_000,
  expect: { timeout: 20_000 },
  fullyParallel: false,
  workers: 1,
  retries: 0,
  reporter: [['list']],
  globalSetup: './e2e/support/live-global-setup.mjs',
  webServer: {
    // `--ignore-lock`: Astro 7 refuses a second dev server per project, and one is often already
    // running for development. The client talks to the server named at build time; `astro dev` reads it from the
    // environment. 127.0.0.1, not localhost: another project may hold ::1 on a nearby port.
    command: `npx astro dev --ignore-lock --host 127.0.0.1 --port ${new URL(ORIGIN).port}`,
    env: {
      PUBLIC_WS_HOST: `127.0.0.1:${SERVER_PORT}`,
      // Astro 7 detects an AI-agent shell and silently backgrounds the dev server, which
      // Playwright can then neither watch nor stop. Blank the markers so it runs in the
      // foreground, as it does on any developer's terminal.
      AI_AGENT: '',
      CLAUDECODE: '',
      CLAUDE_CODE_ENTRYPOINT: '',
    },
    url: ORIGIN,
    reuseExistingServer: false,
    timeout: 180_000,
  },
  use: {
    baseURL: ORIGIN,
    permissions: ['microphone'],
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    launchOptions: {
      args: [
        // A synthetic microphone (a tone) and no permission prompt: the audio the server
        // receives is real PCM, just not a voice. The mock upstream answers any audio.
        '--use-fake-device-for-media-stream',
        '--use-fake-ui-for-media-stream',
        '--no-sandbox',
        // Headless Chromium would otherwise suspend an AudioContext that has had no gesture.
        '--autoplay-policy=no-user-gesture-required',
      ],
    },
  },
  projects: [{ name: 'chromium', use: { browserName: 'chromium' } }],
});

// Launcher for the REAL Rust server plus the mock Qwen upstream, for the Live Connect e2e.
//
//   node client/e2e/support/live-server.mjs          # run the stack until Ctrl-C (manual use, k6 smoke)
//   import { startLiveStack } from './live-server.mjs' # what playwright.live.config.ts does
//
// What runs: the server binary built from this checkout (`server/target-rebuild/debug/
// voxtranslate-server`, the repo's own cargo target dir) against the LOCAL Postgres only,
// with Live Connect on, billing on, the Standard engine pointed at `mock-qwen.mjs`, and
// nothing else configured. It deliberately does NOT inherit the caller's environment: a
// developer shell or a `server/.env` can point at a deployed database and real provider keys,
// and a test must not be able to pick those up. The process runs in an empty scratch directory
// so `dotenvy` has no `.env` to find in the working directory or above it.
//
// Building: this does not compile unless asked. Set `LIVE_E2E_BUILD=1` (or pass `build: true`)
// to run `cargo build` first, which needs the rustup 1.97 toolchain first on PATH and
// MACOSX_DEPLOYMENT_TARGET on macOS (see docs/live-connect.md, Testing). Another cargo run in
// the same target dir makes this wait on cargo's own lock; that is expected.

import { spawn, spawnSync } from 'node:child_process';
import { closeSync, existsSync, mkdtempSync, openSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { assertLocalDb, dbUrl, JWT_SECRET } from './live-seed.mjs';
import { startMockQwen } from './mock-qwen.mjs';

const here = dirname(fileURLToPath(import.meta.url));
export const REPO = resolve(here, '../../..');
export const LIVE_TOKEN_KEY = 'e2e-live-token-key-0123456789abcdef0123456789abcdef';

export const DEFAULTS = {
  serverPort: Number(process.env.LIVE_E2E_SERVER_PORT || 3101),
  mockPort: Number(process.env.LIVE_E2E_MOCK_PORT || 3102),
  // Where the browser loads the client from. Invitation links and QR codes are built from it
  // (APP_BASE_URL) and the server's CORS allows it.
  clientOrigin: process.env.LIVE_E2E_CLIENT_ORIGIN || 'http://127.0.0.1:4331',
};

function serverBinary() {
  return process.env.LIVE_E2E_SERVER_BIN || join(REPO, 'server/target-rebuild/debug/voxtranslate-server');
}

function cargoBuild(log) {
  const r = spawnSync('cargo', ['build', '--bin', 'voxtranslate-server'], {
    cwd: join(REPO, 'server'),
    env: process.env,
    stdio: ['ignore', log, log],
  });
  if (r.status !== 0) throw new Error(`cargo build failed (exit ${r.status}); see the log`);
}

async function waitForHealth(url, child, ms = 90_000) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) throw new Error(`the server exited early (code ${child.exitCode}); see its log`);
    try {
      if ((await fetch(`${url}/health`)).ok) return;
    } catch {
      /* not up yet */
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(`the server never answered ${url}/health in ${ms / 1000}s; see its log`);
}

/**
 * Start the mock upstream and the server. Resolves to
 * `{ serverUrl, wsHost, mock, logFile, stop() }`. Always `stop()` it.
 */
export async function startLiveStack(opts = {}) {
  const { serverPort, mockPort, clientOrigin } = { ...DEFAULTS, ...opts };
  assertLocalDb();
  const scratch = mkdtempSync(join(tmpdir(), 'live-e2e-'));
  const logFile = join(scratch, 'server.log');
  const log = openSync(logFile, 'a');

  if (opts.build ?? process.env.LIVE_E2E_BUILD === '1') cargoBuild(log);
  const bin = serverBinary();
  if (!existsSync(bin)) {
    throw new Error(`no server binary at ${bin}. Build it (cd server && cargo build) or set LIVE_E2E_BUILD=1`);
  }

  const mock = await startMockQwen({ port: mockPort });
  const env = {
    PATH: process.env.PATH,
    HOME: process.env.HOME,
    TMPDIR: scratch,
    PORT: String(serverPort),
    DATABASE_URL: dbUrl(),
    // Billing on needs these three; the values are meaningless outside this process.
    GOOGLE_CLIENT_ID: 'e2e-client-id',
    JWT_SECRET,
    // Providers: required to boot, never reached (the engine is pointed at the mock).
    GROQ_API_KEY: 'e2e-not-a-key',
    DEEPGRAM_API_KEY: 'e2e-not-a-key',
    QWEN_API_KEY: 'e2e-not-a-key',
    QWEN_STANDARD: '1',
    QWEN_REALTIME_ENDPOINT: mock.url,
    // Live Connect on, with the shortest billing interval so a few seconds of speech charge.
    LIVE_CONNECT_ENABLED: '1',
    LIVE_TOKEN_KEY,
    USAGE_UPDATE_INTERVAL: '2',
    // The real rate is a fraction of a cent per tick, which an organisation (whole credits, one
    // credit = one cent) would only be charged after about a minute. A deliberately high rate makes a
    // few seconds of conversation visibly move both wallets; nothing else depends on it.
    QWEN_COST_PER_MINUTE: '1.2',
    APP_BASE_URL: clientOrigin,
    ALLOWED_ORIGINS: [clientOrigin, clientOrigin.replace('127.0.0.1', 'localhost')].join(','),
    RUST_LOG: 'voxtranslate_server=info,canonical=warn',
  };
  const child = spawn(bin, [], { cwd: scratch, env, stdio: ['ignore', log, log] });
  const serverUrl = `http://127.0.0.1:${serverPort}`;

  const stop = async ({ keepLogs = false } = {}) => {
    child.kill('SIGTERM');
    await new Promise((r) => {
      const t = setTimeout(() => {
        child.kill('SIGKILL');
        r();
      }, 5000);
      child.once('exit', () => {
        clearTimeout(t);
        r();
      });
      if (child.exitCode !== null) r();
    });
    await mock.stop();
    try {
      closeSync(log);
    } catch {
      /* already closed */
    }
    if (!keepLogs && !process.env.LIVE_E2E_KEEP_LOGS) rmSync(scratch, { recursive: true, force: true });
  };

  try {
    await waitForHealth(serverUrl, child);
  } catch (e) {
    let tail = '';
    try {
      tail = readFileSync(logFile, 'utf8').split('\n').slice(-25).join('\n');
    } catch {
      /* no log */
    }
    await stop({ keepLogs: true });
    throw new Error(`${e.message}\nlog: ${logFile}\n${tail}`);
  }
  return { serverUrl, wsHost: `127.0.0.1:${serverPort}`, mock, logFile, stop };
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const stack = await startLiveStack({ build: process.argv.includes('--build') || undefined });
  console.log(`live e2e stack up: server ${stack.serverUrl}, mock ${stack.mock.url}, log ${stack.logFile}`);
  const down = async () => {
    await stack.stop();
    process.exit(0);
  };
  process.on('SIGINT', down);
  process.on('SIGTERM', down);
}

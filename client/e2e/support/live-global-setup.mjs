// Playwright globalSetup for `playwright.live.config.ts`: bring up the mock upstream and the
// real server before the first test, tear both down after the last. Returns the teardown.
import { startLiveStack } from './live-server.mjs';

export default async function globalSetup() {
  const stack = await startLiveStack();
  process.env.LIVE_E2E_SERVER_URL = stack.serverUrl;
  process.env.LIVE_E2E_MOCK_URL = `http://127.0.0.1:${stack.mock.port}`;
  // eslint-disable-next-line no-console
  console.log(`live e2e: server ${stack.serverUrl}, mock upstream ${stack.mock.url}`);
  return async () => {
    await stack.stop();
  };
}

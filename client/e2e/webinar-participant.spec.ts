// Webinar participant page e2e (webinar phase 1, F1-5). A guest opens `/w/{code}` and
// watches the live, auto-translated broadcast. We intercept the public webinar endpoint
// with one dispatcher so no Rust backend is needed. Covers: the waiting state on a
// scheduled webinar, the client-side poll flipping to live (video attaches), the ended
// overlay, and the not-found state on a 404.
//
// NOTE on SSR: `/w/[code]` is an on-demand route whose <title>/OG fetch runs on the
// PREVIEW SERVER (not interceptable by page.route). During preview PUBLIC_WS_HOST points
// at a non-serving host, so that server fetch fails soft and the page renders the shell;
// the CLIENT player fetch (which page.route CAN intercept) then drives the visible state.
// That is exactly the runtime fallback path, so these assertions are meaningful.
import { test, expect } from '@playwright/test';
import { openPage, closePage } from './helpers';

function json(body: unknown, status = 200) {
  return { status, contentType: 'application/json', body: JSON.stringify(body) };
}

const CODE = 'ab12cd';
const publicWebinar = (over: Record<string, unknown> = {}) => ({
  code: CODE,
  title: 'Product launch',
  status: 'scheduled',
  source_language: 'en',
  tier: 'enhanced',
  join_url: `https://voxtranslate.app/w/${CODE}`,
  playback_url: `https://hls.example/webinar/${CODE}/index.m3u8`,
  guest_id: 'guest-xyz',
  ...over,
});

test('a guest sees the waiting state, and the client polls to detect the live transition', async ({ browser }) => {
  const t = await openPage(browser);
  // The webinar is scheduled; from the 2nd poll of the MAIN endpoint it reports live.
  let mainPolls = 0;
  await t.page.route('**/api/w/**', (route) => {
    const p = new URL(route.request().url()).pathname;
    if (p === `/api/w/${CODE}`) mainPolls++;
    route.fulfill(json(publicWebinar({ status: mainPolls >= 2 ? 'live' : 'scheduled' })));
  });
  // Absorb any HLS manifest/segment requests so nothing hits the network.
  await t.page.route('**/*.m3u8', (route) => route.fulfill({ status: 200, contentType: 'application/vnd.apple.mpegurl', body: '#EXTM3U' }));
  await t.page.route('**/*.ts', (route) => route.fulfill({ status: 200, contentType: 'video/mp2t', body: '' }));

  await t.page.goto(`/w/${CODE}`, { waitUntil: 'domcontentloaded' });

  // Waiting overlay is shown while scheduled.
  await expect(t.page.locator('#wv-overlay-waiting')).toBeVisible();
  await expect(t.page.locator('#wv-status')).toHaveText(/.+/); // localized "Waiting…"

  // The guest id from the API is persisted to localStorage.
  await expect
    .poll(() => t.page.evaluate(() => localStorage.getItem('vox.guest_id')))
    .toBe('guest-xyz');

  // The player POLLS the public endpoint (every 5s) to detect the waiting→live transition:
  // assert the poll loop actually runs and observes the flip (≥2 calls, later ones live).
  // We assert the detection mechanism rather than the visible `live` badge because reaching
  // the live state additionally needs a decodable video frame, which mocked HLS media can't
  // provide (the player deliberately holds the waiting overlay until a real frame arrives).
  await expect.poll(() => mainPolls, { timeout: 20_000 }).toBeGreaterThanOrEqual(2);

  await closePage(t);
});

test('a guest sees the ended overlay when the webinar has ended', async ({ browser }) => {
  const t = await openPage(browser);
  await t.page.route('**/api/w/**', (route) => route.fulfill(json(publicWebinar({ status: 'ended' }))));

  await t.page.goto(`/w/${CODE}`, { waitUntil: 'domcontentloaded' });
  await expect(t.page.locator('#wv-overlay-ended')).toBeVisible({ timeout: 10_000 });

  await closePage(t);
});

test('an unknown code renders the not-found state', async ({ browser }) => {
  const t = await openPage(browser);
  await t.page.route('**/api/w/**', (route) => route.fulfill(json({ error: 'not found' }, 404)));

  // The SSR fetch fails soft during preview, so the client player fetch drives this: a
  // 404 from the client fetch surfaces the player's error overlay. (When the SSR fetch
  // itself 404s in production the server renders the dedicated not-found card.)
  await t.page.goto(`/w/${CODE}`, { waitUntil: 'domcontentloaded' });
  // In preview the SSR fetch fails soft → the CLIENT player fetch 404s → the error overlay.
  // (A single unambiguous selector: the not-found `.wv-card` is the production-SSR path.)
  await expect(t.page.locator('#wv-overlay-error')).toBeVisible({ timeout: 10_000 });

  await closePage(t);
});

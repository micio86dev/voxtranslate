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

test('a guest sees the waiting state, then the manifest goes live', async ({ browser }) => {
  const t = await openPage(browser);
  // First the webinar is scheduled; after the first poll it flips to live.
  let live = false;
  await t.page.route('**/api/w/**', (route) => {
    route.fulfill(json(publicWebinar({ status: live ? 'live' : 'scheduled' })));
    live = true;
  });
  // Block the actual HLS manifest fetch — hls.js will try to load it; we only assert on
  // the player STATE, not real media playback.
  await t.page.route('**/*.m3u8', (route) => route.fulfill({ status: 200, contentType: 'application/vnd.apple.mpegurl', body: '#EXTM3U' }));

  await t.page.goto(`/w/${CODE}`, { waitUntil: 'domcontentloaded' });

  // Waiting overlay is shown while scheduled.
  await expect(t.page.locator('#wv-overlay-waiting')).toBeVisible();
  await expect(t.page.locator('#wv-status')).toHaveText(/.+/); // localized "Waiting…"

  // The guest id from the API is persisted to localStorage.
  await expect
    .poll(() => t.page.evaluate(() => localStorage.getItem('vox.guest_id')))
    .toBe('guest-xyz');

  // The poll (5s) flips the webinar live → the status badge gains the live class.
  await expect(t.page.locator('#wv-status.is-live')).toBeVisible({ timeout: 15_000 });
  await expect(t.page.locator('#wv-overlay-waiting')).toBeHidden();

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
  await expect(
    t.page.locator('#wv-overlay-error, .wv-card'),
  ).toBeVisible({ timeout: 10_000 });

  await closePage(t);
});

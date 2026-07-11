// Webinar host UI e2e (webinar phase 0). The running backend is guest-only, so we
// simulate billing + an active-subscription org by intercepting every /api/* call
// with one dispatcher. Covers: a signed-in host reveals the Webinars menu entry,
// opens the Webinars screen, creates a webinar, and sees the join link + a QR that
// encodes the API's join_url verbatim, plus the copy affordance.
//
// NOTE: this spec is self-contained (all /api/* + the qrcode chunk are same-origin
// and served by the built bundle) and does NOT need the Rust backend — unlike the
// call/subtitle specs it never opens a /ws signaling socket.
import { test, expect } from '@playwright/test';
import type { Page } from '@playwright/test';
import { openPage, closePage, trackConsoleErrors } from './helpers';

function json(body: unknown, status = 200) {
  return { status, contentType: 'application/json', body: JSON.stringify(body) };
}

const USER = { id: 'u1', email: 'host@acme.com', name: 'Hank', avatar_url: null, balance: 10, consent_given: true };
const ORG = { id: 'org-1', name: 'Acme Inc', role: 'owner', plan: 'business', subscription_status: 'active' };
const JOIN_URL = 'https://voxtranslate.app/w/ab12cd';

const CREATED = {
  id: 'web-1',
  org_id: ORG.id,
  code: 'ab12cd',
  title: 'Product launch',
  description: null,
  source_language: 'en',
  tier: 'enhanced',
  status: 'scheduled',
  scheduled_start: null,
  scheduled_end: null,
  actual_start: null,
  actual_end: null,
  record_video: false,
  record_transcript: true,
  voice_clone: false,
  join_url: JOIN_URL,
  playback_url: null,
  created_at: '2026-07-11T00:00:00Z',
};

/** Mock the billing backend + the webinar host endpoints. `listWebinars` returns the
 *  created webinar once one has been POSTed (so the list refresh after create shows it). */
async function mockApi(page: Page): Promise<void> {
  let created = false;
  await page.route('**/gsi/client', (r) => r.abort()); // block the external Google script
  await page.route('**/api/**', (route) => {
    const req = route.request();
    const p = new URL(req.url()).pathname;
    if (p === '/api/auth/config') return route.fulfill(json({ google_client_id: 'test.apps.googleusercontent.com' }));
    if (p === '/api/user/me') return route.fulfill(json(USER));
    if (p === '/api/business/organizations') return route.fulfill(json([ORG]));
    if (p === '/api/webinars' && req.method() === 'POST') {
      created = true;
      const body = JSON.parse(req.postData() || '{}');
      return route.fulfill(json({ ...CREATED, title: body.title || CREATED.title }, 201));
    }
    if (p === '/api/webinars' && req.method() === 'GET') {
      return route.fulfill(json(created ? [CREATED] : []));
    }
    // Everything else the home screen pings (packages, usage, friends, notifications…).
    if (p === '/api/billing/packages') return route.fulfill(json([]));
    if (p.startsWith('/api/billing/') || p.startsWith('/api/usage/') || p.startsWith('/api/friends')) {
      return route.fulfill(json([]));
    }
    return route.fulfill(json({}, 404));
  });
}

async function seedSession(page: Page): Promise<void> {
  await page.addInitScript((u) => {
    localStorage.setItem('vox.token', 'fake.jwt');
    localStorage.setItem('vox.user', JSON.stringify(u));
  }, USER);
}

test('a host creates a webinar and sees the join link + a QR of the join_url', async ({ browser }) => {
  const t = await openPage(browser);
  const errors = trackConsoleErrors(t.page);
  await mockApi(t.page);
  await seedSession(t.page);

  await t.page.goto('/', { waitUntil: 'networkidle' });
  await expect(t.page.locator('#home')).toBeVisible();

  // The Webinars menu entry is revealed for a host with an active-subscription org.
  await t.page.click('#account-trigger');
  await expect(t.page.locator('#webinars-btn')).toBeVisible();
  await t.page.click('#webinars-btn');

  // The Webinars screen opens (home hidden), pre-populated with the source-language
  // select and an empty list.
  await expect(t.page.locator('#webinars')).toBeVisible();
  await expect(t.page.locator('#home')).toBeHidden();
  await expect(t.page.locator('#webinar-list-empty')).toBeVisible();

  // Fill the create form and submit.
  await t.page.fill('#webinar-title', 'Product launch');
  await t.page.selectOption('#webinar-lang', 'en');
  await t.page.selectOption('#webinar-tier', 'enhanced');
  await t.page.click('#webinar-create-btn');

  // A card appears with the exact join link and a Cancel action (still scheduled).
  const card = t.page.locator('.webinar-card');
  await expect(card).toHaveCount(1);
  await expect(card.locator('.webinar-card-title')).toHaveText('Product launch');
  await expect(card.locator('.webinar-link-input')).toHaveValue(JOIN_URL);
  await expect(card.locator('.webinar-cancel-btn')).toBeVisible();

  // The QR renders as a data-URL image and encodes EXACTLY the API's join_url.
  const qr = card.locator('.webinar-qr');
  await expect(qr).toBeVisible();
  const qrSrc = await qr.getAttribute('src');
  expect(qrSrc).toMatch(/^data:image\/png/);
  const decoded = await decodeQr(t.page, qrSrc);
  expect(decoded).toBe(JOIN_URL);

  // Copy writes the join link to the clipboard and flips the button label to "Copied".
  await t.page.context().grantPermissions(['clipboard-read', 'clipboard-write']);
  await card.locator('.webinar-copy-btn').click();
  await expect(card.locator('.webinar-copy-btn')).not.toHaveText(/^Copy$/);
  const clip = await t.page.evaluate(() => navigator.clipboard.readText().catch(() => ''));
  expect(clip).toBe(JOIN_URL);

  // Back returns to home. Clean console throughout (standing E2E rule).
  await t.page.click('#webinars-back');
  await expect(t.page.locator('#home')).toBeVisible();
  expect(errors).toEqual([]);

  await closePage(t);
});

/** Decode a QR data-URL back to its text in-page (loads jsQR from the CDN). Returns the
 *  decoded string, or '' if decoding is unavailable — the caller asserts on the value. */
async function decodeQr(page: Page, dataUrl: string | null): Promise<string> {
  if (!dataUrl) return '';
  return page.evaluate(async (src) => {
    // jsQR is a tiny pure-JS decoder; pulled at test time only (never shipped).
    await new Promise<void>((res, rej) => {
      const s = document.createElement('script');
      s.src = 'https://cdn.jsdelivr.net/npm/jsqr@1.4.0/dist/jsQR.js';
      s.onload = () => res();
      s.onerror = () => rej(new Error('jsQR load failed'));
      document.head.appendChild(s);
    });
    const img = new Image();
    await new Promise<void>((res, rej) => {
      img.onload = () => res();
      img.onerror = () => rej(new Error('img load failed'));
      img.src = src;
    });
    const canvas = document.createElement('canvas');
    canvas.width = img.width;
    canvas.height = img.height;
    const ctx = canvas.getContext('2d')!;
    ctx.drawImage(img, 0, 0);
    const data = ctx.getImageData(0, 0, canvas.width, canvas.height);
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const result = (window as any).jsQR(data.data, data.width, data.height);
    return result ? (result.data as string) : '';
  }, dataUrl);
}

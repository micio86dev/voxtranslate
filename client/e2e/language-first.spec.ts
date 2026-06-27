// Language-first picker e2e (spec 0102). The backend is guest-only, so we intercept
// /api/* and drive the LANGUAGE_FIRST_UX flag + the engine list ourselves. Covers: the
// flag-off legacy regression guard, language-first tier filtering, RTL flip, union breadth,
// and the guest (Standard-only) path. No uncaught page errors throughout.
import { test, expect } from '@playwright/test';
import type { Page } from '@playwright/test';
import { openPage, closePage } from './helpers';

function json(body: unknown, status = 200) {
  return { status, contentType: 'application/json', body: JSON.stringify(body) };
}

const L8 = ['it', 'en', 'es', 'fr', 'de', 'pt', 'ja', 'zh'];
const ENHANCED = [...L8, 'nl', 'pl', 'ru', 'uk', 'ar', 'he', 'tr', 'ko', 'hi', 'th', 'vi', 'sw'];
const PRO = ['en', 'es', 'pt', 'fr', 'ja', 'ru', 'zh', 'de', 'ko', 'hi', 'id', 'vi', 'it'];
const PREMIUM = [...new Set([...ENHANCED, ...PRO, 'fa', 'ur', 'am', 'zu', 'ca', 'ka', 'hy'])];

function eng(id: string, tier: string, rate: number, out: string[], caps: Partial<Record<string, boolean>> = {}) {
  return {
    id, display_name: tier[0].toUpperCase() + tier.slice(1), tier, description: '',
    rate_per_minute: rate, input_languages: out, output_languages: out,
    capabilities: {
      translated_audio: !!caps.translated_audio, cost_scales_per_language: !!caps.scales,
      client_direct: !!caps.client_direct, max_room_size: 4,
    },
  };
}

const ALL_ENGINES = [
  eng('standard', 'standard', 0.01, L8),
  eng('cartesia', 'enhanced', 0.067, ENHANCED, { client_direct: true, scales: true }),
  eng('premium', 'pro', 0.063, PRO, { translated_audio: true, scales: true }),
  eng('gemini_live_translate', 'premium', 0.068, PREMIUM, { translated_audio: true, scales: true }),
];

async function mockApi(page: Page, opts: { flag: boolean; engines?: unknown[]; user?: unknown }): Promise<void> {
  await page.route('**/gsi/client', (r) => r.abort());
  await page.route('**/api/**', (route) => {
    const p = new URL(route.request().url()).pathname;
    if (p === '/api/auth/config') return route.fulfill(json({ google_client_id: 'test.apps.googleusercontent.com', listener_pays: true }));
    if (p === '/api/engines') return route.fulfill(json({ engines: opts.engines ?? ALL_ENGINES, flags: { language_first_ux: opts.flag } }));
    if (p === '/api/user/me') return route.fulfill(json(opts.user ?? {}));
    if (p === '/api/billing/packages' || p === '/api/billing/history' || p === '/api/usage/sessions') return route.fulfill(json([]));
    return route.fulfill(json({}, 404));
  });
}

const USER = { id: 'u1', email: 'a@b.com', name: 'Alice', avatar_url: null, balance: 10, consent_given: true };
async function login(page: Page): Promise<void> {
  await page.addInitScript((u) => {
    localStorage.setItem('vox.token', 'fake.jwt');
    localStorage.setItem('vox.user', JSON.stringify(u));
  }, USER);
}

test('flag OFF: the legacy language + engine picker still renders (regression guard)', async ({ browser }) => {
  const t = await openPage(browser);
  await login(t.page);
  await mockApi(t.page, { flag: false });
  await t.page.goto('/', { waitUntil: 'networkidle' });
  await expect(t.page.locator('#home')).toBeVisible();
  await expect(t.page.locator('#lang-field')).toBeVisible(); // legacy <select> wrapper
  await expect(t.page.locator('#langfirst-field')).toBeHidden();
  await closePage(t);
});

test('flag ON: language-first picker filters tiers by language and flips RTL', async ({ browser }) => {
  const t = await openPage(browser);
  const errors: string[] = [];
  t.page.on('pageerror', (e) => errors.push(String(e)));
  await login(t.page);
  await mockApi(t.page, { flag: true, user: USER }); // consent_given:true → no consent modal overlay
  await t.page.goto('/', { waitUntil: 'networkidle' });

  // The language-first picker replaces the legacy language <select> + engine selector.
  await expect(t.page.locator('#langfirst-field')).toBeVisible();
  await expect(t.page.locator('#lang-field')).toBeHidden();
  await expect(t.page.locator('#engine-field')).toBeHidden();

  // Default (auto-detected en): all four tiers can output English, cheapest first.
  await expect(t.page.locator('#tier-options .engine-opt')).toHaveCount(4);
  await expect(t.page.locator('#tier-options .engine-opt').first()).toHaveClass(/active/);
  await expect(t.page.locator('#tier-options .engine-opt-name').first()).toHaveText('Standard');

  // Open the picker → the union offers far more than the legacy 8 languages.
  await t.page.click('#lang-trigger');
  await expect(t.page.locator('#lang-panel')).toBeVisible();
  expect(await t.page.locator('#lang-panel .lang-opt').count()).toBeGreaterThan(20);

  // Search + pick Arabic → only the tiers that output Arabic show (Enhanced + Premium;
  // NOT Standard or Pro), cheapest (Enhanced) pre-selected.
  await t.page.fill('#lang-search', 'arab');
  await t.page.click('.lang-opt:has-text("Arabic")');
  await expect(t.page.locator('#lang-panel')).toBeHidden();
  const names = await t.page.locator('#tier-options .engine-opt-name').allTextContents();
  expect(names).toEqual(['Enhanced', 'Premium']);
  await expect(t.page.locator('#tier-options .engine-opt').first()).toHaveClass(/active/);

  // RTL: the whole document mirrors for Arabic.
  await expect.poll(() => t.page.evaluate(() => document.documentElement.dir)).toBe('rtl');

  // Pick a language only Premium covers (Persian) → single tier, auto-selected + note.
  await t.page.click('#lang-trigger');
  await t.page.fill('#lang-search', 'persian');
  await t.page.click('.lang-opt:has-text("Persian")');
  await expect(t.page.locator('#tier-options .engine-opt')).toHaveCount(1);
  await expect(t.page.locator('#tier-options .engine-opt-name')).toHaveText('Premium');
  await expect(t.page.locator('#tier-note')).toBeVisible();

  // Back to a LTR language restores direction.
  await t.page.click('#lang-trigger');
  await t.page.fill('#lang-search', 'german');
  await t.page.click('.lang-opt:has-text("German")');
  await expect.poll(() => t.page.evaluate(() => document.documentElement.dir)).toBe('ltr');

  expect(errors).toEqual([]);
  await closePage(t);
});

test('flag ON, guest: language-first offers only Standard (premium tiers need credits)', async ({ browser }) => {
  const t = await openPage(browser);
  // No login() → guest. Billing config still on, so the login gate shows; continue as guest.
  // Seed the guest 18+/ToS consent (a returning guest) so its modal doesn't overlay the
  // home controls this test asserts on — the test is about tier offering, not the consent gate.
  await t.page.addInitScript(() => {
    try {
      localStorage.setItem('vox_guest_consent', '1');
    } catch {
      /* private mode — consent falls back to in-memory */
    }
  });
  await mockApi(t.page, { flag: true });
  await t.page.goto('/', { waitUntil: 'networkidle' });
  await t.page.click('#guest-btn');
  await expect(t.page.locator('#home')).toBeVisible();
  await expect(t.page.locator('#langfirst-field')).toBeVisible();
  // Guests are pinned to Standard: exactly one tier card, auto-selected.
  await expect(t.page.locator('#tier-options .engine-opt')).toHaveCount(1);
  await expect(t.page.locator('#tier-options .engine-opt-name')).toHaveText('Standard');
  // And the offered languages are Standard's set only (no Arabic).
  await t.page.click('#lang-trigger');
  await t.page.fill('#lang-search', 'arabic');
  await expect(t.page.locator('#lang-panel .lang-opt')).toHaveCount(0);
  await expect(t.page.locator('#lang-panel-empty')).toBeVisible();
  await closePage(t);
});

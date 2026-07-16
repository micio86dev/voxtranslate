// Webinar scheduling + lifecycle e2e (webinar-scheduling epic). A signed-in B2B host with
// an active-subscription org drives: the prominent homepage CTA, the "Avvia ora / Programma"
// create flow with conditional schedule fields + end-enable validation + the notify-friends
// toggle, and editing a scheduled webinar through the detail modal (PATCH). All /api/* is
// intercepted so no Rust backend is needed (mirrors webinar-host.spec.ts).
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
  id: 'web-1', org_id: ORG.id, code: 'ab12cd', title: 'Product launch', description: null,
  source_language: 'en', tier: 'enhanced', status: 'scheduled', visibility: 'public',
  project_id: null, scheduled_start: '2030-01-01T10:00:00Z', scheduled_end: '2030-01-01T11:00:00Z',
  actual_start: null, actual_end: null, record_video: false, record_transcript: true,
  voice_clone: false, chat_enabled: false, members_only: false, notify_friends: true,
  reminder_minutes_before: 10, join_url: JOIN_URL, playback_url: null, created_at: '2026-07-11T00:00:00Z',
  archived_at: null,
};

/** Capture the last POST/PATCH body so tests can assert what the client sent. */
type Captured = { postBody: Record<string, unknown> | null; patchBody: Record<string, unknown> | null };

async function mockApi(page: Page, cap: Captured): Promise<void> {
  let created = false;
  await page.route('**/gsi/client', (r) => r.abort());
  await page.route('**/api/**', (route) => {
    const req = route.request();
    const p = new URL(req.url()).pathname;
    const method = req.method();
    if (p === '/api/auth/config') return route.fulfill(json({ google_client_id: 'test.apps.googleusercontent.com' }));
    if (p === '/api/user/me') return route.fulfill(json(USER));
    if (p === '/api/business/organizations') return route.fulfill(json([ORG]));
    if (p === '/api/webinars' && method === 'POST') {
      created = true;
      cap.postBody = JSON.parse(req.postData() || '{}');
      return route.fulfill(json({ ...CREATED, ...cap.postBody }, 201));
    }
    if (p === '/api/webinars' && method === 'GET') return route.fulfill(json(created ? [CREATED] : []));
    if (p === '/api/webinars/web-1' && method === 'PATCH') {
      cap.patchBody = JSON.parse(req.postData() || '{}');
      return route.fulfill(json({ ...CREATED, ...cap.patchBody }));
    }
    if (p === '/api/billing/packages') return route.fulfill(json([]));
    if (p === '/api/friends/requests') return route.fulfill(json({ incoming: [], outgoing: [] }));
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

async function openWebinars(page: Page): Promise<void> {
  // The prominent homepage CTA is the entry we validate; use it to reach the hub.
  await expect(page.locator('#host-webinar-cta')).toBeVisible();
  await page.click('#home-host-webinar-btn');
  await expect(page.locator('#webinars')).toBeVisible();
  await expect(page.locator('#home')).toBeHidden();
}

test('eligible host sees the prominent homepage CTA and it opens the Webinars hub', async ({ browser }) => {
  const t = await openPage(browser);
  const errors = trackConsoleErrors(t.page);
  await mockApi(t.page, { postBody: null, patchBody: null });
  await seedSession(t.page);

  await t.page.goto('/', { waitUntil: 'networkidle' });
  await expect(t.page.locator('#home')).toBeVisible();
  await openWebinars(t.page);

  expect(errors).toEqual([]);
  await closePage(t);
});

test('scheduling a webinar reveals the schedule fields, enables end after a start, and sends the schedule', async ({ browser }) => {
  const t = await openPage(browser);
  const errors = trackConsoleErrors(t.page);
  const cap: Captured = { postBody: null, patchBody: null };
  await mockApi(t.page, cap);
  await seedSession(t.page);

  await t.page.goto('/', { waitUntil: 'networkidle' });
  await openWebinars(t.page);
  await t.page.click('#webinar-create-toggle');

  // Default mode is "Avvia ora" → schedule fields hidden.
  await expect(t.page.locator('#webinar-schedule-fields')).toBeHidden();

  // Switch to "Programma" → the schedule fields appear; the end input starts disabled.
  await t.page.click('#webinar-mode-schedule');
  await expect(t.page.locator('#webinar-schedule-fields')).toBeVisible();
  await expect(t.page.locator('#webinar-end')).toBeDisabled();

  // Fill the form. Setting a start enables the end input.
  await t.page.fill('#webinar-title', 'Scheduled launch');
  await t.page.selectOption('#webinar-lang', 'en');
  await t.page.fill('#webinar-start', '2030-01-01T10:00');
  await expect(t.page.locator('#webinar-end')).toBeEnabled();
  await t.page.fill('#webinar-end', '2030-01-01T11:00');

  // Make it public so the notify-friends toggle applies, then create.
  await t.page.click('#webinar-visibility-public');
  await expect(t.page.locator('#webinar-notify-friends')).toBeVisible();
  await t.page.click('#webinar-create-btn');

  // The POST carried the schedule + notify_friends.
  await expect.poll(() => cap.postBody?.scheduled_start).toBeTruthy();
  expect(cap.postBody?.scheduled_end).toBeTruthy();
  expect(cap.postBody?.visibility).toBe('public');
  expect(cap.postBody?.notify_friends).toBe(true);

  expect(errors).toEqual([]);
  await closePage(t);
});

test('editing a scheduled webinar through the detail modal PATCHes the change', async ({ browser }) => {
  const t = await openPage(browser);
  const errors = trackConsoleErrors(t.page);
  const cap: Captured = { postBody: null, patchBody: null };
  await mockApi(t.page, cap);
  await seedSession(t.page);

  await t.page.goto('/', { waitUntil: 'networkidle' });
  await openWebinars(t.page);

  // Create a webinar (immediate mode is fine — the mock returns a scheduled one).
  await t.page.click('#webinar-create-toggle');
  await t.page.fill('#webinar-title', 'Product launch');
  await t.page.selectOption('#webinar-lang', 'en');
  await t.page.click('#webinar-create-btn');
  const card = t.page.locator('.webinar-card');
  await expect(card).toHaveCount(1);

  // Open the detail modal → the edit section is present for a scheduled webinar.
  await card.locator('.webinar-view-details-btn').click();
  const modal = t.page.locator('#webinar-detail-modal');
  await expect(modal).toBeVisible();
  await expect(modal.locator('#wdm-edit-section')).toBeVisible();

  // Change the title and save → a PATCH goes out with the new title.
  await modal.locator('#wde-title').fill('Renamed launch');
  await modal.locator('#wde-save-btn').click();
  await expect.poll(() => cap.patchBody?.title).toBe('Renamed launch');

  expect(errors).toEqual([]);
  await closePage(t);
});

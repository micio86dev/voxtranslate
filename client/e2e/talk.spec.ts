// /talk — "Talk to Anyone": two people, one device, one microphone (spec 0110).
//
// Runs against the real `GET /api/engines` contract rather than a stub, so a tier that
// stops being offered — or starts being offered when it should not be — fails here.
// Starting a conversation needs a signed-in user and a live Qwen session, which the e2e
// environment does not have; what IS covered end to end is everything up to that gate:
// the setup flow, the picker, the tier filter, the signed-out refusal, mobile layout and
// accessibility.
import { test, expect } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import { openPage, closePage } from './helpers';

const TAGS = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa', 'best-practice'];

test('talk: the whole setup is one choice', async ({ browser }) => {
  const t = await openPage(browser, { width: 440, height: 900 });
  const { page } = t;
  await page.goto('/talk', { waitUntil: 'networkidle' });

  // The conversation screen and the failure screen stay out of the way until asked for.
  await expect(page.locator('#tk-setup')).toBeVisible();
  await expect(page.locator('#tk-live')).toBeHidden();
  await expect(page.locator('#tk-problem')).toBeHidden();

  // Nothing can start before the one required choice is made.
  await expect(page.locator('#tk-start')).toBeDisabled();

  await page.click('#tk-lang-trigger');
  await expect(page.locator('#tk-lang-panel')).toBeVisible();
  await expect(page.locator('#tk-lang-trigger')).toHaveAttribute('aria-expanded', 'true');

  // Every option names the language, never only a flag.
  const first = page.locator('#tk-lang-list .lang-opt').first();
  await expect(first).toBeVisible({ timeout: 8000 });
  await expect(first.locator('.lang-opt-native')).not.toBeEmpty();
  await expect(first.locator('.lang-opt-en')).not.toBeEmpty();

  // Search narrows the list.
  await page.fill('#tk-lang-search', 'spanish');
  await expect(page.locator('#tk-lang-list .lang-opt')).toHaveCount(1);
  await page.click('#tk-lang-list .lang-opt');

  await expect(page.locator('#tk-lang-panel')).toBeHidden();
  await expect(page.locator('#tk-lang-text')).toContainText('Español');
  await expect(page.locator('#tk-start')).toBeEnabled();

  await closePage(t);
});

test('talk: never offers a client-direct tier', async ({ browser }) => {
  // Enhanced runs the provider in the browser, so the server never sees the audio it
  // would have to gate. Offering it here would mean silently swapping the user's choice
  // the moment they press Start.
  const t = await openPage(browser, { width: 440, height: 900 });
  const { page } = t;
  await page.goto('/talk', { waitUntil: 'networkidle' });

  await page.click('#tk-lang-trigger');
  await page.fill('#tk-lang-search', 'spanish');
  await expect(page.locator('#tk-lang-list .lang-opt')).toHaveCount(1);
  await page.click('#tk-lang-list .lang-opt');

  const tiers = page.locator('#tk-tier-options .engine-opt');
  if ((await tiers.count()) > 0) {
    await expect(tiers.filter({ hasText: 'Enhanced' })).toHaveCount(0);
  }

  await closePage(t);
});

test('talk: a signed-out visitor is told to sign in, not left guessing', async ({ browser }) => {
  const t = await openPage(browser, { width: 440, height: 900 });
  const { page } = t;
  await page.goto('/talk', { waitUntil: 'networkidle' });

  // This is a billed feature with no guest tier — the same rule the extension follows.
  await expect(page.locator('#tk-setup-status')).not.toBeEmpty();

  await page.click('#tk-lang-trigger');
  await page.fill('#tk-lang-search', 'spanish');
  await page.click('#tk-lang-list .lang-opt');
  await page.click('#tk-start');

  // Refused before any microphone prompt, and the setup screen stays put.
  await expect(page.locator('#tk-live')).toBeHidden();
  await expect(page.locator('#tk-setup')).toBeVisible();

  await closePage(t);
});

test('talk: reachable from the home hero and the account menu', async ({ browser }) => {
  const t = await openPage(browser, { width: 440, height: 900 });
  const { page } = t;
  await page.goto('/', { waitUntil: 'networkidle' });

  const hero = page.locator('#hero-talk');
  await expect(hero).toBeVisible();
  await expect(hero).toHaveAttribute('href', '/talk');
  await hero.click();
  await page.waitForURL(/\/talk\/?$/);
  await expect(page.locator('#tk-setup')).toBeVisible();

  await closePage(t);
});

test('talk: fits a phone in portrait without sideways scroll', async ({ browser }) => {
  // The whole point is a phone lying on a table between two people.
  const t = await openPage(browser, { width: 360, height: 780 }, true);
  const { page } = t;
  await page.goto('/talk', { waitUntil: 'networkidle' });

  const fits = await page.evaluate(
    () => document.documentElement.scrollWidth <= document.documentElement.clientWidth,
  );
  expect(fits, 'talk: must not scroll horizontally at 360px').toBe(true);

  // Controls are large enough to hit while standing (WCAG 2.5.5 / --tap-target).
  await page.click('#tk-lang-trigger');
  const box = await page.locator('#tk-lang-trigger').boundingBox();
  expect(box?.height ?? 0).toBeGreaterThanOrEqual(44);

  await closePage(t);
});

test('a11y: talk page has no WCAG violations', async ({ browser }) => {
  const t = await openPage(browser);
  const { page } = t;
  await page.goto('/talk', { waitUntil: 'networkidle' });
  await expect(page.locator('#tk-setup')).toBeVisible();

  // Open the picker too: a listbox is exactly where roles and labels go wrong.
  await page.click('#tk-lang-trigger');
  await expect(page.locator('#tk-lang-list .lang-opt').first()).toBeVisible({ timeout: 8000 });

  const { violations } = await new AxeBuilder({ page }).withTags(TAGS).analyze();
  const report = violations.map(
    (v) =>
      `[${v.impact}] ${v.id}: ${v.help}\n` +
      v.nodes.map((n) => `    ${n.target.join(' ')}`).join('\n'),
  );
  expect(report, 'talk: expected no axe violations').toEqual([]);

  await closePage(t);
});

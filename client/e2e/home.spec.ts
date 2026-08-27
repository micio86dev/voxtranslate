import { test, expect } from '@playwright/test';
import { openPage, closePage } from './helpers';

test('home: i18n switching, hero CTAs, PWA tags', async ({ browser }) => {
  const t = await openPage(browser, { width: 440, height: 900 });
  const { page } = t;
  await page.goto('/', { waitUntil: 'networkidle' });

  // i18n follows the language selector. Non-English locale dictionaries now lazy-load
  // (spec 0104), so the label updates once its chunk resolves — use retrying assertions
  // rather than one-shot reads.
  await page.selectOption('#lang', 'de');
  await expect(page.locator('#enter')).toHaveText('Beitreten');
  await page.selectOption('#lang', 'ja');
  await expect(page.locator('#enter')).toHaveText('参加');
  await page.selectOption('#lang', 'en');
  await expect(page.locator('#enter')).toHaveText('Enter room');

  // Dice regenerates the room code.
  const before = await page.inputValue('#room');
  await page.click('#dice');
  expect(await page.inputValue('#room')).not.toBe(before);

  // Visibility toggle shows the private hint.
  await page.click('.seg-btn[data-vis="private"]');
  expect(((await page.textContent('#vis-hint')) || '').length).toBeGreaterThan(0);
  await page.click('.seg-btn[data-vis="public"]');

  // PWA: manifest + theme.
  const pwa = await page.evaluate(async () => ({
    theme: document.querySelector('meta[name=theme-color]')?.getAttribute('content'),
    name: (await (await fetch('/manifest.webmanifest')).json()).name,
  }));
  expect(pwa.theme).toBe('#0871ab');
  expect(pwa.name).toBe('VoxTranslate');

  // Hero: discovery routes to /world. (Public-room discovery itself is covered by
  // world.spec.ts, where it now lives.)
  await expect(page.locator('#hero-world')).toHaveAttribute('href', '/world');
  await expect(page.locator('h1.hero-title')).toBeVisible();

  // ...but only for someone who can actually use it. Talk to the World is public-room
  // discovery, and public rooms require an account, so showing a guest the front door to
  // a room they get bounced out of is a dead end. The other two CTAs stay: a private room
  // and a face-to-face conversation are both reachable without one (Talk to Anyone
  // explains the sign-in on its own screen rather than vanishing from the home).
  const guest = await page.evaluate(() => !localStorage.getItem('vox.token'));
  const worldHidden = await page
    .locator('#hero-world')
    .evaluate((el) => el.classList.contains('hidden'));
  if (guest) {
    expect(worldHidden, 'Talk to the World must be hidden from a guest').toBe(true);
    await expect(page.locator('#hero-talk')).toBeVisible();
    await expect(page.locator('#hero-private')).toBeVisible();
  }

  // The secondary CTA reuses the create flow: private preselected, cursor in the field.
  await page.click('#hero-private');
  await expect(page.locator('.seg-btn[data-vis="private"]')).toHaveAttribute(
    'aria-pressed',
    'true',
  );
  await expect(page.locator('#room')).toBeFocused();

  await closePage(t);
});

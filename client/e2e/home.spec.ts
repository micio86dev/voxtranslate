import { test, expect } from '@playwright/test';
import { openPage, closePage, NodePeer } from './helpers';

test('home: i18n switching, lobby tap-to-join, PWA tags', async ({ browser }) => {
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

  // Lobby lists a seeded public room; tapping it opens pre-join.
  const seed = new NodePeer('lobby' + Math.floor(Math.random() * 1e6), 'it', 'Marco');
  await seed.ready;
  await page.click('#refresh');
  await page.waitForSelector('.room-item', { timeout: 8000 });
  await page.click('.room-item');
  await page.waitForSelector('#prejoin:not(.hidden)', { timeout: 8000 });
  seed.close();

  await closePage(t);
});

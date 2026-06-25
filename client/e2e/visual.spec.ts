// Visual-regression baselines (Playwright `toHaveScreenshot` — pixel diff with an
// overlay/diff image on mismatch). Captures the stable, backend-independent home
// screen at desktop + mobile widths. Dynamic bits (the random room code, the live
// lobby list) are masked so the baseline is deterministic; animations are disabled.
//
// Baselines live in `e2e/visual.spec.ts-snapshots/` and are OS-specific (Playwright
// suffixes them, e.g. `-darwin`). Regenerate with:
//   npx playwright test visual.spec.ts --update-snapshots
import { test, expect } from '@playwright/test';
import { openPage, closePage } from './helpers';

// Mask selectors that legitimately change between runs.
const DYNAMIC = ['#room', '#rooms'];

async function settleHome(page: import('@playwright/test').Page) {
  await page.goto('/', { waitUntil: 'networkidle' });
  // Deterministic language + fonts ready, so glyph metrics don't shift the layout.
  await page.selectOption('#lang', 'en');
  await expect(page.locator('#enter')).toHaveText('Enter room');
  await page.evaluate(() => (document as Document & { fonts: FontFaceSet }).fonts.ready);
}

test('visual: home screen (desktop)', async ({ browser }) => {
  const t = await openPage(browser, { width: 1280, height: 900 });
  await settleHome(t.page);
  await expect(t.page).toHaveScreenshot('home-desktop.png', {
    fullPage: true,
    animations: 'disabled',
    mask: DYNAMIC.map((s) => t.page.locator(s)),
    maxDiffPixelRatio: 0.01,
  });
  await closePage(t);
});

test('visual: home screen (mobile)', async ({ browser }) => {
  const t = await openPage(browser, { width: 390, height: 844 }, true);
  await settleHome(t.page);
  await expect(t.page).toHaveScreenshot('home-mobile.png', {
    fullPage: true,
    animations: 'disabled',
    mask: DYNAMIC.map((s) => t.page.locator(s)),
    maxDiffPixelRatio: 0.01,
  });
  await closePage(t);
});

// The guest sign-in CTA under the tier cards (v1.47.0).
//
// This spec exists because the branch it covers is invisible to every other spec: it
// renders only when LANGUAGE_FIRST_UX is on AND accounts are enabled AND more than one
// tier is registered — the production shape. The default e2e backend is guest-only with a
// single tier, so elsewhere this code is dead. Rather than fail there, the spec asks the
// backend what shape it is in and skips when it cannot exercise the branch.
import { test, expect, type Browser } from '@playwright/test';
import { WS_HOST } from './helpers';

/** Whether the backend can produce the CTA at all: accounts on + >1 tier + language-first. */
async function productionShape(): Promise<boolean> {
  try {
    const res = await fetch(`http://${WS_HOST}/api/engines`);
    if (!res.ok) return false;
    const d = (await res.json()) as {
      engines?: { output_languages: string[] }[];
      flags?: { language_first_ux?: boolean };
    };
    const cfg = await fetch(`http://${WS_HOST}/api/auth/config`);
    return !!d.flags?.language_first_ux && (d.engines?.length ?? 0) > 1 && cfg.ok;
  } catch {
    return false;
  }
}

/** A guest page in a given browser locale — `openPage` cannot set one, and the locale IS
 *  the input under test (the CTA names the visitor's own language). */
async function guestPage(browser: Browser, locale: string) {
  const ctx = await browser.newContext({ locale, viewport: { width: 1280, height: 900 } });
  const page = await ctx.newPage();
  // Keep the first-run tour shut; it overlays the home screen (same reason as openPage).
  await page.addInitScript(() => {
    try {
      localStorage.setItem('vox_home_tour_seen', '1');
      localStorage.setItem('vox_call_tour_seen', '1');
      localStorage.setItem('vox_guest_consent', '1'); // blocking 18+/ToS modal
      localStorage.setItem('vox.cookie', 'accepted'); // consent banner
    } catch {
      /* storage blocked — the tour just stays open, assertions below still target #tier-note */
    }
  });
  await page.goto('/', { waitUntil: 'networkidle' });
  // With accounts enabled the app boots on the LOGIN screen, not home — a guest gets in
  // through "continue as guest". The other specs never see this because their backend is
  // guest-only, where boot goes straight home.
  const guestBtn = page.locator('#guest-btn');
  if (await guestBtn.isVisible().catch(() => false)) await guestBtn.click();
  await page.waitForSelector('#home:not(.hidden)', { timeout: 15000 });
  return { ctx, page };
}

test('guest CTA names the browser language a guest cannot use', async ({ browser }) => {
  test.skip(!(await productionShape()), 'backend not in production shape (language-first + accounts + 2 tiers)');
  // Ukrainian is in the premium tier's 84 languages and NOT in Standard's 29.
  const { ctx, page } = await guestPage(browser, 'uk-UA');

  const note = page.locator('#tier-note');
  await expect(note).toBeVisible({ timeout: 10000 });
  await expect(note).toContainText('Українська'); // the endonym from the shared catalogue

  // The way out is the SAME sign-in gate the public-room flow uses, not a new path.
  await note.locator('a').click();
  await expect(page.locator('#signin-gate-modal')).toBeVisible({ timeout: 5000 });

  await ctx.close();
});

test('guest CTA falls back to a count when the browser language is already available', async ({ browser }) => {
  test.skip(!(await productionShape()), 'backend not in production shape (language-first + accounts + 2 tiers)');
  // English IS in Standard, so there is no single language worth naming.
  const { ctx, page } = await guestPage(browser, 'en-US');

  const note = page.locator('#tier-note');
  await expect(note).toBeVisible({ timeout: 10000 });
  await expect(note).toContainText('55'); // 84 premium languages − 29 Standard
  await expect(note).not.toContainText('Українська');

  await ctx.close();
});

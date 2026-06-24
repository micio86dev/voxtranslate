import { test, expect } from '@playwright/test';
import { openPage, closePage, trackConsoleErrors } from './helpers';

// Onboarding tour & help system (spec onboarding). These run guest-only against the preview
// build with NO backend: billing resolves false (→ enterHome), so the home wizard auto-opens.
// The call tour is launched on a revealed #call screen (the join path is covered by call.spec.ts)
// — the tour is independent of how the user got into the call.

test('onboarding home wizard: auto-open, step nav, persistence, relaunch, a11y', async ({ browser }) => {
  const t = await openPage(browser, { width: 1100, height: 860 }, false, false);
  const { page } = t;
  const errors = trackConsoleErrors(page);
  await page.goto('/', { waitUntil: 'networkidle' });

  const modal = page.locator('#onb-home-modal');

  // Auto-opens on first visit and records the "seen" flag.
  await expect(modal).not.toHaveClass(/hidden/);
  expect(await page.evaluate(() => localStorage.getItem('vox_home_tour_seen'))).toBe('1');
  // Guest flow: the credits step is excluded, the sign-up step is included → 5 steps.
  await expect(page.locator('#onb-home-dots .onb-dot')).toHaveCount(5);
  await expect(page.locator('#onb-home-title')).toHaveText('Welcome to VoxTranslate');
  // Focus lands on the primary action (keyboard users can advance immediately).
  expect(await page.evaluate(() => document.activeElement?.id)).toBe('onb-home-next');

  // Dismiss the cookie banner (bottom-fixed) so it can't intercept the FAB later.
  await page.evaluate(() => document.getElementById('cookie-accept')?.click());

  // Back is hidden on step 1; Next advances; the active dot tracks the step.
  await expect(page.locator('#onb-home-back')).toBeHidden();
  const activeIdx = () =>
    page.evaluate(() =>
      [...document.querySelectorAll('#onb-home-dots .onb-dot')].findIndex((d) =>
        d.classList.contains('active'),
      ),
    );
  await page.click('#onb-home-next');
  await expect(page.locator('#onb-home-back')).toBeVisible();
  expect(await activeIdx()).toBe(1);

  // Arrow keys navigate (←/→).
  await page.keyboard.press('ArrowRight');
  expect(await activeIdx()).toBe(2);
  await page.keyboard.press('ArrowLeft');
  expect(await activeIdx()).toBe(1);

  // Walk to the last step → Next becomes the CTA and finishing closes the wizard.
  for (let i = 0; i < 6; i++) {
    const last = await page.evaluate(() => {
      const d = [...document.querySelectorAll('#onb-home-dots .onb-dot')];
      return d.findIndex((x) => x.classList.contains('active')) === d.length - 1;
    });
    if (last) break;
    await page.click('#onb-home-next');
  }
  await expect(page.locator('#onb-home-next')).toHaveText('Start talking');
  await page.click('#onb-home-next');
  await expect(modal).toHaveClass(/hidden/);

  // Does NOT re-open on reload once seen.
  await page.reload({ waitUntil: 'networkidle' });
  await expect(modal).toHaveClass(/hidden/);
  await page.evaluate(() => document.getElementById('cookie-accept')?.click());

  // The "?" launcher reopens it regardless of the flag; ESC closes it (app overlay handler).
  await page.click('#onb-home-help');
  await expect(modal).not.toHaveClass(/hidden/);
  await page.keyboard.press('Escape');
  await expect(modal).toHaveClass(/hidden/);

  expect(errors).toEqual([]);
  await closePage(t);
});

test('onboarding call tour: spotlight, ⋯ force-open for share, relaunch flag, release', async ({
  browser,
}) => {
  const t = await openPage(browser, { width: 1200, height: 860 }, false, false);
  const { page } = t;
  const errors = trackConsoleErrors(page);
  await page.goto('/', { waitUntil: 'networkidle' });

  // Clear the home wizard + cookie banner, then reveal the call screen (no live call needed).
  await page.evaluate(() => {
    document.getElementById('onb-home-skip')?.click();
    document.getElementById('cookie-accept')?.click();
    document.querySelectorAll('.screen').forEach((s) => s.classList.add('hidden'));
    document.getElementById('call')?.classList.remove('hidden');
  });

  // Launch via "?"; the first popover is the centered intro, and the flag is recorded.
  await page.click('#onb-call-help');
  await expect(page.locator('.driver-popover')).toBeVisible();
  await expect(page.locator('.driver-popover-title')).toHaveText('Welcome to the call');
  expect(await page.evaluate(() => localStorage.getItem('vox_call_tour_seen'))).toBe('1');

  // Next → the mic control gets the spotlight ring (driver's active-element class).
  await page.click('.driver-popover-next-btn');
  await expect(page.locator('#btn-mic')).toHaveClass(/driver-active-element/);

  // Advance until the ⋯ overflow menu is force-opened for the Share step.
  for (let i = 0; i < 9; i++) {
    if (await page.locator('body.onb-more-forced').count()) break;
    await page.click('.driver-popover-next-btn');
  }
  await expect(page.locator('body.onb-more-forced')).toHaveCount(1);
  await expect(page.locator('#more-menu')).toBeVisible();
  await expect(page.locator('#btn-share')).toHaveClass(/driver-active-element/);

  // Finish the tour → the forced menu is released on destroy.
  for (let i = 0; i < 6; i++) {
    if (!(await page.locator('.driver-popover').count())) break;
    await page.click('.driver-popover-next-btn');
  }
  await expect(page.locator('.driver-popover')).toHaveCount(0);
  await expect(page.locator('body.onb-more-forced')).toHaveCount(0);

  expect(errors).toEqual([]);
  await closePage(t);
});

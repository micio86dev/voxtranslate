// Audio Settings modal reached from pre-join. Vox Voices is currently DISABLED
// (config VOX_ENABLED=false — too slow on CPU/wasm for live use), so the modal is just
// the Browser Voice picker: the engine field, the pack/install/benchmark blocks and the
// Vox voice list all stay hidden. This pins the modal's open → populate → close flow
// (and a clean console) without a backend, peers, or an installed pack.
import { test, expect } from '@playwright/test';
import { openPage, closePage, trackConsoleErrors } from './helpers';

test('audio settings: Browser Voice picker only, all Vox UI hidden (Vox disabled)', async ({
  browser,
}) => {
  const t = await openPage(browser);
  const errors = trackConsoleErrors(t.page);

  // Home → pre-join (no join needed; the audio button lives on the pre-join screen).
  await t.page.goto('/', { waitUntil: 'networkidle' });
  await t.page.selectOption('#lang', 'en');
  await t.page.fill('#name', 'Alice');
  await t.page.fill('#room', `aud-${Math.random().toString(36).slice(2, 8)}`);
  await t.page.click('#enter');
  await t.page.waitForSelector('#prejoin:not(.hidden)');

  // Open the Audio Settings modal.
  await t.page.click('#prejoin-audio-btn');
  await expect(t.page.locator('#audio-modal')).toBeVisible();

  // Vox disabled → no engine to choose: the engine-preference field is hidden (there is
  // no vox option at all), and every Vox-specific block stays hidden.
  await expect(t.page.locator('#audio-engine-select')).toBeHidden();
  await expect(t.page.locator('#audio-engine-select option[value="vox"]')).toHaveCount(0);
  await expect(t.page.locator('#audio-pack')).toBeHidden();
  await expect(t.page.locator('#audio-vox-voices')).toBeHidden();

  // The browser voice radiogroup renders (either voices or the friendly empty note).
  await expect(t.page.locator('#audio-browser-list')).toBeVisible();

  // Close returns to pre-join with the modal gone.
  await t.page.click('#audio-close');
  await expect(t.page.locator('#audio-modal')).toBeHidden();
  await expect(t.page.locator('#prejoin')).toBeVisible();

  expect(errors).toEqual([]);
  await closePage(t);
});

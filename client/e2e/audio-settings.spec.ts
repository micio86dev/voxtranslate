// Audio Settings modal (spec 0093 Vox Voices UI) reached from pre-join. The e2e build
// sets no PUBLIC_VOX_MANIFEST_URL, so Vox Voices is DORMANT: the pack/install and Vox
// voice blocks stay hidden and the engine picker offers Browser Voice only. This pins
// the modal's open → populate → close flow (and a clean console) without a backend,
// peers, or an installed pack.
import { test, expect } from '@playwright/test';
import { openPage, closePage, trackConsoleErrors } from './helpers';

test('audio settings: engine picker + browser voices, Vox dormant without a manifest', async ({
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

  // The status block reports a current engine, and the engine picker is populated
  // (Browser Voice is always available). Vox is offered but DISABLED until a pack is
  // installed — with no manifest it can never install, so it stays a dead option.
  await expect(t.page.locator('#audio-current-engine')).not.toBeEmpty();
  const engineOptions = t.page.locator('#audio-engine-select option');
  await expect(engineOptions).not.toHaveCount(0);
  const engineValues = await engineOptions.evaluateAll((os) =>
    os.map((o) => (o as HTMLOptionElement).value),
  );
  expect(engineValues).toContain('browser');
  // (`<option>` disabled state — Playwright's toBeDisabled() doesn't read it, so assert
  // the DOM property directly.)
  await expect(t.page.locator('#audio-engine-select option[value="vox"]')).toHaveJSProperty(
    'disabled',
    true,
  );

  // Feature dormant (no manifest) → the pack card and the Vox voice list stay hidden.
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

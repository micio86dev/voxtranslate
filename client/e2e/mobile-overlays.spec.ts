// Mobile in-call overlay layout (regression coverage).
//
// The existing meet-ui spec runs at a DESKTOP viewport (hover-capable), so it never
// exercised the `@media (hover: none)` path — where every `.video-cell` was lifted to
// z-index 9, painting the video OVER the z-8 on-video overlays (top-left meta cluster +
// top-right participant counter) so they vanished on phones; and the per-cell report/
// block buttons sat under the counter. These tests run a MOBILE context (isMobile +
// hasTouch → coarse pointer → the `hover: none` rules apply) with real browser peers,
// and assert the overlays are visible AND geometrically non-overlapping, in both the
// grid and the speaker/focus ("riquadro") layouts, with a clean console.
import { test, expect } from '@playwright/test';
import {
  openPage,
  closePage,
  joinCall,
  trackConsoleErrors,
  rectsOverlap,
  sleep,
  type Tracked,
} from './helpers';

const MOBILE = { width: 390, height: 844 };

test.describe('mobile in-call overlays', () => {
  test('grid view: meta cluster, counter and report/ban are visible and non-overlapping (1→4 users)', async ({
    browser,
  }, testInfo) => {
    const room = `mob-${Math.random().toString(36).slice(2, 8)}`;
    // Subject is mobile → triggers the `hover: none` + `max-width: 600px` rules.
    const subject = await openPage(browser, MOBILE, true);
    const errors = trackConsoleErrors(subject.page);
    await joinCall(subject.page, { name: 'Alessandro', lang: 'it', room });

    // Solo (1 user): the whole top-left meta cluster must be VISIBLE on mobile — this is
    // the exact regression (cells z9 over the z8 overlays hid all of this).
    await expect(subject.page.locator('.stage-meta #call-room')).toBeVisible();
    await expect(subject.page.locator('#stage-self-name')).toBeVisible(); // name/lang restored on mobile
    await expect(subject.page.locator('#stage-self-lang')).toBeVisible();
    await expect(subject.page.locator('.stage-info-strip #call-vis')).toBeVisible();
    await expect(subject.page.locator('.stage-info-strip #session-timer')).toBeVisible();
    await expect(subject.page.locator('.stage-info-strip #call-balance')).toBeAttached();
    await expect(subject.page.locator('#part-count')).toBeVisible();

    // Grow the room to 4 with real browser peers.
    const peers: Tracked[] = [];
    for (const [name, lang] of [
      ['Bob', 'en'],
      ['Cara', 'es'],
      ['Dan', 'fr'],
    ] as const) {
      const p = await openPage(browser);
      await joinCall(p.page, { name, lang, room });
      peers.push(p);
    }
    await expect(subject.page.locator('#part-count-n')).toHaveText('4');
    await expect(subject.page.locator('.video-grid .video-cell')).toHaveCount(4);
    await sleep(500); // let the grid settle

    await testInfo.attach('grid-4-mobile.png', {
      body: await subject.page.screenshot(),
      contentType: 'image/png',
    });

    // The two global clusters must not overlap each other.
    const meta = await subject.page.locator('.stage-meta-wrap').boundingBox();
    const counter = await subject.page.locator('#part-count').boundingBox();
    expect(rectsOverlap(meta, counter), 'meta cluster overlaps the participant counter').toBe(false);

    // Every remote cell's report/block buttons are visible (touch → always shown) and
    // clear of BOTH global clusters (they were moved to the cell bottom).
    const actions = subject.page.locator('.video-grid .cell-actions');
    expect(await actions.count()).toBe(3); // one per remote cell; self has none
    for (let i = 0; i < 3; i++) {
      await expect(actions.nth(i)).toBeVisible();
      const ab = await actions.nth(i).boundingBox();
      expect(rectsOverlap(ab, counter), `report/ban #${i} overlaps the counter`).toBe(false);
      expect(rectsOverlap(ab, meta), `report/ban #${i} overlaps the meta cluster`).toBe(false);
    }

    expect(errors, `console errors during the flow:\n${errors.join('\n')}`).toEqual([]);

    for (const p of peers) await closePage(p);
    await closePage(subject);
  });

  test('speaker/focus view: thumbnails stack without overlapping each other or the overlays', async ({
    browser,
  }, testInfo) => {
    const room = `mobf-${Math.random().toString(36).slice(2, 8)}`;
    const subject = await openPage(browser, MOBILE, true);
    const errors = trackConsoleErrors(subject.page);
    await joinCall(subject.page, { name: 'Alessandro', lang: 'it', room });

    const peers: Tracked[] = [];
    for (const [name, lang] of [
      ['Bob', 'en'],
      ['Cara', 'es'],
      ['Dan', 'fr'],
    ] as const) {
      const p = await openPage(browser);
      await joinCall(p.page, { name, lang, room });
      peers.push(p);
    }
    await expect(subject.page.locator('.video-grid .video-cell')).toHaveCount(4);

    // Pin a remote peer → focus/speaker layout (one main cell + a thumbnail column).
    await subject.page.locator('.video-cell:not(.self) .pin-btn').first().click();
    await expect(subject.page.locator('#video-grid[data-mode="focus"]')).toBeAttached();
    await sleep(500);

    await testInfo.attach('focus-4-mobile.png', {
      body: await subject.page.screenshot(),
      contentType: 'image/png',
    });

    // Thumbnails must NOT pile on the same spot (the stacking bug): pairwise no-overlap.
    const thumbs = subject.page.locator('.video-grid .video-cell.video-thumb');
    expect(await thumbs.count()).toBe(3); // the 3 non-pinned cells
    const boxes = [];
    for (let i = 0; i < 3; i++) boxes.push(await thumbs.nth(i).boundingBox());
    for (let i = 0; i < 3; i++)
      for (let j = i + 1; j < 3; j++)
        expect(rectsOverlap(boxes[i], boxes[j]), `thumbnails ${i} and ${j} overlap`).toBe(false);

    // Global overlays still visible; the main cell's report/ban clear of counter + thumbs.
    await expect(subject.page.locator('.stage-meta #call-room')).toBeVisible();
    await expect(subject.page.locator('#part-count')).toBeVisible();
    const counter = await subject.page.locator('#part-count').boundingBox();
    const mainActions = await subject.page.locator('.video-cell.main-cell .cell-actions').boundingBox();
    expect(rectsOverlap(mainActions, counter), 'main-cell actions overlap the counter').toBe(false);
    for (let i = 0; i < 3; i++)
      expect(rectsOverlap(mainActions, boxes[i]), `main-cell actions overlap thumbnail ${i}`).toBe(false);

    expect(errors, `console errors during the flow:\n${errors.join('\n')}`).toEqual([]);

    for (const p of peers) await closePage(p);
    await closePage(subject);
  });
});

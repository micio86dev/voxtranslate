// Regression guard (mobile): a reaction must paint ABOVE the video tiles.
// When you're alone a single cell fills the whole stage, so a reaction at the old
// z-index 6 was painted behind it and the tap looked dead (#228). The reaction-float
// is explicitly raised (z-index 10) to clear the tiles. NOTE: the touch `.video-cell`
// is NO LONGER lifted to z-9 — that lift painted cells over the z-8 on-video overlays
// and hid them on mobile, so it was removed; the cell now stays in normal flow (auto)
// and the reaction's own positive z keeps it on top. We assert the durable invariant
// (reaction above cell), not the exact cell z, against the real `@media (hover: none)`
// stylesheet by mounting the scoped structure the app renders.
import { test, expect } from '@playwright/test';

test('mobile: reaction-float stacks above the touch-raised video cell', async ({ browser }) => {
  const ctx = await browser.newContext({
    viewport: { width: 390, height: 844 }, // iPhone-ish
    isMobile: true,
    hasTouch: true,
    serviceWorkers: 'block',
  });
  const page = await ctx.newPage();
  await page.goto('/', { waitUntil: 'networkidle' });

  // This is the rule that flips on a real phone (touch device → no hover).
  expect(await page.evaluate(() => matchMedia('(hover: none)').matches), '(hover: none)').toBe(true);

  const z = await page.evaluate(() => {
    // index.astro's scoped rules key off a data-astro-cid-* attribute; reuse the
    // real one so `.video-stage`/`.video-grid` scoped selectors match our nodes.
    const scoped = document.querySelector('[class] , [id]');
    const cidAttr =
      Array.from(document.querySelector('select#lang')?.attributes ?? [])
        .map((a) => a.name)
        .find((n) => n.startsWith('data-astro-cid-')) ?? '';

    const stage = document.createElement('div');
    stage.className = 'video-stage';
    if (cidAttr) stage.setAttribute(cidAttr, '');
    const grid = document.createElement('div');
    grid.className = 'video-grid';
    if (cidAttr) grid.setAttribute(cidAttr, '');
    const cell = document.createElement('div');
    cell.className = 'video-cell';
    const float = document.createElement('div');
    float.className = 'reaction-float';
    grid.appendChild(cell);
    stage.appendChild(grid);
    stage.appendChild(float);
    document.body.appendChild(stage);

    return {
      cidAttr,
      cell: getComputedStyle(cell).zIndex,
      float: getComputedStyle(float).zIndex,
    };
  });

  console.log('scope attr:', z.cidAttr, '| z-index → video-cell:', z.cell, '| reaction-float:', z.float);
  const floatZ = Number(z.float);
  const cellZ = z.cell === 'auto' ? 0 : Number(z.cell);
  expect(floatZ, 'reaction-float must be explicitly raised (positive z-index)').toBeGreaterThan(0);
  // The cell must not be lifted at/above the reaction (the #228 bug was the cell hiding it).
  expect(cellZ, 'reaction-float must stack above the video cell').toBeLessThan(floatZ);

  await ctx.close();
});

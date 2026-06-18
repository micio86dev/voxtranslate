// Regression guard (mobile): a reaction must paint ABOVE the video tiles.
// On touch devices every .video-cell is raised to z-index 9 (#228); when you're
// alone a single cell fills the whole stage, so a reaction at the old z-index 6
// was painted behind it and the tap looked dead. We assert the fix directly
// against the production stylesheet under the REAL `@media (hover: none)` rule by
// mounting the same scoped structure the app renders and reading computed z-index.
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
  expect(z.cell, 'video-cell raised to 9 on touch (#228 rule)').toBe('9');
  expect(Number(z.float), 'reaction must stack above the cell').toBeGreaterThan(Number(z.cell));

  await ctx.close();
});

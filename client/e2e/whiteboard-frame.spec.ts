// The drawable-area affordance (#96 follow-up): the .wb-frame overlay must sit exactly
// on the letterboxed 16:9 sheet where drawing actually lands, so users can SEE the bounds.
import { test, expect } from '@playwright/test';
import { openPage, closePage, joinCall } from './helpers';

const LOGICAL_ASPECT = 1600 / 900;

test('whiteboard: drawable-area frame matches the letterboxed 16:9 sheet', async ({ browser }) => {
  const a = await openPage(browser);
  await joinCall(a.page, { name: 'Artist', lang: 'en', room: 'wbframe' + Math.floor(Math.random() * 1e6) });
  const page = a.page;

  await page.click('#btn-more');
  await page.click('#btn-whiteboard');
  await page.waitForSelector('#whiteboard:not(.hidden)');
  await page.keyboard.press('Escape');

  // resize() positions the frame on the next animation frame (ResizeObserver/rAF),
  // so wait for it to be laid out before measuring (instant for a real user).
  await page.waitForFunction(() => {
    const f = document.querySelector('.wb-frame') as HTMLElement | null;
    return !!f && f.offsetWidth > 100;
  });

  const m = await page.evaluate(() => {
    const c = document.getElementById('wb-canvas') as HTMLCanvasElement;
    const f = document.querySelector('.wb-frame') as HTMLElement;
    return {
      cw: c.clientWidth,
      ch: c.clientHeight,
      f: { x: f.offsetLeft, y: f.offsetTop, w: f.offsetWidth, h: f.offsetHeight },
      visible: getComputedStyle(f).display !== 'none',
    };
  });

  // Expected letterbox rect (mirrors contentRect()).
  let w = m.cw;
  let h = m.cw / LOGICAL_ASPECT;
  if (h > m.ch) { h = m.ch; w = m.ch * LOGICAL_ASPECT; }
  const exp = { x: (m.cw - w) / 2, y: (m.ch - h) / 2, w, h };

  console.log('canvas', m.cw, m.ch, '| frame', m.f, '| expected', exp);
  expect(m.visible).toBe(true);
  const near = (got: number, want: number) => expect(Math.abs(got - want)).toBeLessThanOrEqual(2);
  near(m.f.x, exp.x);
  near(m.f.y, exp.y);
  near(m.f.w, exp.w);
  near(m.f.h, exp.h);
  // And it really is a 16:9 sheet.
  expect(Math.abs(m.f.w / m.f.h - LOGICAL_ASPECT)).toBeLessThan(0.02);

  await closePage(a);
});

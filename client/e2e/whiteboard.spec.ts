// Advanced whiteboard (spec 0062 / #96): tool/width/colour pickers, multi-page
// navigation, and PNG/PDF export. The collaborative relay is covered by spec 0045's
// model; here we drive the new UI end-to-end.
import { test, expect } from '@playwright/test';
import { openPage, closePage, joinCall } from './helpers';

async function openBoard(page: import('@playwright/test').Page): Promise<void> {
  await page.click('#btn-more');
  await page.click('#btn-whiteboard');
  await page.waitForSelector('#whiteboard:not(.hidden)');
  await page.keyboard.press('Escape'); // close the ⋯ menu so the board is unobstructed
}

test('whiteboard: tools, multi-page, PNG/PDF export (spec 0062)', async ({ browser }) => {
  const a = await openPage(browser);
  await joinCall(a.page, { name: 'Artist', lang: 'en', room: 'wb' + Math.floor(Math.random() * 1e6) });
  const page = a.page;
  await openBoard(page);

  // R1 — tools + width: selecting a tool / width moves the .active state.
  await page.click('#wb-highlighter');
  await expect(page.locator('#wb-highlighter')).toHaveClass(/active/);
  await expect(page.locator('#wb-pen')).not.toHaveClass(/active/);
  await page.click('.wb-width[data-width="l"]');
  await expect(page.locator('.wb-width[data-width="l"]')).toHaveClass(/active/);

  // Draw a rectangle so the page has content to export.
  await page.click('#wb-rect');
  const box = (await page.locator('#wb-canvas').boundingBox())!;
  await page.mouse.move(box.x + box.width / 2 - 60, box.y + box.height / 2 - 40);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width / 2 + 60, box.y + box.height / 2 + 40, { steps: 5 });
  await page.mouse.up();

  // R2 — multi-page: the strip tracks "n / N" through add / nav / duplicate / delete.
  const label = page.locator('#wb-page-label');
  await expect(label).toHaveText('1 / 1');
  await expect(page.locator('#wb-page-prev')).toBeDisabled();
  await expect(page.locator('#wb-page-del')).toBeDisabled();

  await page.click('#wb-page-add');
  await expect(label).toHaveText('2 / 2');
  await expect(page.locator('#wb-page-prev')).toBeEnabled();
  await expect(page.locator('#wb-page-del')).toBeEnabled();

  await page.click('#wb-page-prev');
  await expect(label).toHaveText('1 / 2');

  await page.click('#wb-page-dup'); // duplicate page 1 → new page, switch to it
  await expect(label).toHaveText('3 / 3');

  await page.click('#wb-page-del'); // delete it → back to 2 pages, first page
  await expect(label).toHaveText('1 / 2');

  // R4 — export PNG: the menu opens and the download fires with a .png name.
  await page.click('#wb-export');
  await expect(page.locator('#wb-export-menu')).toBeVisible();
  const [png] = await Promise.all([page.waitForEvent('download'), page.click('#wb-export-png')]);
  expect(png.suggestedFilename()).toMatch(/whiteboard-page-\d+\.png$/);
  await expect(page.locator('#wb-export-menu')).toBeHidden(); // menu closes after a choice

  // R5 — export PDF: all pages → one whiteboard.pdf download.
  await page.click('#wb-export');
  const [pdf] = await Promise.all([page.waitForEvent('download'), page.click('#wb-export-pdf')]);
  expect(pdf.suggestedFilename()).toBe('whiteboard.pdf');

  await closePage(a);
});

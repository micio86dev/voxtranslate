// Automated WCAG 2.2 AA audit (axe-core) of the three main screens:
// home, pre-join, and in-call. Fails on ANY violation.
import { test, expect } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import type { Page } from '@playwright/test';
import { openPage, closePage, joinCall } from './helpers';

const TAGS = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa', 'best-practice'];

async function audit(page: Page, screen: string): Promise<void> {
  const { violations } = await new AxeBuilder({ page })
    .withTags(TAGS)
    // Audit visible UI only: hidden screens/modals carry display:none and are
    // skipped by axe anyway, but excluding video keeps the media engine out.
    .exclude('video')
    .analyze();
  const report = violations.map(
    (v) =>
      `[${v.impact}] ${v.id}: ${v.help}\n` +
      v.nodes.map((n) => `    ${n.target.join(' ')}`).join('\n'),
  );
  expect(report, `${screen}: expected no axe violations`).toEqual([]);
}

test('a11y: home screen has no WCAG violations', async ({ browser }) => {
  const t = await openPage(browser);
  await t.page.goto('/', { waitUntil: 'networkidle' });
  await audit(t.page, 'home');
  await closePage(t);
});

test('a11y: pre-join screen has no WCAG violations', async ({ browser }) => {
  const t = await openPage(browser);
  const { page } = t;
  await page.goto('/', { waitUntil: 'networkidle' });
  await page.selectOption('#lang', 'en');
  await page.fill('#name', 'AxeUser');
  await page.click('#enter');
  await page.waitForSelector('#prejoin:not(.hidden)');
  await audit(page, 'prejoin');
  await closePage(t);
});

test('a11y: in-call screen has no WCAG violations', async ({ browser }) => {
  const t = await openPage(browser);
  const { page } = t;
  await joinCall(page, {
    name: 'AxeUser',
    lang: 'en',
    room: 'axe' + Math.floor(Math.random() * 1e6),
  });
  await audit(page, 'call');

  // Also audit the two slide-in panels (chat + participants) while open.
  await page.click('#btn-chat');
  await page.waitForSelector('#chat-panel:not(.closed)');
  await audit(page, 'call+chat');
  await page.click('#btn-more'); // participants now lives in the ⋯ menu
  await page.click('#btn-participants');
  await page.waitForSelector('#participants-panel:not(.closed)');
  await audit(page, 'call+participants');

  await closePage(t);
});

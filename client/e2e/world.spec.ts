// /world — "Talk to the World" public conversation discovery.
//
// Seeds real rooms over the WebSocket (NodePeer) so the page is exercised against
// the live `GET /rooms` contract rather than a stub.
import { test, expect } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import { openPage, closePage, NodePeer } from './helpers';

const TAGS = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa', 'best-practice'];
const code = (prefix: string) => `${prefix}${Math.floor(Math.random() * 1e6)}`;

test('world: empty state invites you to start a conversation', async ({ browser }) => {
  const t = await openPage(browser, { width: 440, height: 900 });
  const { page } = t;
  await page.goto('/world', { waitUntil: 'networkidle' });

  // No live rooms seeded → the positive empty state, not "no rooms found".
  await expect(page.locator('#wr-empty')).toBeVisible();
  await expect(page.locator('#wr-list')).toBeHidden();
  await expect(page.locator('#wr-more')).toBeHidden();

  // Its CTA reuses the home create flow via the ?public= deep link.
  await page.click('#wr-empty [data-world-create]');
  await page.waitForURL(/\/\?public=1$/);
  await page.waitForSelector('#home:not(.hidden)', { timeout: 8000 });
  await expect(page.locator('#room')).toBeFocused();

  await closePage(t);
});

test('world: lists joinable rooms, caps the batch, and joins', async ({ browser }) => {
  const t = await openPage(browser, { width: 440, height: 900 });
  const { page } = t;

  // Seven live public rooms: more than one batch, so the cap is actually exercised.
  const seeds = Array.from({ length: 7 }, (_, i) => new NodePeer(code('world'), 'it', `Seed${i}`));
  await Promise.all(seeds.map((s) => s.ready));

  await page.goto('/world', { waitUntil: 'networkidle' });
  const cards = page.locator('.wr-card');
  await expect(cards.first()).toBeVisible({ timeout: 8000 });

  // At most five conversations per batch.
  expect(await cards.count()).toBeLessThanOrEqual(5);

  // Every listed room reads as a live conversation, never as a room record.
  await expect(cards.first().locator('.wr-people')).not.toBeEmpty();
  await expect(cards.first().locator('.wr-count')).toContainText('/ 4');
  await expect(cards.first().locator('.wr-cta')).toHaveText('Join conversation');

  // No horizontal overflow at a common phone width.
  const overflow = await page.evaluate(
    () => document.documentElement.scrollWidth <= document.documentElement.clientWidth,
  );
  expect(overflow, 'page must not scroll horizontally at 440px').toBe(true);

  // Tapping a card hands the room to the SPA, which opens pre-join.
  await cards.first().click();
  await page.waitForURL(/\/\?room=/);
  await page.waitForSelector('#prejoin:not(.hidden)', { timeout: 8000 });

  seeds.forEach((s) => s.close());
  await closePage(t);
});

test('world: another batch keeps the page populated', async ({ browser }) => {
  const t = await openPage(browser, { width: 440, height: 900 });
  const { page } = t;

  const seeds = Array.from({ length: 8 }, (_, i) => new NodePeer(code('more'), 'es', `Peer${i}`));
  await Promise.all(seeds.map((s) => s.ready));

  await page.goto('/world', { waitUntil: 'networkidle' });
  await expect(page.locator('.wr-card').first()).toBeVisible({ timeout: 8000 });

  const first = await page.locator('.wr-card').evaluateAll((els) =>
    els.map((e) => (e as HTMLElement).dataset.room),
  );

  await page.click('#wr-more');
  // The list never blanks — cards stay on screen while the next batch loads.
  await expect(page.locator('.wr-card').first()).toBeVisible();
  await expect
    .poll(async () =>
      page.locator('.wr-card').evaluateAll((els) =>
        els.map((e) => (e as HTMLElement).dataset.room).join(','),
      ),
    )
    .not.toBe(first.join(','));

  expect(await page.locator('.wr-card').count()).toBeLessThanOrEqual(5);

  seeds.forEach((s) => s.close());
  await closePage(t);
});

test('world: a full room is listed but not joinable', async ({ browser }) => {
  const t = await openPage(browser, { width: 440, height: 900 });
  const { page } = t;

  // One room at capacity (MAX_PEERS = 4) plus one with room to spare.
  const full = code('full');
  const seeds = [
    ...Array.from({ length: 4 }, (_, i) => new NodePeer(full, 'it', `Full${i}`)),
    new NodePeer(code('open'), 'ja', 'Open0'),
  ];
  await Promise.all(seeds.map((s) => s.ready));

  await page.goto('/world', { waitUntil: 'networkidle' });
  await expect(page.locator('.wr-card').first()).toBeVisible({ timeout: 8000 });

  // A 4/4 room is never offered as a batch pick.
  await expect(page.locator(`.wr-card[data-room="${full}"]`)).toHaveCount(0);

  seeds.forEach((s) => s.close());
  await closePage(t);
});

test('a11y: world page has no WCAG violations', async ({ browser }) => {
  const t = await openPage(browser);
  const { page } = t;
  const seeds = [new NodePeer(code('axe'), 'it', 'Axe0'), new NodePeer(code('axe'), 'de', 'Axe1')];
  await Promise.all(seeds.map((s) => s.ready));

  await page.goto('/world', { waitUntil: 'networkidle' });
  await expect(page.locator('.wr-card').first()).toBeVisible({ timeout: 8000 });

  const { violations } = await new AxeBuilder({ page }).withTags(TAGS).analyze();
  const report = violations.map(
    (v) =>
      `[${v.impact}] ${v.id}: ${v.help}\n` +
      v.nodes.map((n) => `    ${n.target.join(' ')}`).join('\n'),
  );
  expect(report, 'world: expected no axe violations').toEqual([]);

  seeds.forEach((s) => s.close());
  await closePage(t);
});

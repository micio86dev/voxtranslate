// Meet-like session UI (spec 0055): session-duration chip, live participant
// count, and one-tap quick reactions in the control bar.
import { test, expect } from '@playwright/test';
import { openPage, closePage, joinCall, NodePeer } from './helpers';

test('meet-ui: session timer ticks, live participant count, quick reactions (spec 0055)', async ({ browser }) => {
  const a = await openPage(browser);
  const room = `mui-${Math.random().toString(36).slice(2, 8)}`;
  await joinCall(a.page, { name: 'Alice', lang: 'en', room });

  // R1 — session-duration chip is visible and ticks (MM:SS, advancing).
  const elapsed = a.page.locator('#session-elapsed');
  await expect(a.page.locator('#session-timer')).toBeVisible();
  await expect(elapsed).toHaveText(/^\d{2}:\d{2}$/);
  const first = await elapsed.textContent();
  await expect.poll(() => elapsed.textContent(), { timeout: 4000 }).not.toBe(first);

  // R2 — participant count starts at 1 (self), rises to 2 when a peer joins,
  // and falls back to 1 when it leaves.
  const count = a.page.locator('#part-count-n');
  await expect(a.page.locator('#part-count')).toBeVisible();
  await expect(count).toHaveText('1');
  const bob = new NodePeer(room, 'it', 'Bob');
  await bob.ready;
  await expect(count).toHaveText('2');
  bob.close();
  await expect(count).toHaveText('1');

  // R3 — four quick-reaction buttons live in the control bar and fire without
  // opening a menu (no throw, button stays in place).
  const reactions = a.page.locator('#quick-reactions .react-btn');
  await expect(reactions).toHaveCount(4);
  await reactions.first().click();
  await expect(reactions).toHaveCount(4);

  await closePage(a);
});

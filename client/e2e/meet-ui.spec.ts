// Immersive in-call overlays (spec 0061 / #98): emoji reaction chips, an on-video
// meta cluster (clock | room ⓘ) top-left, a participant avatar badge top-right, and
// a room-info popover (visibility · duration · balance). Builds on spec 0055/0060.
import { test, expect } from '@playwright/test';
import { openPage, closePage, joinCall, NodePeer } from './helpers';

test('meet-ui: reaction chips, on-video clock/room/info + participant badge (spec 0061)', async ({ browser }) => {
  const a = await openPage(browser);
  const room = `mui-${Math.random().toString(36).slice(2, 8)}`;
  await joinCall(a.page, { name: 'Alice', lang: 'en', room });

  // R1 — reaction chips: four .react-btn live in the floating .reaction-bar tray and
  // fire on tap without opening/closing a menu (no throw, the chips stay put).
  const reactions = a.page.locator('#quick-reactions.reaction-bar .react-btn');
  await expect(a.page.locator('#quick-reactions.reaction-bar')).toBeVisible();
  await expect(reactions).toHaveCount(4);
  await reactions.first().click();
  await expect(reactions).toHaveCount(4);

  // R2 — on-video meta cluster, top-left: live wall-clock (HH:MM), the room code, and
  // the ⓘ info button are all visible over the video (no separate header row).
  await expect(a.page.locator('.stage-meta #header-clock')).toHaveText(/^\d{1,2}:\d{2}/);
  await expect(a.page.locator('.stage-meta #call-room')).toHaveText(new RegExp(room, 'i'));
  await expect(a.page.locator('.stage-meta #call-info')).toBeVisible();

  // R3 — participant badge, top-right: avatar initial (Alice → "A") + a live count that
  // starts at 1 (self), rises to 2 when a peer joins, and falls back to 1 on leave.
  const count = a.page.locator('#part-count-n');
  await expect(a.page.locator('#part-count')).toBeVisible();
  await expect(a.page.locator('#part-avatar')).toHaveText('A');
  await expect(count).toHaveText('1');
  const bob = new NodePeer(room, 'it', 'Bob');
  await bob.ready;
  await expect(count).toHaveText('2');
  bob.close();
  await expect(count).toHaveText('1');

  // R4 — room-info popover: the ⓘ button discloses the session duration (which ticks),
  // closed by default so it never crowds the video. Balance lives here too (#98).
  await expect(a.page.locator('#call-info-pop')).toBeHidden();
  await a.page.click('#call-info');
  await expect(a.page.locator('#call-info-pop')).toBeVisible();
  const elapsed = a.page.locator('#call-info-pop #session-elapsed');
  await expect(a.page.locator('#call-info-pop #session-timer')).toBeVisible();
  await expect(elapsed).toHaveText(/^\d{2}:\d{2}$/);
  const first = await elapsed.textContent();
  await expect.poll(() => elapsed.textContent(), { timeout: 4000 }).not.toBe(first);
  await expect(a.page.locator('#call-info-pop #call-balance')).toBeAttached();

  // Escape closes the popover (mirrors the other in-call popovers).
  await a.page.keyboard.press('Escape');
  await expect(a.page.locator('#call-info-pop')).toBeHidden();

  await closePage(a);
});

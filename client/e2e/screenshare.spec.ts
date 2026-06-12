import { test, expect } from '@playwright/test';
import type { Page } from '@playwright/test';
import { openPage, closePage, joinCall, sleep } from './helpers';

// Issue #4 (1): screen sharing must work independently of the camera. The hard
// case is a peer that joined WITHOUT a camera at all — historically there was no
// outgoing video m-line, so the shared screen never reached anyone. We simulate
// "no camera" by rejecting getUserMedia({video}) and stub getDisplayMedia with a
// deterministic canvas stream (no OS picker).
async function noCameraButCanShare(page: Page): Promise<void> {
  await page.addInitScript(() => {
    const md = navigator.mediaDevices;
    const origGUM = md.getUserMedia.bind(md);
    md.getUserMedia = (constraints?: MediaStreamConstraints) => {
      if (constraints && constraints.video) {
        return Promise.reject(new DOMException('Requested device not found', 'NotFoundError'));
      }
      return origGUM(constraints);
    };
    md.getDisplayMedia = async () => {
      const canvas = Object.assign(document.createElement('canvas'), { width: 320, height: 240 });
      const ctx = canvas.getContext('2d')!;
      let i = 0;
      setInterval(() => {
        ctx.fillStyle = `hsl(${(i += 11) % 360} 80% 50%)`;
        ctx.fillRect(0, 0, 320, 240);
      }, 80);
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      return (canvas as any).captureStream(12) as MediaStream;
    };
  });
}

/** Drive a camera-less page from home into the call (audio-only). */
async function joinAudioOnly(page: Page, room: string): Promise<void> {
  await page.goto('/', { waitUntil: 'networkidle' });
  await page.selectOption('#lang', 'en');
  await page.fill('#name', 'Sharer');
  await page.fill('#room', room);
  await page.click('#enter');
  await page.waitForSelector('#prejoin:not(.hidden)');
  // No camera → the preview shows the camera-off placeholder rather than video.
  await page.waitForFunction(() => {
    const off = document.getElementById('preview-off');
    return !!off && !off.hidden;
  });
  await page.click('#join-btn');
  await page.waitForSelector('#call:not(.hidden)');
  const cookieAccept = page.locator('#cookie-accept');
  if (await cookieAccept.isVisible().catch(() => false)) await cookieAccept.click();
}

test('screen share works without a camera (issue #4)', async ({ browser }) => {
  const room = 'share' + Math.floor(Math.random() * 1e6);
  const a = await openPage(browser); // the sharer — no camera
  const b = await openPage(browser); // a normal viewer with a camera
  await noCameraButCanShare(a.page);

  await joinAudioOnly(a.page, room);
  await joinCall(b.page, { name: 'Viewer', lang: 'en', room });
  await sleep(5000); // WebRTC connect

  // Both see two cells.
  expect(await a.page.$$eval('.video-cell', (e) => e.length)).toBe(2);
  expect(await b.page.$$eval('.video-cell', (e) => e.length)).toBe(2);

  // Before sharing, the viewer shows the sharer's camera-off avatar (no video).
  expect(
    await b.page.evaluate(() => {
      const av = document.querySelector('.video-cell:not(.self) .avatar') as HTMLElement | null;
      return !!av && !av.hidden;
    }),
  ).toBeTruthy();

  // Start the screen share (the share button is the active-success one).
  await a.page.click('#btn-share');

  // The viewer now receives flowing video from the camera-less peer, and the
  // camera-off avatar is hidden — proving the screen reaches peers.
  await b.page.waitForFunction(
    () => {
      const cell = document.querySelector('.video-cell:not(.self)');
      if (!cell) return false;
      const v = cell.querySelector('video') as HTMLVideoElement | null;
      const av = cell.querySelector('.avatar') as HTMLElement | null;
      return !!(v && v.srcObject && v.videoWidth > 0) && !!av && av.hidden;
    },
    { timeout: 20000 },
  );

  // The sharer's own tile shows the screen (avatar hidden) with the 🖥 badge.
  expect(
    await a.page.evaluate(() => {
      const cell = document.querySelector('.video-cell.self');
      const av = cell?.querySelector('.avatar') as HTMLElement | null;
      const badge = cell?.querySelector('.screen-share-badge');
      return !!av && av.hidden && !!badge;
    }),
  ).toBeTruthy();
  expect(
    await a.page.evaluate(() =>
      document.getElementById('btn-share')!.classList.contains('active-success'),
    ),
  ).toBeTruthy();

  // Stop sharing → with no camera, the viewer falls back to the camera-off avatar.
  await a.page.click('#btn-share');
  await b.page.waitForFunction(
    () => {
      const av = document.querySelector('.video-cell:not(.self) .avatar') as HTMLElement | null;
      return !!av && !av.hidden;
    },
    { timeout: 10000 },
  );
  expect(
    await a.page.evaluate(() => {
      const cell = document.querySelector('.video-cell.self');
      const badge = cell?.querySelector('.screen-share-badge');
      return !badge; // badge removed
    }),
  ).toBeTruthy();

  await closePage(a);
  await closePage(b);
});

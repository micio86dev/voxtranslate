import { test, expect } from '@playwright/test';
import { openPage, closePage, NodePeer, sleep } from './helpers';

test('room full: a 5th participant is rejected and returned home', async ({ browser }) => {
  const room = 'full' + Math.floor(Math.random() * 1e6);
  const fillers = [
    new NodePeer(room, 'it', 'P1'),
    new NodePeer(room, 'en', 'P2'),
    new NodePeer(room, 'es', 'P3'),
    new NodePeer(room, 'fr', 'P4'),
  ];
  await Promise.all(fillers.map((f) => f.ready));
  await sleep(300);

  const t = await openPage(browser);
  const { page } = t;
  await page.goto('/', { waitUntil: 'networkidle' });
  await page.selectOption('#lang', 'en');
  await page.fill('#name', 'Latecomer');
  await page.fill('#room', room);
  await page.click('#enter');
  await page.waitForSelector('#prejoin:not(.hidden)');
  await page.waitForFunction(() => {
    const v = document.getElementById('preview') as HTMLVideoElement | null;
    return !!(v && v.srcObject && v.videoWidth > 0);
  },
    undefined,
    { timeout: 20000 },
  );
  await page.click('#join-btn');

  // The room_full message bounces us back home with an error.
  await page.waitForSelector('#home:not(.hidden)', { timeout: 8000 });
  await sleep(500);
  expect((await page.textContent('#home-status')) || '').toMatch(/full/i);

  fillers.forEach((f) => f.close());
  await closePage(t);
});

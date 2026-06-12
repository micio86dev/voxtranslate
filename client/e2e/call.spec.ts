import { test, expect } from '@playwright/test';
import { resolve } from 'node:path';
import { openPage, closePage, joinCall, NodePeer, sleep } from './helpers';

// Playwright runs with cwd = client/.
const SAMPLE = resolve(process.cwd(), '../server/tests/fixtures/sample.webm');

test('call: WebRTC video, translated chat, subtitles, controls, leave', async ({ browser }) => {
  const room = 'call' + Math.floor(Math.random() * 1e6);
  const a = await openPage(browser);
  const b = await openPage(browser);
  await joinCall(a.page, { name: 'Alice', lang: 'en', room });
  await joinCall(b.page, { name: 'Bob', lang: 'it', room });
  await sleep(5000); // WebRTC connect

  // Both see 2 cells; remote video is flowing.
  expect(await a.page.$$eval('.video-cell', (e) => e.length)).toBe(2);
  expect(await b.page.$$eval('.video-cell', (e) => e.length)).toBe(2);
  // Guest backend (no DB) sends no session_id → transcript indicator stays hidden.
  expect(
    await a.page.evaluate(() =>
      document.getElementById('transcript-indicator')!.classList.contains('hidden'),
    ),
  ).toBeTruthy();
  expect(
    await a.page.evaluate(() => {
      const v = document.querySelector('.video-cell:not(.self) video') as HTMLVideoElement;
      return !!(v && v.srcObject && v.videoWidth > 0);
    }),
  ).toBeTruthy();

  // Translated chat (en → it).
  await a.page.click('#btn-chat');
  await a.page.fill('#chat-input', 'hello everyone, nice to meet you');
  await a.page.click('#chat-send');
  await sleep(2000);
  await b.page.click('#btn-chat');
  const bobChat = await b.page.$$eval('.chat-msg-other .chat-text', (e) => e.map((x) => x.textContent));
  expect(bobChat.some((c) => c && c.trim())).toBeTruthy();

  // Emoji: a quick reaction floats over the sender's cell on every page…
  await a.page.click('#emoji-toggle');
  await a.page.click('#emoji-react button'); // first quick reaction (👍)
  await Promise.all([
    a.page.waitForSelector('.video-cell.self .emoji-float', { timeout: 5000 }),
    b.page.waitForSelector('.video-cell:not(.self) .emoji-float', { timeout: 5000 }),
  ]);
  // …and the panel stays open (issue #15), so a second reaction can be fired
  // straight away without reopening it.
  await expect(a.page.locator('#emoji-panel')).toBeVisible();
  await a.page.click('#emoji-react button:nth-child(2)'); // second reaction (❤️)
  await b.page.waitForSelector('.video-cell:not(.self) .emoji-float', { timeout: 5000 });
  // A grid emoji inserts into the chat input (the panel is still open).
  await a.page.click('#emoji-grid button');
  expect(await a.page.inputValue('#chat-input')).toContain('👍');
  await a.page.fill('#chat-input', '');
  await a.page.click('#emoji-toggle'); // close the panel

  // Subtitles: a node speaker (it) streams audio; Alice (en) sees a translation.
  const carla = new NodePeer(room, 'it', 'Carla');
  await carla.ready;
  await sleep(500);
  await carla.speak(SAMPLE);
  await a.page.waitForSelector(`.video-cell[data-peer="${carla.id}"] .subtitle-translation`, {
    timeout: 25000,
  });
  const subs = await a.page.$$eval(`.video-cell[data-peer="${carla.id}"] .subtitle-translation`, (e) =>
    e.map((x) => x.textContent),
  );
  expect(subs.some((s) => s && s.trim())).toBeTruthy();
  carla.close();

  // Controls: mute mic, camera off, toggle TTS twice.
  await a.page.click('#btn-mic');
  await a.page.click('#btn-cam');
  await a.page.click('#btn-tts');
  await a.page.click('#btn-tts');
  await sleep(700);
  expect(await a.page.evaluate(() => document.getElementById('btn-mic')!.classList.contains('active-danger'))).toBeTruthy();
  expect(
    await a.page.evaluate(() => {
      const av = document.querySelector('.video-cell.self .avatar') as HTMLElement;
      return av && !av.hidden;
    }),
  ).toBeTruthy();
  // Bob sees Alice muted + camera off.
  expect(
    await b.page.evaluate(() => {
      const m = document.querySelector('.video-cell:not(.self) .mute-indicator') as HTMLElement;
      const av = document.querySelector('.video-cell:not(.self) .avatar') as HTMLElement;
      return m && !m.hidden && av && !av.hidden;
    }),
  ).toBeTruthy();

  // Leave → back home; Bob sees the cell removed.
  await a.page.click('#btn-leave');
  await a.page.waitForSelector('#home:not(.hidden)');
  await sleep(900);
  expect(await b.page.$$eval('.video-cell', (e) => e.length)).toBe(1);
  // ...and guests never get the post-call transcript modal.
  expect(
    await a.page.evaluate(() =>
      document.getElementById('postcall-modal')!.classList.contains('hidden'),
    ),
  ).toBeTruthy();

  await closePage(a);
  await closePage(b);
});

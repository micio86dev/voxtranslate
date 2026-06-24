// Shared e2e helpers: pages with coverage, room joining, and a node-side
// WebSocket peer (to seed the lobby, fill rooms, or stream audio for subtitles).
import { readFileSync } from 'node:fs';
import type { Browser, BrowserContext, Page } from '@playwright/test';
import { startCoverage, collectCoverage } from './cov';

export const WS_HOST = 'localhost:3001';

export type Tracked = { page: Page; ctx: BrowserContext };

export async function openPage(
  browser: Browser,
  viewport = { width: 1000, height: 720 },
  mobile = false,
  // The first-run home tour auto-opens on a fresh context (flag unset) and its
  // modal overlays the home screen — intercepting clicks on #enter etc. Every
  // spec EXCEPT onboarding.spec is testing some other flow, so seed the "seen"
  // flags by default so the tour stays closed. onboarding.spec passes false to
  // exercise the genuine first-visit auto-open.
  seedToursSeen = true,
): Promise<Tracked> {
  const ctx = await browser.newContext({
    permissions: ['microphone', 'camera'],
    viewport,
    isMobile: mobile,
    hasTouch: mobile,
    // Block the PWA service worker: once active it intercepts fetches before
    // Playwright's route layer, so page.route() on /api/* would be bypassed.
    serviceWorkers: 'block',
  });
  const page = await ctx.newPage();
  if (seedToursSeen) {
    await page.addInitScript(() => {
      try {
        localStorage.setItem('vox_home_tour_seen', '1');
        localStorage.setItem('vox_call_tour_seen', '1');
      } catch {
        /* private mode — onboarding falls back to in-memory, tour still gated */
      }
    });
  }
  await startCoverage(page);
  return { page, ctx };
}

export async function closePage({ page, ctx }: Tracked): Promise<void> {
  await collectCoverage(page);
  await ctx.close();
}

/** Drive the UI from home through pre-join into the call. */
export async function joinCall(
  page: Page,
  opts: { name: string; lang: string; room: string; publicRoom?: boolean },
): Promise<void> {
  await page.goto('/', { waitUntil: 'networkidle' });
  await page.selectOption('#lang', opts.lang);
  await page.fill('#name', opts.name);
  await page.fill('#room', opts.room);
  if (opts.publicRoom === false) await page.click('.seg-btn[data-vis="private"]');
  await page.click('#enter');
  await page.waitForSelector('#prejoin:not(.hidden)');
  await page.waitForFunction(() => {
    const v = document.getElementById('preview') as HTMLVideoElement | null;
    return !!(v && v.srcObject && v.videoWidth > 0);
  });
  await page.click('#join-btn');
  await page.waitForSelector('#call:not(.hidden)');
  // The consent/cookie banner is fixed to the bottom of the viewport and overlays
  // the in-call control bar; accept it (as a real user would) so the controls
  // beneath it are clickable.
  const cookieAccept = page.locator('#cookie-accept');
  if (await cookieAccept.isVisible().catch(() => false)) await cookieAccept.click();
}

export const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export type Rect = { x: number; y: number; width: number; height: number } | null;

/** True when two bounding boxes share any area. A `null` box (element not rendered)
 *  never overlaps — callers assert visibility separately. */
export function rectsOverlap(a: Rect, b: Rect): boolean {
  if (!a || !b) return false;
  return a.x < b.x + b.width && a.x + a.width > b.x && a.y < b.y + b.height && a.y + a.height > b.y;
}

/** Collect REAL frontend console errors for the life of the page: uncaught exceptions
 *  (`pageerror`) and explicit `console.error` calls. A clean console is part of "the
 *  flow works" (standing E2E rule) — assert the returned array is empty at the end.
 *
 *  Excludes the browser's generic "Failed to load resource: … <status>" mirror of a
 *  failed network request: it carries NO url, and in the guest-only e2e backend some
 *  endpoints are intentionally unavailable (e.g. `GET /api/auth/config` → 503 with no
 *  auth configured; 200 in prod). That's backend availability, not a JS bug — failing
 *  on it would be flaky noise. Genuine JS breakage still surfaces via the other two. */
export function trackConsoleErrors(page: Page): string[] {
  const errors: string[] = [];
  page.on('console', (m) => {
    if (m.type() === 'error' && !m.text().startsWith('Failed to load resource')) errors.push(m.text());
  });
  page.on('pageerror', (e) => errors.push(`pageerror: ${e.message}`));
  return errors;
}

/** A node-side peer used to seed the lobby / fill rooms / stream audio. */
export class NodePeer {
  id: string;
  private ws: WebSocket;
  ready: Promise<void>;
  constructor(room: string, lang: string, name: string, publicRoom = true) {
    this.id = `${name}-${Math.random().toString(36).slice(2, 7)}`;
    const p = new URLSearchParams({ room, lang, id: this.id, name, public: String(publicRoom) });
    this.ws = new WebSocket(`ws://${WS_HOST}/ws?${p}`);
    this.ws.binaryType = 'arraybuffer';
    this.ready = new Promise((res) => (this.ws.onopen = () => res()));
  }
  async speak(file: string): Promise<void> {
    const audio = readFileSync(file);
    this.ws.send(JSON.stringify({ type: 'start' }));
    await sleep(150);
    for (let o = 0; o < audio.length; o += 1024) {
      this.ws.send(audio.subarray(o, o + 1024));
      await sleep(120);
    }
    await sleep(2000);
    this.ws.send(JSON.stringify({ type: 'stop' }));
  }
  close(): void {
    this.ws.close();
  }
}

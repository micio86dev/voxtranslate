// GA4 collect-request regression guard.
//
// Symptom this pins against: GA tracked nothing — no console error, and NO
// network request to `/collect` ever fired, even after we started emitting a
// manual page_view on consent grant.
//
// Root cause: the gtag stub pushed a plain ARRAY (`(...args) => push(args)`)
// instead of the `arguments` object. gtag.js dispatches a queued command ONLY
// when the pushed item is the `arguments` object (exactly how Google's snippet
// works); a plain array is silently dropped, so every consent/config/event —
// including page_view — no-ops and no hit is ever sent.
//
// This is a source-text guard (same approach as csp.test.ts): the behavioural
// path needs gtag.js + a real DOM, but the one thing that silently breaks every
// hit is the stub's push form, so we pin that.

import { readFileSync } from 'node:fs';
import { afterEach, describe, expect, it, vi } from 'vitest';

const src = readFileSync(new URL('./analytics.ts', import.meta.url), 'utf8');

describe('GA gtag stub (collect-request regression)', () => {
  it('pushes the `arguments` object, exactly like Google’s snippet', () => {
    expect(src).toMatch(/dataLayer!?\.push\(arguments\)/);
  });

  it('never reverts to pushing a rest-param array (the original bug)', () => {
    // `(...args) => dataLayer.push(args)` pushes an Array → gtag.js ignores it.
    expect(src).not.toMatch(/dataLayer!?\.push\(args\)/);
    expect(src).not.toMatch(/gtag\s*=\s*function\s*\(\s*\.\.\.\s*args/);
  });

  it('still gates on consent: config suppresses the auto page_view', () => {
    // We send page_view explicitly on grant, so config must NOT auto-send one
    // (it would be withheld under denied consent and never replayed).
    expect(src).toMatch(/send_page_view:\s*false/);
    expect(src).toMatch(/gtag\?\.\('event',\s*'page_view'\)/);
  });
});

// ---- behavioural coverage (node env, stubbed window/document/location) ------
//
// GA_ID / IS_STAGING are captured at module load, and `started`/`pageViewSent`
// are module state, so every scenario re-imports a fresh copy after stubbing
// import.meta.env + the browser globals. In the node env there is no real
// window/location/document, which makes every `typeof` guard controllable.

type GtagWindow = { dataLayer?: unknown[]; gtag?: (...args: unknown[]) => void };
type Analytics = typeof import('./analytics');

interface LoadOpts {
  gaId?: string;
  staging?: string;
  /** hostname for the stubbed `location`; null ⇒ leave `location` undefined. */
  hostname?: string | null;
  withDocument?: boolean;
}

async function loadAnalytics(opts: LoadOpts = {}): Promise<{
  mod: Analytics;
  win: GtagWindow;
  scripts: { src: string; async: boolean }[];
}> {
  vi.resetModules();
  vi.stubEnv('PUBLIC_GA_ID', opts.gaId ?? 'G-TEST123');
  vi.stubEnv('PUBLIC_STAGING', opts.staging ?? '');
  const win: GtagWindow = {};
  vi.stubGlobal('window', win);
  if (opts.hostname !== null) {
    vi.stubGlobal('location', { hostname: opts.hostname ?? 'voxtranslate.app' });
  }
  const scripts: { src: string; async: boolean }[] = [];
  if (opts.withDocument !== false) {
    vi.stubGlobal('document', {
      createElement: (_tag: string) => ({ src: '', async: false }),
      head: {
        appendChild: (el: { src: string; async: boolean }) => {
          scripts.push(el);
        },
      },
    });
  }
  const mod = await import('./analytics');
  return { mod, win, scripts };
}

/** dataLayer entries as plain arrays (gtag pushes `arguments` objects). */
function entries(win: GtagWindow): unknown[][] {
  return (win.dataLayer ?? []).map((a) => Array.from(a as ArrayLike<unknown>));
}

describe('analytics (behaviour)', () => {
  afterEach(() => {
    vi.unstubAllEnvs();
    vi.unstubAllGlobals();
  });

  it('stays dormant without a GA id', async () => {
    const { mod, win, scripts } = await loadAnalytics({ gaId: '' });
    mod.initAnalytics();
    mod.track('anything');
    expect(win.gtag).toBeUndefined();
    expect(win.dataLayer).toBeUndefined();
    expect(scripts).toEqual([]);
  });

  it('stays dormant on staging', async () => {
    const { mod, win } = await loadAnalytics({ staging: 'true' });
    mod.initAnalytics();
    expect(win.gtag).toBeUndefined();
  });

  it('stays dormant on local hosts', async () => {
    for (const hostname of ['localhost', '127.0.0.1', '[::1]', 'dev.local']) {
      const { mod, win } = await loadAnalytics({ hostname });
      mod.initAnalytics();
      expect(win.gtag, hostname).toBeUndefined();
    }
  });

  it('bails before touching the DOM when there is no document (SSR)', async () => {
    const { mod, win } = await loadAnalytics({ withDocument: false });
    mod.initAnalytics();
    expect(win.gtag).toBeUndefined();
    // grant with no gtag stub yet → optional chaining no-ops, no crash
    expect(() => mod.grantAnalyticsConsent()).not.toThrow();
  });

  it('boots with consent DENIED, a suppressed page_view, and the async tag', async () => {
    const { mod, win, scripts } = await loadAnalytics();
    mod.initAnalytics();

    expect(entries(win)[0]).toEqual([
      'consent',
      'default',
      {
        ad_storage: 'denied',
        ad_user_data: 'denied',
        ad_personalization: 'denied',
        analytics_storage: 'denied',
        wait_for_update: 500,
      },
    ]);
    const config = entries(win).find((e) => e[0] === 'config');
    expect(config?.[1]).toBe('G-TEST123');
    expect(config?.[2]).toMatchObject({ send_page_view: false });
    expect(scripts).toHaveLength(1);
    expect(scripts[0].async).toBe(true);
    expect(scripts[0].src).toBe('https://www.googletagmanager.com/gtag/js?id=G-TEST123');

    mod.initAnalytics(); // idempotent — no second tag, no duplicate consent
    expect(scripts).toHaveLength(1);
  });

  it('grant flips consent and emits exactly one page_view', async () => {
    const { mod, win } = await loadAnalytics();
    mod.initAnalytics();
    mod.grantAnalyticsConsent();
    mod.grantAnalyticsConsent(); // second grant must NOT double-count the visit

    const all = entries(win);
    expect(all).toContainEqual(['consent', 'update', { analytics_storage: 'granted' }]);
    expect(all.filter((e) => e[0] === 'event' && e[1] === 'page_view')).toHaveLength(1);
  });

  it('deny flips consent back and re-arms the page_view for a re-grant', async () => {
    const { mod, win } = await loadAnalytics();
    mod.initAnalytics();
    mod.grantAnalyticsConsent();
    mod.denyAnalyticsConsent();
    mod.grantAnalyticsConsent();

    const all = entries(win);
    expect(all).toContainEqual(['consent', 'update', { analytics_storage: 'denied' }]);
    expect(all.filter((e) => e[0] === 'event' && e[1] === 'page_view')).toHaveLength(2);
  });

  it('track forwards events with params (and no-ops pre-init without crashing)', async () => {
    const first = await loadAnalytics();
    expect(() => first.mod.track('too_early')).not.toThrow(); // enabled but gtag not stubbed yet

    const { mod, win } = await loadAnalytics();
    mod.initAnalytics();
    mod.track('call_start', { tier: 'pro' });
    expect(entries(win)).toContainEqual(['event', 'call_start', { tier: 'pro' }]);
  });
});

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
  /** PUBLIC_FB_PIXEL_ID for this scenario; '' disables the pixel. */
  pixelId?: string;
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
  vi.stubEnv('PUBLIC_FB_PIXEL_ID', opts.pixelId ?? '');
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

  // Acquisition attribution: GA4 only understands `utm_*`, so a visitor arriving
  // with our own `?source` / `?ref` looks organic to GA. The signup event has to
  // carry the first-touch source itself, otherwise "registrations by campaign"
  // is unanswerable in GA4.
  it('reports a new account as a sign_up carrying the acquisition source', async () => {
    const { mod, win } = await loadAnalytics();
    mod.initAnalytics();
    mod.trackAuthSuccess({ isNew: true, source: 'reddit' });

    expect(entries(win)).toContainEqual([
      'event',
      'sign_up',
      { method: 'google', acquisition_source: 'reddit' },
    ]);
    // A signup must NOT also count as a login, or both metrics are inflated.
    expect(entries(win).filter((e) => e[1] === 'login')).toHaveLength(0);
  });

  it('labels a missing source as organic instead of dropping the param', async () => {
    const { mod, win } = await loadAnalytics();
    mod.initAnalytics();
    mod.trackAuthSuccess({ isNew: true, source: null });

    expect(entries(win)).toContainEqual([
      'event',
      'sign_up',
      { method: 'google', acquisition_source: 'organic' },
    ]);
  });

  it('reports a returning user as login, with no source noise', async () => {
    const { mod, win } = await loadAnalytics();
    mod.initAnalytics();
    mod.trackAuthSuccess({ isNew: false, source: 'reddit' });

    expect(entries(win)).toContainEqual(['event', 'login', { method: 'google' }]);
    expect(entries(win).filter((e) => e[1] === 'sign_up')).toHaveLength(0);
  });

  it('honours a non-google auth method', async () => {
    const { mod, win } = await loadAnalytics();
    mod.initAnalytics();
    mod.trackAuthSuccess({ isNew: false, source: null, method: 'email' });

    expect(entries(win)).toContainEqual(['event', 'login', { method: 'email' }]);
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

// ---- Meta pixel (consent-gated advertising) ---------------------------------
//
// The pixel is an advertising tracker: it may load ONLY after the visitor accepts, and
// the registration conversion must reach Meta as the standard `CompleteRegistration`
// (standard events are what campaign optimisation can bid on). These tests pin the gate
// itself, because a pixel that loads before consent is the failure that matters.
describe('Meta pixel', () => {
  it('does not touch fbq before consent is granted', async () => {
    const { mod, win } = await loadAnalytics({ pixelId: '111' });
    mod.initAnalytics();
    expect((win as { fbq?: unknown }).fbq).toBeUndefined();
    expect(mod.trackFbEvent('CompleteRegistration')).toBe(false);
  });

  it('loads the pixel and sends PageView once consent is granted', async () => {
    const { mod, win, scripts } = await loadAnalytics({ pixelId: '111' });
    mod.initAnalytics();
    mod.loadMetaPixel();

    const fbq = (win as { fbq?: (...a: unknown[]) => void }).fbq;
    expect(typeof fbq).toBe('function');
    expect(scripts.some((s) => s.src.includes('connect.facebook.net'))).toBe(true);
    expect(mod.fbqCalls(win)).toEqual([
      ['init', '111'],
      ['track', 'PageView'],
    ]);
  });

  it('is idempotent — a second grant does not double-load or double-count', async () => {
    const { mod, win, scripts } = await loadAnalytics({ pixelId: '111' });
    mod.initAnalytics();
    mod.loadMetaPixel();
    mod.loadMetaPixel();
    expect(scripts.filter((s) => s.src.includes('connect.facebook.net'))).toHaveLength(1);
    expect(mod.fbqCalls(win).filter((c) => c[1] === 'PageView')).toHaveLength(1);
  });

  it('stays dormant with no pixel id configured', async () => {
    const { mod, win, scripts } = await loadAnalytics({ pixelId: '' });
    mod.initAnalytics();
    mod.loadMetaPixel();
    expect((win as { fbq?: unknown }).fbq).toBeUndefined();
    expect(scripts.some((s) => s.src.includes('connect.facebook.net'))).toBe(false);
  });

  it('reports a registration as CompleteRegistration', async () => {
    const { mod, win } = await loadAnalytics({ pixelId: '111' });
    mod.initAnalytics();
    mod.loadMetaPixel();
    mod.trackAuthSuccess({ isNew: true, source: 'reddit' });
    expect(mod.fbqCalls(win)).toContainEqual(['track', 'CompleteRegistration']);
  });

  it('does not report a returning login as a registration', async () => {
    const { mod, win } = await loadAnalytics({ pixelId: '111' });
    mod.initAnalytics();
    mod.loadMetaPixel();
    mod.trackAuthSuccess({ isNew: false, source: null });
    expect(mod.fbqCalls(win).some((c) => c[1] === 'CompleteRegistration')).toBe(false);
  });

  it('reports a payment as a valued Purchase on both platforms', async () => {
    const { mod, win } = await loadAnalytics({ pixelId: '111' });
    mod.initAnalytics();
    mod.loadMetaPixel();
    mod.trackPurchase({ value: 15, currency: 'usd' });

    // Meta bids on `Purchase`; value + currency are what makes it ROAS-optimisable.
    expect(mod.fbqCalls(win)).toContainEqual(['track', 'Purchase', { value: 15, currency: 'USD' }]);
    // GA4 keeps the established event name and must agree on the amount.
    expect(entries(win)).toContainEqual([
      'event',
      'payment_completed',
      { value: 15, currency: 'USD' },
    ]);
  });

  it('never invents an amount when the price is unknown', async () => {
    const { mod, win } = await loadAnalytics({ pixelId: '111' });
    mod.initAnalytics();
    mod.loadMetaPixel();
    mod.trackPurchase();
    mod.trackPurchase({ value: 0 });

    // Still counted as a conversion, but unvalued — a guessed value would poison bidding.
    expect(mod.fbqCalls(win).filter((c) => c[1] === 'Purchase')).toEqual([
      ['track', 'Purchase'],
      ['track', 'Purchase'],
    ]);
    expect(entries(win).filter((e) => e[1] === 'payment_completed')).toEqual([
      ['event', 'payment_completed', {}],
      ['event', 'payment_completed', {}],
    ]);
  });

  it('does not touch the pixel for a purchase made without consent', async () => {
    const { mod, win } = await loadAnalytics({ pixelId: '111' });
    mod.initAnalytics(); // no loadMetaPixel() — the visitor never accepted
    mod.trackPurchase({ value: 15 });
    expect((win as { fbq?: unknown }).fbq).toBeUndefined();
  });
});

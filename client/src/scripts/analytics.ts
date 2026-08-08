// Google Analytics 4 (gtag.js) with Consent Mode v2.
//
// The tag loads immediately (so Google can detect it and use consent modeling),
// but `analytics_storage` is DENIED by default — no cookies are written and no
// hits are sent until the visitor accepts the cookie banner, at which point we
// flip consent to `granted`. This passes Google's tag-detection check while
// staying GDPR-compliant (nothing tracked pre-consent).
//
// Runs in real production only: skipped on staging (PUBLIC_STAGING) and on
// localhost, and a no-op until PUBLIC_GA_ID is configured at build time.

const GA_ID = (import.meta.env.PUBLIC_GA_ID as string | undefined) || '';
/** Meta pixel id. Advertising, so it loads ONLY on an explicit accept — never at boot,
 *  unlike gtag, which relies on Consent Mode to withhold hits. Empty ⇒ no pixel. */
const FB_PIXEL_ID = (import.meta.env.PUBLIC_FB_PIXEL_ID as string | undefined) || '';
const IS_STAGING =
  import.meta.env.PUBLIC_STAGING === 'true' ||
  (import.meta.env.PUBLIC_STAGING as unknown) === true;

declare global {
  interface Window {
    dataLayer?: unknown[];
    gtag?: (...args: unknown[]) => void;
    fbq?: FbqFn;
    _fbq?: FbqFn;
  }
}

/** The pixel stub Meta's snippet installs: callable, with a queue until fbevents loads. */
interface FbqFn {
  (...args: unknown[]): void;
  callMethod?: (...args: unknown[]) => void;
  queue: unknown[][];
  push?: unknown;
  loaded?: boolean;
  version?: string;
}

/** GA runs only in real production: needs an ID, not staging, not localhost. */
function analyticsEnabled(): boolean {
  if (!GA_ID || IS_STAGING) return false;
  if (typeof location !== 'undefined') {
    const h = location.hostname;
    if (h === 'localhost' || h === '127.0.0.1' || h === '[::1]' || h.endsWith('.local')) {
      return false;
    }
  }
  return true;
}

let started = false;
let pageViewSent = false;

/**
 * Load gtag.js with Consent Mode v2: consent defaults to DENIED, so nothing is
 * stored or sent until `grantAnalyticsConsent()`. Idempotent; call once on boot.
 */
export function initAnalytics(): void {
  if (started || !analyticsEnabled() || typeof document === 'undefined') return;
  started = true;
  window.dataLayer = window.dataLayer || [];
  // gtag.js dispatches a queued command ONLY when the pushed item is the
  // `arguments` object (this is how Google's own snippet works). Pushing a plain
  // array — which `(...args) => push(args)` does — is silently ignored: every
  // consent/config/event no-ops and NO hit is ever sent (no error, no /collect
  // request). So push `arguments` verbatim, exactly like the canonical snippet.
  window.gtag = function gtag() {
    // eslint-disable-next-line prefer-rest-params
    window.dataLayer!.push(arguments);
  };
  // Consent Mode v2 — deny by default until the user accepts the cookie banner.
  window.gtag('consent', 'default', {
    ad_storage: 'denied',
    ad_user_data: 'denied',
    ad_personalization: 'denied',
    analytics_storage: 'denied',
    wait_for_update: 500,
  });
  const s = document.createElement('script');
  s.async = true;
  s.src = `https://www.googletagmanager.com/gtag/js?id=${GA_ID}`;
  document.head.appendChild(s);
  window.gtag('js', new Date());
  window.gtag('config', GA_ID, {
    // Don't let `config` auto-send page_view: it fires while analytics_storage is still
    // 'denied', gets withheld by Consent Mode, and is NOT replayed when consent later
    // flips to granted — so GA would never receive a hit. We send page_view explicitly
    // from grantAnalyticsConsent() instead, the moment storage is actually allowed.
    send_page_view: false,
    linker: {
      domains: ['voxtranslate.app', 'app.voxtranslate.app', 'dashboard.voxtranslate.app']
    }
  });
}

/** Flip analytics consent to granted — call when the user accepts cookies. */
export function grantAnalyticsConsent(): void {
  if (!analyticsEnabled()) return;
  window.gtag?.('consent', 'update', { analytics_storage: 'granted' });
  // The initial page_view was suppressed while consent was denied and Consent Mode does
  // not replay it on grant, so emit one now (once) to actually register the visit in GA.
  if (!pageViewSent) {
    pageViewSent = true;
    window.gtag?.('event', 'page_view');
  }
}

/** Flip analytics consent back to denied — call on decline / withdrawal. */
export function denyAnalyticsConsent(): void {
  window.gtag?.('consent', 'update', { analytics_storage: 'denied' });
  // Allow a fresh page_view to be sent if the visitor re-grants consent this session.
  pageViewSent = false;
}

/** Fire a GA4 event. No-op when analytics is disabled or pre-init. */
export function track(event: string, params: Record<string, unknown> = {}): void {
  if (!analyticsEnabled()) return;
  window.gtag?.('event', event, params);
}

/**
 * Report a successful authentication, split by what actually happened: a new
 * account fires GA4's recommended `sign_up` conversion, a returning user fires
 * `login`. Never both — one auth is one event, or signups and logins both
 * double-count.
 *
 * `sign_up` carries the first-touch acquisition source (`?source` / `?utm_source`
 * / `?ref`, see `auth.captureAcquisitionSource`). GA4's own attribution only
 * understands `utm_*`, so a visitor arriving with our bare `?source=reddit` looks
 * organic to GA — sending it as an event param is the only way "registrations by
 * campaign" is answerable in GA4. Register `acquisition_source` as a custom
 * dimension in GA4, otherwise it is collected but not reportable.
 *
 * An absent source is sent as `organic` rather than omitted, so the dimension is
 * never empty and organic signups stay countable.
 */
export function trackAuthSuccess(opts: {
  isNew: boolean;
  source?: string | null;
  /** Auth provider, GA4's recommended `method` param. */
  method?: string;
}): void {
  const method = opts.method ?? 'google';
  if (opts.isNew) {
    track('sign_up', { method, acquisition_source: opts.source || 'organic' });
    // Meta's standard registration conversion — the event campaign optimisation bids on.
    // No-op unless the pixel was loaded, i.e. unless advertising consent was given.
    trackFbEvent('CompleteRegistration');
  } else {
    track('login', { method });
  }
}

/**
 * Report a completed purchase to BOTH platforms in one call, so GA4 and Meta can never
 * drift on what a payment was worth.
 *
 * `value` + `currency` are the whole point: without them Meta can only bid on purchase
 * COUNT and no ROAS is computable. They stay OPTIONAL because the Stripe return lands on
 * a fresh page load that carries no amount — when the caller cannot produce the real
 * price we still report the conversion, just unvalued. Never substitute a guessed
 * amount: a wrong value poisons bidding worse than a missing one does.
 *
 * The GA4 event keeps its existing `payment_completed` name (renaming it to GA's
 * recommended `purchase` would orphan the conversion already configured in GA4); Meta
 * gets its standard `Purchase`, which is the event campaign optimisation bids on. The
 * Meta call is a no-op unless the pixel was loaded, i.e. unless advertising consent was
 * given. Currency defaults to USD — the only currency credit packages are priced in.
 */
export function trackPurchase(opts: { value?: number | null; currency?: string } = {}): void {
  const value =
    typeof opts.value === 'number' && Number.isFinite(opts.value) && opts.value > 0
      ? opts.value
      : null;
  const currency = (opts.currency || 'USD').toUpperCase();
  const params = value === null ? undefined : { value, currency };
  // Analytics must never break the post-payment flow (balance refresh, URL tidy), so a
  // throwing tag is swallowed here rather than surfacing to the caller.
  try {
    track('payment_completed', params ?? {});
    trackFbEvent('Purchase', params);
  } catch {
    /* tag unavailable / blocked — the payment itself is unaffected */
  }
}

// ──────────────────────────────────────────────────────────────────────────────
// Meta pixel. Advertising, so the rules differ from gtag: nothing is loaded until
// `loadMetaPixel()` is called from the consent handler, and there is no <noscript>
// fallback anywhere — that would fire the pixel regardless of consent.
// ──────────────────────────────────────────────────────────────────────────────

/** Whether the pixel may run at all: needs an id, and the same not-staging/not-local
 *  rule as GA so development traffic never reaches an ad account. */
function pixelEnabled(): boolean {
  return !!FB_PIXEL_ID && analyticsEnabled();
}

/**
 * Install the pixel and send its PageView. Call ONLY from an explicit accept.
 * Idempotent: a second call neither re-injects the script nor re-counts the view.
 */
export function loadMetaPixel(): void {
  if (!pixelEnabled() || typeof document === 'undefined' || window.fbq) return;
  const fbq: FbqFn = function (...args: unknown[]) {
    if (fbq.callMethod) fbq.callMethod(...args);
    else fbq.queue.push(args);
  } as FbqFn;
  fbq.queue = [];
  fbq.loaded = true;
  fbq.version = '2.0';
  fbq.push = fbq;
  window.fbq = fbq;
  if (!window._fbq) window._fbq = fbq;
  const s = document.createElement('script');
  s.async = true;
  s.src = 'https://connect.facebook.net/en_US/fbevents.js';
  document.head.appendChild(s);
  window.fbq('init', FB_PIXEL_ID);
  window.fbq('track', 'PageView');
}

/** Fire a Meta event. Returns whether it was sent — false means no consent (or no id),
 *  which callers can treat as "not measurable" rather than an error. */
export function trackFbEvent(event: string, params?: Record<string, unknown>): boolean {
  if (!window.fbq) return false;
  if (params) window.fbq('track', event, params);
  else window.fbq('track', event);
  return true;
}

/** The queued fbq calls, for tests: the stub records everything until fbevents loads. */
export function fbqCalls(win: { fbq?: FbqFn } = window): unknown[][] {
  return (win.fbq?.queue ?? []).map((a) => Array.from(a as ArrayLike<unknown>));
}

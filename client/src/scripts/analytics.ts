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
const IS_STAGING =
  import.meta.env.PUBLIC_STAGING === 'true' ||
  (import.meta.env.PUBLIC_STAGING as unknown) === true;

declare global {
  interface Window {
    dataLayer?: unknown[];
    gtag?: (...args: unknown[]) => void;
  }
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
      domains: ['voxtranslate.app', 'website.voxtranslate.app', 'dashboard.voxtranslate.app']
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

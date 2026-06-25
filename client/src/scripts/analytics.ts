// Google Analytics 4 (gtag.js) for the app. Loads only when PUBLIC_GA_ID is set at
// build time — otherwise every function is a no-op, so the app ships zero tracking
// until an ID is configured. Product usage is covered by the privacy policy the user
// accepts at sign-up; this is first-party analytics (no ad/marketing pixels here).

const GA_ID = (import.meta.env.PUBLIC_GA_ID as string | undefined) || '';

declare global {
  interface Window {
    dataLayer?: unknown[];
    gtag?: (...args: unknown[]) => void;
  }
}

let started = false;

/** Inject gtag.js and start a session. Idempotent; safe to call on every app entry. */
export function initAnalytics(): void {
  if (started || !GA_ID || typeof document === 'undefined') return;
  started = true;
  const s = document.createElement('script');
  s.async = true;
  s.src = `https://www.googletagmanager.com/gtag/js?id=${GA_ID}`;
  document.head.appendChild(s);
  window.dataLayer = window.dataLayer || [];
  window.gtag = function (...args: unknown[]) {
    window.dataLayer!.push(args);
  };
  window.gtag('js', new Date());
  // SPA-style app: we send our own events; page_view still fires once on config.
  window.gtag('config', GA_ID);
}

/** Fire a GA4 event. No-op when analytics is disabled (no PUBLIC_GA_ID). */
export function track(event: string, params: Record<string, unknown> = {}): void {
  if (!GA_ID) return;
  window.gtag?.('event', event, params);
}

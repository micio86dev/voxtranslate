// Auth + billing client. Talks to the optional accounts/credits backend:
// Google login -> JWT session, balance, credit packages, Stripe checkout, and
// usage/credit history. When the backend runs in guest-only mode (billing off),
// `billingEnabled()` resolves false and the UI skips all of this.

import { getUiLang } from './i18n';

const WS_HOST = import.meta.env.PUBLIC_WS_HOST || location.host;
const WS_PROTO = location.protocol === 'https:' ? 'wss:' : 'ws:';
export const WS_BASE = `${WS_PROTO}//${WS_HOST}`;
export const HTTP_BASE = WS_BASE.replace(/^ws/, 'http');

export interface User {
  id: string;
  email: string;
  name: string;
  avatar_url?: string | null;
  balance: number;
  /** True once the user confirmed 18+ and accepted the ToS/Privacy. */
  consent_given?: boolean;
  /** True once this account has a stored Cartesia cloned voice (spec 0108): the
   *  pre-join voice-prep prompt is then skipped on every device, not just the one
   *  that recorded it. */
  has_voice_clone?: boolean;
}

export interface CreditPackage {
  id: string;
  name: string;
  price_usd: number;
  credits_usd: number;
}

export interface Transaction {
  amount: number;
  kind: string;
  balance_after: number;
  description?: string | null;
  created_at: string;
}

export interface UsageSession {
  room: string;
  speaking_seconds: number;
  cost: number;
  started_at: string;
  ended_at?: string | null;
}

/** One row of `GET /api/sessions`: a recorded call the user took part in. */
export interface CallSession {
  id: string;
  room: string;
  started_at: string;
  ended_at?: string | null;
  event_count: number;
}

const TOKEN_KEY = 'vox.token';
const USER_KEY = 'vox.user';
const SRC_KEY = 'vox.src';

// localStorage may be unavailable (private mode, tests) — fall back to memory.
const mem = new Map<string, string>();
function store(): Pick<Storage, 'getItem' | 'setItem' | 'removeItem'> {
  try {
    if (typeof localStorage !== 'undefined') return localStorage;
  } catch {
    /* blocked */
  }
  return {
    getItem: (k) => (mem.has(k) ? mem.get(k)! : null),
    setItem: (k, v) => void mem.set(k, v),
    removeItem: (k) => void mem.delete(k),
  };
}

let token: string | null = store().getItem(TOKEN_KEY);
let user: User | null = parseUser(store().getItem(USER_KEY));
let billing: boolean | null = null;
let googleClientId = '';
// Listener-pays mode (spec 0099): set from `/api/auth/config`. Drives the picker
// cost copy ("quality you RECEIVE" vs "per translation language").
let listenerPays = false;

function parseUser(raw: string | null): User | null {
  if (!raw) return null;
  try {
    return JSON.parse(raw) as User;
  } catch {
    return null;
  }
}

export function getToken(): string | null {
  return token;
}
export function getUser(): User | null {
  return user;
}
export function isLoggedIn(): boolean {
  return !!token && !!user;
}

export function saveSession(t: string, u: User): void {
  token = t;
  user = u;
  store().setItem(TOKEN_KEY, t);
  store().setItem(USER_KEY, JSON.stringify(u));
}

export function clearSession(): void {
  token = null;
  user = null;
  store().removeItem(TOKEN_KEY);
  store().removeItem(USER_KEY);
}

/** Patch the cached balance (after a usage tick or top-up) and persist it. */
export function setBalance(balance: number): void {
  if (!user) return;
  user = { ...user, balance };
  store().setItem(USER_KEY, JSON.stringify(user));
}

export function authHeaders(): Record<string, string> {
  return token ? { Authorization: `Bearer ${token}` } : {};
}

/**
 * Capture the marketing acquisition source from the landing URL and remember it
 * (first-touch wins). Reads `?source`, falling back to `?utm_source` / `?ref`.
 * Persisted so it survives navigation to the login screen; sent to the server on
 * the user's first Google login, where it is stamped on the new account. Safe to
 * call on every page load — it never overwrites an already-captured source, and
 * no-ops where `location` is unavailable (tests/SSR).
 */
export function captureAcquisitionSource(): void {
  try {
    if (typeof location === 'undefined') return;
    if (store().getItem(SRC_KEY)) return; // first touch already recorded
    const q = new URLSearchParams(location.search);
    const raw = q.get('source') || q.get('utm_source') || q.get('ref');
    const src = raw?.trim().slice(0, 64);
    if (src) store().setItem(SRC_KEY, src);
  } catch {
    /* storage or URL parsing blocked — attribution is best-effort */
  }
}

/** The captured acquisition source, or null if the visitor arrived organically. */
export function getAcquisitionSource(): string | null {
  return store().getItem(SRC_KEY);
}

/** Build the `/ws` URL, attaching the session token when logged in. */
export function buildWsUrl(params: URLSearchParams): string {
  if (token) params.set('token', token);
  return `${WS_BASE}/ws?${params.toString()}`;
}

/**
 * Whether the backend has accounts/credits enabled. Probes `/api/auth/config`
 * once (200 = on + carries the Google client id, 503 = guest-only) and caches.
 */
export async function billingEnabled(): Promise<boolean> {
  if (billing !== null) return billing;
  try {
    const res = await fetch(`${HTTP_BASE}/api/auth/config`, { cache: 'no-store' });
    if (res.ok) {
      const cfg = (await res.json()) as { google_client_id?: string; listener_pays?: boolean };
      googleClientId = cfg.google_client_id || '';
      listenerPays = !!cfg.listener_pays;
      billing = true;
    } else {
      billing = false;
    }
  } catch {
    billing = false;
  }
  return billing;
}

/** The Google OAuth client id (available after `billingEnabled()` resolves). */
export function getGoogleClientId(): string {
  return googleClientId;
}

/** Whether the backend runs in listener-pays mode (spec 0099) — set once
 *  `billingEnabled()` has resolved. Drives the engine-cost copy. */
export function isListenerPays(): boolean {
  return listenerPays;
}

/** Exchange a Google credential for a session; stores token + user on success.
 * Includes the captured acquisition source so the server can attribute a new
 * account to the campaign the user arrived from (first login only). */
export async function loginWithGoogle(credential: string): Promise<User> {
  const source = getAcquisitionSource() || undefined;
  const res = await fetch(`${HTTP_BASE}/api/auth/google`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ credential, source, locale: getUiLang() }),
  });
  if (!res.ok) throw new Error(`login failed (${res.status})`);
  const data = (await res.json()) as { token: string; user: User };
  saveSession(data.token, data.user);
  return data.user;
}

/** Exchange an OAuth authorization code (popup code flow, `redirect_uri:'postmessage'`)
 * for a session; stores token + user. The code flow lets the server obtain a refresh
 * token for the Calendar scope (scheduled meetings). Carries the acquisition source. */
export async function exchangeGoogleCode(code: string): Promise<User> {
  const source = getAcquisitionSource() || undefined;
  const res = await fetch(`${HTTP_BASE}/api/auth/google`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ code, redirect_uri: 'postmessage', source, locale: getUiLang() }),
  });
  if (!res.ok) throw new Error(`login failed (${res.status})`);
  const data = (await res.json()) as { token: string; user: User };
  saveSession(data.token, data.user);
  return data.user;
}

/** Re-fetch the current user (balance) from the server. */
export async function refreshMe(): Promise<User | null> {
  if (!token) return null;
  const res = await fetch(`${HTTP_BASE}/api/user/me`, { headers: authHeaders() });
  if (res.status === 401) {
    clearSession();
    return null;
  }
  if (!res.ok) return user;
  const u = (await res.json()) as User;
  saveSession(token, u);
  return u;
}

export async function fetchPackages(): Promise<CreditPackage[]> {
  const res = await fetch(`${HTTP_BASE}/api/billing/packages`, { cache: 'no-store' });
  if (!res.ok) return [];
  return (await res.json()) as CreditPackage[];
}

export async function fetchHistory(): Promise<Transaction[]> {
  const res = await fetch(`${HTTP_BASE}/api/billing/history`, { headers: authHeaders() });
  if (!res.ok) return [];
  return (await res.json()) as Transaction[];
}

export async function fetchUsage(): Promise<UsageSession[]> {
  const res = await fetch(`${HTTP_BASE}/api/usage/sessions`, { headers: authHeaders() });
  if (!res.ok) return [];
  return (await res.json()) as UsageSession[];
}

/** Recorded call sessions (transcripts) the user took part in, newest first. */
export async function fetchSessions(): Promise<CallSession[]> {
  const res = await fetch(`${HTTP_BASE}/api/sessions`, { headers: authHeaders() });
  if (!res.ok) return [];
  return (await res.json()) as CallSession[];
}

/** Trigger a browser download of a Blob under the given filename. */
export function downloadBlob(blob: Blob, filename: string): void {
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = filename;
  a.click();
  URL.revokeObjectURL(url);
}

/** `?lang=` of the SRT/VTT endpoints — which text the subtitles carry. */
export type SubtitleMode = 'original' | 'translated' | 'both';

/**
 * Download a session transcript as JSON, PDF or SRT/VTT subtitles
 * (authenticated). The PDF is localized to the browser timezone and `lang`;
 * subtitles use `lang` as the translation target. Returns `{ ok, status }` so
 * callers can distinguish a rate-limit (429) from other failures (#222); `status`
 * is 0 on a network error.
 */
export async function downloadTranscript(
  sessionId: string,
  format: 'json' | 'pdf' | 'srt' | 'vtt',
  lang = 'en',
  subtitleMode: SubtitleMode = 'translated',
  corrected = false,
): Promise<{ ok: boolean; status: number }> {
  let url = `${HTTP_BASE}/api/sessions/${encodeURIComponent(sessionId)}/transcript.${format}`;
  if (format === 'pdf') {
    let tz = 'UTC';
    try {
      tz = Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC';
    } catch {
      /* default to UTC */
    }
    url += `?tz=${encodeURIComponent(tz)}&lang=${encodeURIComponent(lang)}`;
  } else if (format === 'srt' || format === 'vtt') {
    url += `?lang=${encodeURIComponent(subtitleMode)}&target=${encodeURIComponent(lang)}`;
  }
  // Render from the cached AI correction (spec 0068). JSON has no other query.
  if (corrected) url += `${url.includes('?') ? '&' : '?'}corrected=1`;
  try {
    const res = await fetch(url, { headers: authHeaders() });
    if (!res.ok) return { ok: false, status: res.status };
    const blob = await res.blob();
    // Prefer the server-chosen filename from Content-Disposition.
    const cd = res.headers.get('content-disposition') || '';
    const name = /filename="([^"]+)"/.exec(cd)?.[1] || `voxtranslate-transcript.${format}`;
    downloadBlob(blob, name);
    return { ok: true, status: res.status };
  } catch {
    return { ok: false, status: 0 }; // network error — caller toasts a generic failure
  }
}

/** Start a Stripe Checkout Session; returns the hosted URL to redirect to. */
export async function startCheckout(packageId: string): Promise<string> {
  const res = await fetch(`${HTTP_BASE}/api/billing/checkout`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify({ package_id: packageId }),
  });
  if (!res.ok) throw new Error(`checkout failed (${res.status})`);
  const data = (await res.json()) as { url: string };
  return data.url;
}

/** Whether the logged-in user has accepted age + ToS/Privacy. */
export function consentGiven(): boolean {
  return !!user?.consent_given;
}

/** Record age (18+) + ToS/Privacy acceptance. */
export async function submitConsent(ageConfirmed: boolean): Promise<boolean> {
  const res = await fetch(`${HTTP_BASE}/api/user/consent`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify({ age_confirmed: ageConfirmed }),
  });
  if (res.ok && user) {
    user = { ...user, consent_given: true };
    store().setItem(USER_KEY, JSON.stringify(user));
  }
  return res.ok;
}

const GUEST_CONSENT_KEY = 'vox_guest_consent';

/** Whether a guest (no account) has self-attested 18+ + ToS acceptance. Client-side
 *  only — guests have no server record to gate against. */
export function guestConsentGiven(): boolean {
  return store().getItem(GUEST_CONSENT_KEY) === '1';
}

/** Record a guest's 18+ + ToS acceptance locally. */
export function setGuestConsent(): void {
  store().setItem(GUEST_CONSENT_KEY, '1');
}

/** File an abuse report against a peer. */
export async function reportUser(payload: {
  room: string;
  reported_peer_id?: string;
  reported_name?: string;
  reason: string;
  transcript_excerpt?: string;
}): Promise<boolean> {
  const res = await fetch(`${HTTP_BASE}/api/report`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...authHeaders() },
    body: JSON.stringify(payload),
  });
  return res.ok;
}

/** GDPR data export — returns the user's full data document, or null. */
export async function exportData(): Promise<unknown | null> {
  const res = await fetch(`${HTTP_BASE}/api/user/data`, { headers: authHeaders() });
  return res.ok ? res.json() : null;
}

/** GDPR erasure — deletes the account; clears the local session on success. */
export async function deleteAccount(): Promise<boolean> {
  const res = await fetch(`${HTTP_BASE}/api/user`, { method: 'DELETE', headers: authHeaders() });
  if (res.ok) clearSession();
  return res.ok;
}

/** Format a credit balance as USD (the credit unit is 1 credit = $1). */
export function formatCredits(amount: number): string {
  return `$${(amount ?? 0).toFixed(2)}`;
}

/**
 * Resize a Google avatar URL to `size` px (Google supports the `=sNN` suffix).
 * Returns null for missing/non-Google URLs unchanged-but-usable.
 */
export function avatarUrl(url: string | null | undefined, size = 96): string | null {
  if (!url) return null;
  // Google content URLs carry an `=s96-c` style suffix; replace or append it.
  if (/=s\d+(-c)?$/.test(url)) return url.replace(/=s\d+(-c)?$/, `=s${size}`);
  if (url.includes('googleusercontent.com')) return `${url}=s${size}`;
  return url;
}

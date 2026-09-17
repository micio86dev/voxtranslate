/**
 * Typed client for the existing `/api/business/organizations/{orgId}/voip/...` backend
 * (server/src/voip/routes.rs, contacts.rs), so a subscriber can dial, monitor and end a
 * translated phone call from inside the call app itself instead of the dashboard
 * (spec: web-app-voip-dialer).
 *
 * Mirrors `dashboard/src/lib/api.ts`'s voip section (read-only reference) — the same
 * `ApiResult<T>` envelope and `request()` wrapper, NOT this app's own `business.ts`
 * swallow-and-return-`[]`/`null` style. Refusal codes are the whole UX for a feature that
 * spends money: a call that fails silently is a customer billed for nothing and told
 * nothing, so every result here carries the HTTP status and the raw body a caller can
 * pass to `errorCode()` / `refusalKey()` from `./phone-dialer`.
 *
 * Every endpoint below was read directly off `server/src/voip/routes.rs` and
 * `contacts.rs` (org-scoped paths, response field names and types) rather than assumed
 * from the dashboard's own client — see the design doc's "Interfaces / Contracts"
 * section for the byte-level comparison.
 */
import { authHeaders, HTTP_BASE } from './auth';

export interface ApiResult<T> {
  ok: boolean;
  status: number;
  data: T | null;
}

async function request<T>(method: string, path: string, body?: unknown): Promise<ApiResult<T>> {
  try {
    const headers: Record<string, string> = { ...authHeaders() };
    if (body !== undefined) headers['Content-Type'] = 'application/json';
    const res = await fetch(`${HTTP_BASE}${path}`, {
      method,
      headers,
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
    let data: T | null = null;
    if (res.status !== 204) {
      data = (await res.json().catch(() => null)) as T | null;
    }
    return { ok: res.ok, status: res.status, data };
  } catch {
    return { ok: false, status: 0, data: null };
  }
}

/**
 * Read the server's stable refusal code from a non-2xx (or failed) body, for
 * `refusalKey()` in `./phone-dialer`. `null` when the body carries no `error` field —
 * a transport failure (network drop, CORS preflight refusal) is exactly this shape, and
 * the caller's own fallback key (never the generic "call failed" default) must apply.
 */
export function errorCode(data: unknown): string | null {
  const code = (data as { error?: unknown } | null)?.error;
  return typeof code === 'string' ? code : null;
}

// --- Quote + dial (spec 0111) -------------------------------------------------

export interface VoipQuote {
  /** Masked form only — the quote payload reaches a browser console. */
  destination: string;
  country: string;
  price_per_minute: string;
  currency: string;
  reserve_credits: number;
  estimated_minutes: number;
  balance_credits: number;
  engine_id: string;
  recording: boolean;
  transcription: boolean;
  consent_policy: string;
  /** Set when an announcement will be played, and in which language. */
  disclosure_language: string | null;
}

export interface VoipDialRequest {
  destination: string;
  source_language: string;
  target_language: string;
  engine_id?: string;
  project_id?: string | null;
  /** Re-resolved server-side from owned, verified numbers — never trusted as-is. */
  caller_id?: string | null;
  record?: boolean;
  transcribe?: boolean;
  ai_analysis?: boolean;
  estimated_minutes?: number;
}

export interface VoipCallCreated {
  call_id: string;
  session_id: string;
  /**
   * The room the telephone was joined into, so the caller's browser can join it too.
   * A translated phone call is a room with a telephone in it; without joining it the
   * caller is not in their own call.
   */
  room?: string | null;
  status: string;
  reserved_credits: number;
  price_per_minute: string;
}

/** `POST …/voip/quote` — price a call without placing it. */
export function quoteVoipCall(
  orgId: string,
  body: Partial<VoipDialRequest> & { destination: string },
): Promise<ApiResult<VoipQuote>> {
  return request('POST', `/api/business/organizations/${orgId}/voip/quote`, body);
}

/** `POST …/voip/calls` — place a call. */
export function dialVoipCall(
  orgId: string,
  body: VoipDialRequest,
): Promise<ApiResult<VoipCallCreated>> {
  return request('POST', `/api/business/organizations/${orgId}/voip/calls`, body);
}

// --- Call detail + hangup ------------------------------------------------------

export interface VoipCallDetail {
  id: string;
  session_id: string;
  /** Present only while the call is NOT completed/failed — a join handle, not a log. */
  room: string | null;
  status: string;
  failure_reason: string | null;
  direction: string;
  /** The full E.164 number. This endpoint is the only place it is returned. */
  recipient_e164: string;
  recipient_country: string;
  source_language: string;
  target_language: string;
  engine_id: string;
  started_at: string;
  ended_at: string | null;
  duration_seconds: number | null;
  credits_consumed: number;
  quoted_price_per_min: string | null;
  /** `'pending'` until the provider rates the leg, then `'final'` — never a number. */
  cost_status: 'pending' | 'final';
  recording_status: string;
  recording_available: boolean;
  transcription_status: string;
  ai_analysis_requested: boolean;
  consent_status: string;
  project_id: string | null;
  contact_id: string | null;
  contact_name: string | null;
}

/** `GET …/voip/calls/{id}` — one call, with the money detail. */
export function getVoipCall(orgId: string, callId: string): Promise<ApiResult<VoipCallDetail>> {
  return request('GET', `/api/business/organizations/${orgId}/voip/calls/${callId}`);
}

/**
 * `POST …/voip/calls/{id}/hangup` — best-effort tear-down of the PSTN leg. The
 * authoritative state change comes from the provider's hangup webhook, not this
 * response; `{requested: true}` only confirms the request reached the server.
 */
export function hangUpVoipCall(
  orgId: string,
  callId: string,
): Promise<ApiResult<{ requested: boolean }>> {
  return request('POST', `/api/business/organizations/${orgId}/voip/calls/${callId}/hangup`);
}

// --- Contacts (spec 0114) -------------------------------------------------------

export interface VoipContactNumber {
  id?: string;
  e164: string;
  label: string | null;
  /** The language THIS number speaks — not the person's. */
  language: string | null;
  country?: string | null;
  is_primary: boolean;
}

export interface VoipContactSummary {
  id: string;
  name: string;
  company: string | null;
  role: string | null;
  notes: string | null;
  tags: string[];
  email: string | null;
}

export interface VoipContactDetail extends VoipContactSummary {
  numbers: VoipContactNumber[];
  projects: { id: string; name: string }[];
}

export interface ContactQuery {
  q?: string;
  projectId?: string;
  tag?: string;
  language?: string;
  page?: number;
  limit?: number;
}

/** `GET …/voip/contacts` — the org's address book, so the dial UI can offer it (R8). */
export function listVoipContacts(
  orgId: string,
  opts: ContactQuery = {},
): Promise<ApiResult<{ contacts: VoipContactSummary[]; page: number; limit: number }>> {
  const q = new URLSearchParams();
  if (opts.q) q.set('q', opts.q);
  if (opts.projectId) q.set('project_id', opts.projectId);
  if (opts.tag) q.set('tag', opts.tag);
  if (opts.language) q.set('language', opts.language);
  if (opts.page) q.set('page', String(opts.page));
  if (opts.limit) q.set('limit', String(opts.limit));
  const qs = q.toString();
  return request('GET', `/api/business/organizations/${orgId}/voip/contacts${qs ? `?${qs}` : ''}`);
}

/** `GET …/voip/contacts/{id}` — one contact with its numbers + linked projects, for the
 *  dial UI's prefill (number, that number's own language, its linked project). */
export function getVoipContact(
  orgId: string,
  contactId: string,
): Promise<ApiResult<VoipContactDetail>> {
  return request('GET', `/api/business/organizations/${orgId}/voip/contacts/${contactId}`);
}

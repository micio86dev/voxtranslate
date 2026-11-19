// Live Connect REST client (spec 0121).
//
// The REST endpoints, all typed, none of which throws: every outcome is an `ApiResult`, so a
// caller handles HTTP refusals and a dead network through the same branch. The server's
// error body is `{ code, message }` and the CODE is the contract — the message is never
// shown to a user (it is English and may change), the view maps the code to i18n.
//
// Two credentials exist here and they must never meet:
//  - the ACCOUNT JWT (`authHeaders()`), used only to create a session as its paying owner, and
//    by a member of an organisation to list its desks and answer a waiting customer;
//  - the LIVE token, issued per participant, used for everything after that. It is not an
//    account credential and it is the only thing a guest has.
// Neither goes into a URL, except the live token on the WebSocket (browsers cannot set
// headers on a WebSocket), see `conversation.ts`.

import { authHeaders, HTTP_BASE } from '../auth';

export type ApiResult<T> = { ok: true; data: T } | { ok: false; status: number; code: string };

export interface CreatedSession {
  session_id: string;
  invite_url: string;
  qr_payload: string;
  expires_at: string;
  peer_id: string;
  token: string;
  ws_path: string;
}

export interface JoinedSession {
  session_id: string;
  peer_id: string;
  token: string;
  ws_path: string;
  host: { name: string; lang: string };
}

export interface SessionParticipant {
  peer_id: string;
  role: 'host' | 'guest';
  display_name: string;
  lang: string;
}

export interface SessionInfo {
  session: {
    id: string;
    status: string;
    host_lang: string;
    created_at: string;
    expires_at: string;
    participants: SessionParticipant[];
  };
  you: { peer_id: string; role: 'host' | 'guest' };
}

/** A desk QR was scanned: a NEW session every time, with the customer's own live token. */
export interface ScannedDesk {
  session_id: string;
  peer_id: string;
  token: string;
  ws_path: string;
  desk: { name: string; location_label: string | null };
  /** How long the customer can be left waiting before the server gives up on the session. */
  wait_secs: number;
}

export interface StaffDesk {
  id: string;
  org_id: string;
  org_name: string;
  name: string;
  location_label: string | null;
  default_lang: string;
  waiting: number;
}

export interface WaitingCustomer {
  session_id: string;
  guest_name: string;
  guest_lang: string;
  waited_secs: number;
}

/** A member of staff took a waiting customer: their own live token, as the staff seat. */
export interface AnsweredSession {
  session_id: string;
  peer_id: string;
  token: string;
  ws_path: string;
  guest: { name: string; lang: string };
}

type Fetch = typeof fetch;

async function call<T>(
  fetchImpl: Fetch,
  path: string,
  init: RequestInit,
): Promise<ApiResult<T>> {
  let res: Response;
  try {
    res = await fetchImpl(`${HTTP_BASE}${path}`, init);
  } catch {
    // Offline, DNS, CORS, a reset connection: indistinguishable and equally transient.
    return { ok: false, status: 0, code: 'network' };
  }
  if (res.ok) {
    try {
      return { ok: true, data: (await res.json()) as T };
    } catch {
      return { ok: false, status: res.status, code: 'server_error' };
    }
  }
  let code: string | undefined;
  try {
    const body = (await res.json()) as { code?: unknown };
    if (typeof body.code === 'string') code = body.code;
  } catch {
    // A proxy's HTML error page, an empty body: fall back to the status class below.
  }
  return { ok: false, status: res.status, code: code ?? (res.status >= 500 ? 'server_error' : 'http_error') };
}

const JSON_HEADERS = { 'Content-Type': 'application/json' };

/** Create a conversation as its (paying) owner. Needs the account JWT. */
export function createSession(
  args: { lang: string; saveTranscript: boolean },
  fetchImpl: Fetch = fetch,
): Promise<ApiResult<CreatedSession>> {
  return call<CreatedSession>(fetchImpl, '/api/live/sessions', {
    method: 'POST',
    headers: { ...JSON_HEADERS, ...authHeaders() },
    body: JSON.stringify({ lang: args.lang, save_transcript: args.saveTranscript }),
  });
}

/** Join through an invite. Deliberately NO Authorization header: a guest has no account. */
export function joinSession(
  args: { code: string; name: string; lang: string; consent: boolean },
  fetchImpl: Fetch = fetch,
): Promise<ApiResult<JoinedSession>> {
  return call<JoinedSession>(fetchImpl, '/api/live/join', {
    method: 'POST',
    headers: { ...JSON_HEADERS },
    body: JSON.stringify({
      code: args.code,
      name: args.name,
      lang: args.lang,
      consent: args.consent,
    }),
  });
}

/** The session as the server sees it. Used to rejoin after a reload and to probe a reconnect. */
export function getSession(
  sessionId: string,
  liveToken: string,
  fetchImpl: Fetch = fetch,
): Promise<ApiResult<SessionInfo>> {
  return call<SessionInfo>(fetchImpl, `/api/live/sessions/${encodeURIComponent(sessionId)}`, {
    method: 'GET',
    headers: { Authorization: `Bearer ${liveToken}` },
  });
}

/** End the conversation for both phones. */
export function endSession(
  sessionId: string,
  liveToken: string,
  fetchImpl: Fetch = fetch,
): Promise<ApiResult<unknown>> {
  return call<unknown>(fetchImpl, `/api/live/sessions/${encodeURIComponent(sessionId)}/end`, {
    method: 'POST',
    headers: { Authorization: `Bearer ${liveToken}` },
  });
}

/**
 * Scan a business desk's permanent QR. The QR never denotes a conversation, so every call
 * creates a NEW session; there is nothing to resume by repeating it. No Authorization header:
 * the customer has no account. `segment` is the `<public_id>.<mac>` path segment of the QR URL,
 * a bearer secret that goes in the body and nowhere else.
 */
export function scanDesk(
  args: { segment: string; name: string; lang: string; consent: boolean },
  fetchImpl: Fetch = fetch,
): Promise<ApiResult<ScannedDesk>> {
  return call<ScannedDesk>(fetchImpl, '/api/live/desks/scan', {
    method: 'POST',
    headers: { ...JSON_HEADERS },
    body: JSON.stringify({
      segment: args.segment,
      name: args.name,
      lang: args.lang,
      consent: args.consent,
    }),
  });
}

/** The desks of every organisation the signed-in member belongs to, with who is waiting. */
export function myDesks(fetchImpl: Fetch = fetch): Promise<ApiResult<{ desks: StaffDesk[] }>> {
  return call<{ desks: StaffDesk[] }>(fetchImpl, '/api/live/my-desks', {
    method: 'GET',
    headers: { ...authHeaders() },
  });
}

/** The customers waiting at one desk, oldest first. */
export function deskWaiting(
  deskId: string,
  fetchImpl: Fetch = fetch,
): Promise<ApiResult<{ waiting: WaitingCustomer[] }>> {
  return call<{ waiting: WaitingCustomer[] }>(fetchImpl, `/api/live/desks/${encodeURIComponent(deskId)}/waiting`, {
    method: 'GET',
    headers: { ...authHeaders() },
  });
}

/** Take a waiting customer. Exactly one colleague can: the loser gets 409 `already_answered`. */
export function answerSession(
  sessionId: string,
  lang: string,
  fetchImpl: Fetch = fetch,
): Promise<ApiResult<AnsweredSession>> {
  return call<AnsweredSession>(fetchImpl, `/api/live/sessions/${encodeURIComponent(sessionId)}/answer`, {
    method: 'POST',
    headers: { ...JSON_HEADERS, ...authHeaders() },
    body: JSON.stringify({ lang }),
  });
}

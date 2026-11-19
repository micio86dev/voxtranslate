// Live Connect REST client (spec 0121).
//
// Four endpoints, all typed, none of which throws: every outcome is an `ApiResult`, so a
// caller handles HTTP refusals and a dead network through the same branch. The server's
// error body is `{ code, message }` and the CODE is the contract — the message is never
// shown to a user (it is English and may change), the view maps the code to i18n.
//
// Two credentials exist here and they must never meet:
//  - the ACCOUNT JWT (`authHeaders()`), used only to create a session as its paying owner;
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

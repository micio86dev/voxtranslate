//! Webinar host helpers (webinar phase 0). A signed-in B2B host can create and
//! manage webinars for an org with an active subscription. Each webinar carries a
//! public join link (`join_url`, e.g. https://voxtranslate.app/w/{code}) that the
//! screen renders as a copyable link + QR code.
//!
//! Pure fetch glue over the host endpoints under `/api/webinars`. All calls carry
//! the Bearer JWT (`authHeaders()`); on a non-2xx the request-returning helpers
//! throw a typed `WebinarError` so the UI can map the HTTP status to a message.

import { authHeaders, HTTP_BASE } from "./auth";
import type { BusinessOrg } from "./business";

/** Webinar translation tier. `enhanced` is the default (Cartesia Sonic). */
export type WebinarTier = "standard" | "enhanced";

/** Lifecycle status of a webinar as returned by the host endpoints. */
export type WebinarStatus = "scheduled" | "live" | "ended" | "cancelled";

/** The host view of a webinar (the shape every host endpoint returns). */
export interface WebinarView {
  id: string;
  org_id: string;
  /** Short public code embedded in `join_url` (e.g. `ab12cd`). */
  code: string;
  title: string;
  description: string | null;
  source_language: string;
  tier: WebinarTier;
  status: WebinarStatus;
  scheduled_start: string | null;
  scheduled_end: string | null;
  actual_start: string | null;
  actual_end: string | null;
  record_video: boolean;
  record_transcript: boolean;
  voice_clone: boolean;
  /** Public join link — encode THIS verbatim in the QR (never rebuild it). */
  join_url: string;
  playback_url: string | null;
  created_at: string;
}

/** The public (no-auth) view of a webinar for a participant landing on `/w/{code}`.
 *  Returned by `GET /api/w/{code}`. `playback_url` is the LL-HLS manifest to play;
 *  `guest_id` is a stable anonymous id the participant persists locally. */
export interface PublicWebinar {
  code: string;
  title: string;
  status: WebinarStatus;
  source_language: string;
  tier: WebinarTier;
  join_url: string;
  /** LL-HLS manifest `https://{hls_host}/webinar/{code}/index.m3u8` (may be absent
   *  until the host goes live). */
  playback_url: string | null;
  guest_id: string;
}

/** Response of `POST /api/webinars/{id}/go-live`: a short-lived tokenized WHIP
 *  ingest URL to publish to. Fetch it right before publishing — the token in
 *  `publish_url` expires in ~`expires_in` seconds. */
export interface GoLiveResponse {
  /** Tokenized WHIP endpoint (the `?token=` is already embedded — POST verbatim). */
  publish_url: string;
  expires_in: number;
}

/** Body for `POST /api/webinars`. */
export interface CreateWebinarBody {
  org_id: string;
  title: string;
  source_language: string;
  tier?: WebinarTier;
  record_video?: boolean;
  record_transcript?: boolean;
  voice_clone?: boolean;
  scheduled_start?: string | null;
  scheduled_end?: string | null;
}

/** Body for `PATCH /api/webinars/{id}` — every field optional. */
export interface PatchWebinarBody {
  title?: string;
  description?: string | null;
  tier?: WebinarTier;
  record_video?: boolean;
  record_transcript?: boolean;
  voice_clone?: boolean;
  scheduled_start?: string | null;
  scheduled_end?: string | null;
}

/** A failed host request. `status` is the HTTP status (0 on a network error) so
 *  the UI can distinguish 401/402/404/409 and toast the right message. */
export class WebinarError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.name = "WebinarError";
    this.status = status;
  }
}

/** Hosting a webinar is a paid feature — only orgs with an active subscription. */
export function canHostWebinar(org: BusinessOrg | undefined): boolean {
  return org?.subscription_status === "active";
}

/** Parse a host response, throwing a typed `WebinarError` on a non-2xx. */
async function parse<T>(res: Response): Promise<T> {
  if (!res.ok) {
    let message = `request failed (${res.status})`;
    try {
      const body = (await res.json()) as { error?: string; message?: string };
      message = body.error || body.message || message;
    } catch {
      /* non-JSON error body — keep the generic message */
    }
    throw new WebinarError(res.status, message);
  }
  return (await res.json()) as T;
}

/** Wrap a network failure (fetch rejection) as a `WebinarError` with status 0. */
function netError(): never {
  throw new WebinarError(0, "network error");
}

/** Create a webinar for an org. Throws `WebinarError` on failure. */
export async function createWebinar(body: CreateWebinarBody): Promise<WebinarView> {
  let res: Response;
  try {
    res = await fetch(`${HTTP_BASE}/api/webinars`, {
      method: "POST",
      headers: { ...authHeaders(), "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
  } catch {
    netError();
  }
  return parse<WebinarView>(res);
}

/** List an org's webinars. Throws `WebinarError` on failure. */
export async function listWebinars(orgId: string): Promise<WebinarView[]> {
  let res: Response;
  try {
    res = await fetch(
      `${HTTP_BASE}/api/webinars?org_id=${encodeURIComponent(orgId)}`,
      { headers: authHeaders() },
    );
  } catch {
    netError();
  }
  return parse<WebinarView[]>(res);
}

/** Fetch a single webinar (host view). Throws `WebinarError` on failure. */
export async function getWebinar(id: string): Promise<WebinarView> {
  let res: Response;
  try {
    res = await fetch(`${HTTP_BASE}/api/webinars/${encodeURIComponent(id)}`, {
      headers: authHeaders(),
    });
  } catch {
    netError();
  }
  return parse<WebinarView>(res);
}

/** Patch a scheduled webinar. Throws `WebinarError` (409 once it's no longer
 *  scheduled). */
export async function patchWebinar(
  id: string,
  body: PatchWebinarBody,
): Promise<WebinarView> {
  let res: Response;
  try {
    res = await fetch(`${HTTP_BASE}/api/webinars/${encodeURIComponent(id)}`, {
      method: "PATCH",
      headers: { ...authHeaders(), "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
  } catch {
    netError();
  }
  return parse<WebinarView>(res);
}

/** Cancel a webinar. Throws `WebinarError` on failure. */
export async function cancelWebinar(id: string): Promise<WebinarView> {
  let res: Response;
  try {
    res = await fetch(
      `${HTTP_BASE}/api/webinars/${encodeURIComponent(id)}/cancel`,
      { method: "POST", headers: authHeaders() },
    );
  } catch {
    netError();
  }
  return parse<WebinarView>(res);
}

/** Request a fresh tokenized WHIP publish URL for a webinar (host only). The token
 *  embedded in `publish_url` is short-lived (~120s), so call this immediately before
 *  publishing. Throws `WebinarError` on failure. */
export async function goLive(id: string): Promise<GoLiveResponse> {
  let res: Response;
  try {
    res = await fetch(
      `${HTTP_BASE}/api/webinars/${encodeURIComponent(id)}/go-live`,
      { method: "POST", headers: authHeaders() },
    );
  } catch {
    netError();
  }
  return parse<GoLiveResponse>(res);
}

/** Tell the server the WHIP publish is connected (status → `live`). Call once the
 *  publisher's peer connection reaches `connected`. Throws `WebinarError` on failure. */
export async function publishStarted(id: string): Promise<WebinarView> {
  let res: Response;
  try {
    res = await fetch(
      `${HTTP_BASE}/api/webinars/${encodeURIComponent(id)}/publish-started`,
      { method: "POST", headers: authHeaders() },
    );
  } catch {
    netError();
  }
  return parse<WebinarView>(res);
}

/** Tell the server the publish stopped (status → `ended`). Call on manual stop or
 *  page unload. Throws `WebinarError` on failure. */
export async function publishStopped(id: string): Promise<WebinarView> {
  let res: Response;
  try {
    res = await fetch(
      `${HTTP_BASE}/api/webinars/${encodeURIComponent(id)}/publish-stopped`,
      { method: "POST", headers: authHeaders() },
    );
  } catch {
    netError();
  }
  return parse<WebinarView>(res);
}

/** Fetch the public (no-auth) view of a webinar by its short code. Used by the
 *  participant page `/w/{code}`. Throws `WebinarError` (404 when the code is unknown
 *  or the webinar was cancelled). */
export async function getPublicWebinar(code: string): Promise<PublicWebinar> {
  let res: Response;
  try {
    res = await fetch(`${HTTP_BASE}/api/w/${encodeURIComponent(code)}`);
  } catch {
    netError();
  }
  return parse<PublicWebinar>(res);
}

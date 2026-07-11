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

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

/** Whether a webinar is discoverable. `private` (the default) is reachable only by
 *  its direct `/w/{code}` link; `public` also gets listed on the app home page. */
export type WebinarVisibility = "public" | "private";

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
  /** Discovery mode: `private` (default, link-only) or `public` (listed on the home). */
  visibility: WebinarVisibility;
  /** Business project this webinar is filed under, or null when unassigned. */
  project_id: string | null;
  scheduled_start: string | null;
  scheduled_end: string | null;
  actual_start: string | null;
  actual_end: string | null;
  record_video: boolean;
  record_transcript: boolean;
  voice_clone: boolean;
  /** Whether the auto-translated chat (Feature ⑤) is enabled for this webinar. */
  chat_enabled: boolean;
  /** Public join link — encode THIS verbatim in the QR (never rebuild it). */
  join_url: string;
  playback_url: string | null;
  created_at: string;
  /** ISO timestamp the webinar was soft-archived, or null while it's active. */
  archived_at: string | null;
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
  /** Whether the auto-translated chat (Feature ⑤) is enabled for this webinar. */
  chat_enabled: boolean;
  /** LL-HLS manifest `https://{hls_host}/webinar/{code}/index.m3u8` (may be absent
   *  until the host goes live). */
  playback_url: string | null;
  guest_id: string;
  /** Host's avatar URL, snapshotted at webinar creation time. Shown in the waiting
   *  overlay before the stream starts. Null when the host has no avatar or for webinars
   *  created before migration 043. */
  host_avatar_url: string | null;
  /** When true, only authenticated users may join the presence WebSocket. */
  members_only: boolean;
  /** ISO-8601 timestamp the host went live, or null while the webinar hasn't started.
   *  Drives the audience live timer's elapsed count. */
  actual_start?: string | null;
  /** ISO-8601 scheduled end, or null when the webinar was created without an end.
   *  When present, the live timer can toggle to a countdown to this instant. */
  scheduled_end?: string | null;
}

/** Response of `POST /api/webinars/{id}/go-live`: a short-lived tokenized WHIP
 *  ingest URL to publish to. Fetch it right before publishing — the token in
 *  `publish_url` expires in ~`expires_in` seconds. */
export interface GoLiveResponse {
  /** Tokenized WHIP endpoint (the `?token=` is already embedded — POST verbatim). */
  publish_url: string;
  expires_in: number;
}

/** Response of `POST /api/webinars/{id}/calendar`: the Google Calendar event that
 *  was created for a scheduled webinar. `html_link` opens the event in the user's
 *  calendar UI; `join_url` is the webinar's public link that was embedded in it. */
export interface CalendarEventResponse {
  google_event_id: string;
  html_link: string;
  join_url: string;
}

/** Body for `POST /api/webinars`. */
export interface CreateWebinarBody {
  org_id: string;
  title: string;
  source_language: string;
  /** Optional business project to file the webinar under (omit/null for none). */
  project_id?: string | null;
  tier?: WebinarTier;
  record_video?: boolean;
  record_transcript?: boolean;
  voice_clone?: boolean;
  /** Enable the auto-translated chat (Feature ⑤) for the webinar. */
  chat_enabled?: boolean;
  /** Discovery mode; defaults to `private` server-side when omitted. */
  visibility?: WebinarVisibility;
  /** When true, only authenticated users can join the presence WebSocket. */
  members_only?: boolean;
  scheduled_start?: string | null;
  scheduled_end?: string | null;
}

/** Body for `PATCH /api/webinars/{id}` — every field optional. */
export interface PatchWebinarBody {
  title?: string;
  description?: string | null;
  /** Reassign the business project the webinar is filed under. Server-side is
   * COALESCE-based: sending a project id reassigns; null/omit leaves it unchanged
   * (there is no unlink today). */
  project_id?: string | null;
  tier?: WebinarTier;
  record_video?: boolean;
  record_transcript?: boolean;
  voice_clone?: boolean;
  /** Enable/disable the auto-translated chat (Feature ⑤) for the webinar. */
  chat_enabled?: boolean;
  /** Change the discovery mode (`public` lists it on the home; `private` hides it). */
  visibility?: WebinarVisibility;
  /** Enable or disable the members-only gate. */
  members_only?: boolean;
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

/** Filename for a downloaded webinar QR PNG, e.g. `webinar-ab12cd.png`. The code is
 *  sanitized to safe filename characters (letters, digits, `-`, `_`); anything else
 *  becomes `-`, and an empty result falls back to `webinar` so the name is always valid. */
export function qrDownloadFilename(code: string): string {
  const safe = (code || "").replace(/[^a-zA-Z0-9_-]+/g, "-").replace(/^-+|-+$/g, "");
  return `webinar-${safe || "webinar"}.png`;
}

/** Whether the "Clone my voice" toggle should be offered when creating a webinar.
 *  Voice cloning runs only on the Enhanced tier (Cartesia); Standard (Deepgram)
 *  cannot clone. It's also moot once the host already has a cloned voice — the
 *  existing clone is reused automatically — so hide it in that case. */
export function showVoiceCloneToggle(
  tier: WebinarTier | string,
  hasClone: boolean,
): boolean {
  return tier === "enhanced" && !hasClone;
}

/** Whether the pre-live "Clone your voice" action should be offered on a webinar
 *  card: only for Enhanced webinars whose host has not cloned a voice yet. Standard
 *  webinars can't use a cloned voice, and an already-cloned voice is reused. */
export function showWebinarCloneAction(
  tier: WebinarTier | string,
  hasClone: boolean,
): boolean {
  return tier === "enhanced" && !hasClone;
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

/** List an org's webinars. Pass `archived: true` for the archived (historical) list;
 *  the default `false` returns the active list. Throws `WebinarError` on failure. */
export async function listWebinars(
  orgId: string,
  archived = false,
): Promise<WebinarView[]> {
  let res: Response;
  try {
    res = await fetch(
      `${HTTP_BASE}/api/webinars?org_id=${encodeURIComponent(orgId)}` +
        `&archived=${archived}`,
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

/** Soft-archive a webinar (host only): the server sets `archived_at` and drops it
 *  from the active list. Returns the archived webinar (`archived_at` populated).
 *  Throws `WebinarError` on failure. */
export async function archiveWebinar(id: string): Promise<WebinarView> {
  let res: Response;
  try {
    res = await fetch(
      `${HTTP_BASE}/api/webinars/${encodeURIComponent(id)}/archive`,
      { method: "POST", headers: authHeaders() },
    );
  } catch {
    netError();
  }
  return parse<WebinarView>(res);
}

/** Restore an archived webinar (host only): the server clears `archived_at` and it
 *  reappears in the active list. Returns the restored webinar (`archived_at: null`).
 *  Throws `WebinarError` on failure. */
export async function unarchiveWebinar(id: string): Promise<WebinarView> {
  let res: Response;
  try {
    res = await fetch(
      `${HTTP_BASE}/api/webinars/${encodeURIComponent(id)}/unarchive`,
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

/** Add a scheduled webinar to the host's Google Calendar (host only). The server
 *  creates the event (title, times, the public join link) and returns it. A `409`
 *  means the host hasn't connected Google Calendar yet — the UI should route them
 *  through the connect-calendar flow and retry; a `400` means the webinar isn't
 *  scheduled (no start time to place on the calendar). Throws `WebinarError`. */
export async function addToCalendar(id: string): Promise<CalendarEventResponse> {
  let res: Response;
  try {
    res = await fetch(
      `${HTTP_BASE}/api/webinars/${encodeURIComponent(id)}/calendar`,
      { method: "POST", headers: authHeaders() },
    );
  } catch {
    netError();
  }
  return parse<CalendarEventResponse>(res);
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

/** A single entry in the home page's "Public webinars" list, as returned by the
 *  unauthenticated `GET /api/webinars/public` endpoint. Only public, non-archived,
 *  scheduled-or-live webinars appear (live-first). `viewers` is the live audience
 *  count (0 while scheduled). Distinct from `PublicWebinar` (the full participant
 *  view for `/w/{code}`): this is the compact card shape for the lobby list. */
export interface PublicWebinarListItem {
  code: string;
  title: string;
  /** `scheduled` or `live` only (ended/cancelled/archived are never listed). */
  status: "scheduled" | "live";
  source_language: string;
  tier: WebinarTier;
  scheduled_start: string | null;
  /** Public join link — navigate here to open the participant page (`/w/{code}`). */
  join_url: string;
  /** Live audience count; 0 for a scheduled (not-yet-live) webinar. */
  viewers: number;
  /** When true, only authenticated users may join; guests see the metadata but are shown a login gate. */
  members_only: boolean;
}

/** Whether a public-list webinar is currently broadcasting (drives the LIVE badge
 *  and whether the viewers count is shown). Pure. */
export function isWebinarLive(item: { status: string }): boolean {
  return item.status === "live";
}

/** Format a live-webinar clock as zero-padded `HH:MM` from a total number of seconds
 *  (e.g. `0` → `00:00`, `65` → `00:01`, `3600` → `01:00`, `3725` → `01:02`). Sub-minute
 *  seconds are truncated to the current whole minute. Negative / non-finite input clamps
 *  to `00:00`. Pure + input-driven (no `Date.now()`) so the studio and audience timers
 *  can share and unit-test it deterministically. */
export function formatWebinarClock(totalSeconds: number): string {
  const s = Number.isFinite(totalSeconds) ? Math.max(0, Math.floor(totalSeconds)) : 0;
  const pad = (n: number) => String(n).padStart(2, "0");
  const hours = Math.floor(s / 3600);
  const minutes = Math.floor((s % 3600) / 60);
  return `${pad(hours)}:${pad(minutes)}`;
}

/** Format a webinar's `scheduled_start` for the card's "scheduled" hint: a short
 *  localized date + time (e.g. `Jul 12, 3:00 PM`), or "" when there is no scheduled
 *  start (an immediate webinar) or the value is unparseable. Pure. */
export function formatScheduledStart(iso: string | null | undefined): string {
  if (!iso) return "";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  return d.toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  });
}

/** Fetch the public webinar list for the home page. Unauthenticated GET; best-effort
 *  — any failure (network, non-2xx, malformed body) resolves to `[]` so the home lobby
 *  never breaks over an optional discovery section. Returns `json.webinars` unwrapped. */
export async function listPublicWebinars(): Promise<PublicWebinarListItem[]> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/webinars/public`, {
      headers: authHeaders(),
    });
    if (!res.ok) return [];
    const json = (await res.json()) as { webinars?: PublicWebinarListItem[] };
    return Array.isArray(json.webinars) ? json.webinars : [];
  } catch {
    return [];
  }
}

/** The device ids chosen in the webinar pre-live step (Meet-style device pickers).
 *  Empty/undefined means "let the browser pick the default device". */
export interface WebinarDeviceChoice {
  /** Selected microphone `deviceId` (from `enumerateDevices`), or empty for default. */
  audioDeviceId?: string;
  /** Selected camera `deviceId`, or empty for default. */
  videoDeviceId?: string;
  /** Whether to start capture with the webcam on. Default false (mic-only). */
  withCamera?: boolean;
}

/** Build the `MediaStreamConstraints` for a WHIP publish from the host's pre-live
 *  device choice. The microphone is ALWAYS requested (a webinar needs audio); a
 *  specific device is pinned with `deviceId: { exact }`, otherwise `true` lets the
 *  browser pick the default. The camera is requested only when `withCamera` is set —
 *  when off it is `false` so `getUserMedia` never lights the camera. Pure + framework
 *  free so the pre-join's device choice flows into `WhipPublisher` deterministically. */
export function buildPublishConstraints(
  choice: WebinarDeviceChoice = {},
): MediaStreamConstraints {
  const audio: MediaTrackConstraints | boolean = choice.audioDeviceId
    ? { deviceId: { exact: choice.audioDeviceId } }
    : true;
  let video: MediaTrackConstraints | boolean = false;
  if (choice.withCamera) {
    // Request a 16:9 resolution so cameras don't default to 4:3 (640x480). The
    // ideal hints let the browser pick the nearest supported 16:9 mode.
    video = choice.videoDeviceId
      ? {
          deviceId: { exact: choice.videoDeviceId },
          width: { ideal: 1280 },
          height: { ideal: 720 },
        }
      : { width: { ideal: 1280 }, height: { ideal: 720 } };
  }
  return { audio, video };
}

/** Format an ISO timestamp (or null) for a `<input type="datetime-local">` value:
 *  the local `YYYY-MM-DDTHH:mm` the control expects. Returns "" for null/invalid so
 *  an unscheduled (immediate) webinar leaves the inputs empty. Pure. */
export function toDatetimeLocalValue(iso: string | null | undefined): string {
  if (!iso) return "";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  const pad = (n: number) => String(n).padStart(2, "0");
  return (
    `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}` +
    `T${pad(d.getHours())}:${pad(d.getMinutes())}`
  );
}

/** Convert a `datetime-local` input value (local wall-clock) to an ISO-8601 UTC
 *  string for the webinar create/patch body, or null when the field is empty (an
 *  immediate webinar). Returns null for an unparseable value too. Pure. */
export function fromDatetimeLocalValue(value: string): string | null {
  const v = value.trim();
  if (!v) return null;
  const d = new Date(v);
  if (Number.isNaN(d.getTime())) return null;
  return d.toISOString();
}

/** Outcome of validating an optional webinar schedule client-side, mirroring the
 *  server's 400 rules so the UI can flag it inline before the API call. `ok` means
 *  the (possibly partial) schedule is valid to submit. */
export type ScheduleValidity = "ok" | "startPast" | "endBeforeStart";

/** Small clock-skew tolerance (ms) so a "start = right now" schedule isn't
 *  bounced by sub-minute drift; matches the server's 2-minute past tolerance. */
const SCHEDULE_PAST_TOLERANCE_MS = 2 * 60 * 1000;

/** Validate an optional webinar schedule (ISO strings, e.g. from
 *  [`fromDatetimeLocalValue`]) against `nowMs`. Pure + `now`-injected so it's
 *  deterministic and DOM-free.
 *
 *  - A `startISO` in the clear past (`start < now - 2min`) → `startPast`.
 *  - An `endISO` at or before the effective start → `endBeforeStart`.
 *  - Missing/unparseable values are skipped (an immediate/partial schedule is ok).
 *  The start-in-future check runs first, so a past start with a bad range still
 *  reports `startPast`. */
export function validateSchedule(
  startISO: string | null,
  endISO: string | null,
  nowMs: number,
): ScheduleValidity {
  const start = startISO ? new Date(startISO).getTime() : NaN;
  const end = endISO ? new Date(endISO).getTime() : NaN;
  if (!Number.isNaN(start) && start < nowMs - SCHEDULE_PAST_TOLERANCE_MS) {
    return "startPast";
  }
  if (!Number.isNaN(start) && !Number.isNaN(end) && end <= start) {
    return "endBeforeStart";
  }
  return "ok";
}

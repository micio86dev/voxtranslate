// Fetch wrappers for the advanced-feature REST endpoints (specs 0011+):
// transcript documents, AI pricing, and — in later phases — bookmarks,
// subtitles, glossaries, reports, sentiment and email. Auth/session plumbing
// stays in auth.ts; this module only talks JSON.

import { authHeaders, HTTP_BASE } from './auth';

// ---- Transcript document (GET /api/sessions/{id}/transcript.json) ----------

export interface TranscriptParticipant {
  /** The peer id (never a user UUID). */
  id: string;
  name: string;
  language: string;
}

export interface TranscriptEvent {
  type: 'speech' | 'chat' | string;
  ts: string;
  speaker_id: string;
  speaker_name: string;
  lang: string;
  original: string;
  /** `{ lang: text }` for every target language in the room at capture time. */
  translations: Record<string, string>;
}

export interface ExportBookmark {
  ts: string;
  label?: string | null;
  /** Creator's display name (user UUIDs never leave the server). */
  by: string;
}

export interface TranscriptDoc {
  session: {
    id: string;
    room_name: string;
    started_at: string;
    ended_at?: string | null;
    duration_seconds: number;
    participants: TranscriptParticipant[];
  };
  events: TranscriptEvent[];
  bookmarks: ExportBookmark[];
  exported_at: string;
}

/** Full transcript document for a session, or null on 403/404/network error. */
export async function fetchTranscript(sessionId: string): Promise<TranscriptDoc | null> {
  try {
    const res = await fetch(
      `${HTTP_BASE}/api/sessions/${encodeURIComponent(sessionId)}/transcript.json`,
      { headers: authHeaders() },
    );
    if (!res.ok) return null;
    return (await res.json()) as TranscriptDoc;
  } catch {
    return null;
  }
}

// ---- Bookmarks (REST under /api/sessions/{id}/bookmarks) -------------------

export interface Bookmark {
  id: string;
  ts: string;
  label?: string | null;
  /** Creator's display name (user UUIDs never leave the server). */
  by: string;
  /** True when the viewer owns it — gates the edit/delete UI. */
  mine: boolean;
}

/** All participants' bookmarks, chronological; null on 403/404/network error. */
export async function fetchBookmarks(sessionId: string): Promise<Bookmark[] | null> {
  try {
    const res = await fetch(
      `${HTTP_BASE}/api/sessions/${encodeURIComponent(sessionId)}/bookmarks`,
      { headers: authHeaders() },
    );
    if (!res.ok) return null;
    return (await res.json()) as Bookmark[];
  } catch {
    return null;
  }
}

/**
 * Pin a moment. No `ts` is sent — the server stamps "now", avoiding client
 * clock skew; the in-call flow POSTs instantly and PATCHes the label after.
 */
export async function addBookmark(sessionId: string): Promise<Bookmark | null> {
  try {
    const res = await fetch(
      `${HTTP_BASE}/api/sessions/${encodeURIComponent(sessionId)}/bookmarks`,
      {
        method: 'POST',
        headers: { ...authHeaders(), 'Content-Type': 'application/json' },
        body: '{}',
      },
    );
    if (!res.ok) return null;
    return (await res.json()) as Bookmark;
  } catch {
    return null;
  }
}

/** Relabel an owned bookmark (empty label clears it). */
export async function updateBookmarkLabel(
  sessionId: string,
  bookmarkId: string,
  label: string,
): Promise<boolean> {
  try {
    const res = await fetch(
      `${HTTP_BASE}/api/sessions/${encodeURIComponent(sessionId)}/bookmarks/${encodeURIComponent(bookmarkId)}`,
      {
        method: 'PATCH',
        headers: { ...authHeaders(), 'Content-Type': 'application/json' },
        body: JSON.stringify({ label }),
      },
    );
    return res.ok;
  } catch {
    return false;
  }
}

/** Delete an owned bookmark. */
export async function deleteBookmark(sessionId: string, bookmarkId: string): Promise<boolean> {
  try {
    const res = await fetch(
      `${HTTP_BASE}/api/sessions/${encodeURIComponent(sessionId)}/bookmarks/${encodeURIComponent(bookmarkId)}`,
      { method: 'DELETE', headers: authHeaders() },
    );
    return res.ok;
  } catch {
    return false;
  }
}

// ---- Room glossary (REST under /api/rooms/{room}/glossary) ------------------

export interface GlossaryEntry {
  /** Present on saved entries; the editor sends rows without ids. */
  id?: string;
  source_lang: string;
  target_lang: string;
  source_term: string;
  target_term: string;
}

export interface Glossary {
  name: string | null;
  entries: GlossaryEntry[];
  /** Server-side cap (GLOSSARY_MAX_ENTRIES) — shown in the editor. */
  max_entries: number;
}

/** Save/import outcome: `glossary` on success, else the server's 400 text. */
export interface GlossaryResult {
  glossary: Glossary | null;
  /** Empty on network failure (the caller shows a generic message). */
  error: string;
}

const glossaryUrl = (room: string) => `${HTTP_BASE}/api/rooms/${encodeURIComponent(room)}/glossary`;

/** The room's glossary (empty one for fresh rooms); null on 401/network error. */
export async function fetchGlossary(room: string): Promise<Glossary | null> {
  try {
    const res = await fetch(glossaryUrl(room), { headers: authHeaders() });
    if (res.status === 404) return { name: null, entries: [], max_entries: 200 };
    if (!res.ok) return null;
    return (await res.json()) as Glossary;
  } catch {
    return null;
  }
}

/** Run a glossary POST and normalize the ok/400 outcome. */
async function glossaryPost(url: string, body: unknown): Promise<GlossaryResult> {
  try {
    const res = await fetch(url, {
      method: 'POST',
      headers: { ...authHeaders(), 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
    if (!res.ok) return { glossary: null, error: await res.text() };
    return { glossary: (await res.json()) as Glossary, error: '' };
  } catch {
    return { glossary: null, error: '' };
  }
}

/** Replace the room glossary (name + full entry list). 400 → validation text. */
export function saveGlossary(
  room: string,
  name: string | null,
  entries: GlossaryEntry[],
): Promise<GlossaryResult> {
  return glossaryPost(glossaryUrl(room), { name, entries });
}

/** Merge a CSV (source_lang,target_lang,source_term,target_term) into the glossary. */
export function importGlossaryCsv(room: string, csv: string): Promise<GlossaryResult> {
  return glossaryPost(`${glossaryUrl(room)}/import`, { csv });
}

/** Delete the whole room glossary (idempotent server-side). */
export async function deleteGlossary(room: string): Promise<boolean> {
  try {
    const res = await fetch(glossaryUrl(room), { method: 'DELETE', headers: authHeaders() });
    return res.ok;
  } catch {
    return false;
  }
}

// ---- Chat file upload (spec 0018) ------------------------------------------

/** MVP-supported upload extensions (mirrors the server's `classify_ext`). */
export const UPLOAD_EXTS = ['mp3', 'wav', 'txt', 'pdf'] as const;

/** The `accept` attribute value for the file picker. */
export const UPLOAD_ACCEPT = '.mp3,.wav,.txt,.pdf,audio/mpeg,audio/wav,text/plain,application/pdf';

/** Client-side size cap (must stay ≤ the server's `SUPABASE_MAX_UPLOAD_BYTES`). */
export const UPLOAD_MAX_BYTES = 25 * 1024 * 1024;

/** Why a client-side pre-check rejected a file (before any network call). */
export type UploadReject = 'type' | 'size';

/** Validate a file against the supported types + size cap. `null` = OK. */
export function checkUploadFile(file: File): UploadReject | null {
  const ext = file.name.split('.').pop()?.toLowerCase() ?? '';
  if (!(UPLOAD_EXTS as readonly string[]).includes(ext)) return 'type';
  if (file.size > UPLOAD_MAX_BYTES || file.size === 0) return 'size';
  return null;
}

/** Outcome of an upload: `ok` true, else the server status / network failure. */
export interface UploadResult {
  ok: boolean;
  /** HTTP status (0 = network error / aborted). */
  status: number;
}

/**
 * Upload a file into the room chat. Uses XHR (not fetch) so we can report
 * upload progress (0–1). The translated message is delivered separately over
 * the WebSocket as a normal `chat_message`, so there is nothing to render from
 * the response beyond success/failure.
 */
export function uploadChatFile(
  room: string,
  peerId: string,
  file: File,
  onProgress?: (fraction: number) => void,
): Promise<UploadResult> {
  return new Promise((resolve) => {
    const form = new FormData();
    form.append('peer_id', peerId);
    form.append('file', file, file.name);

    const xhr = new XMLHttpRequest();
    xhr.open('POST', `${HTTP_BASE}/api/rooms/${encodeURIComponent(room)}/files`);
    for (const [k, v] of Object.entries(authHeaders())) xhr.setRequestHeader(k, v);
    if (onProgress && xhr.upload) {
      xhr.upload.onprogress = (e) => {
        if (e.lengthComputable) onProgress(e.loaded / e.total);
      };
    }
    xhr.onload = () => resolve({ ok: xhr.status >= 200 && xhr.status < 300, status: xhr.status });
    xhr.onerror = () => resolve({ ok: false, status: 0 });
    xhr.onabort = () => resolve({ ok: false, status: 0 });
    xhr.send(form);
  });
}

/** Whether the backend has chat file upload configured (Supabase Storage). */
export async function fileUploadEnabled(): Promise<boolean> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/files/config`, { cache: 'no-store' });
    if (!res.ok) return false;
    const cfg = (await res.json()) as { enabled?: boolean };
    return cfg.enabled === true;
  } catch {
    return false;
  }
}

// ---- AI pricing (GET /api/billing/ai-pricing) -------------------------------

export interface AiPricing {
  report: { base: number; per_minute: number };
  sentiment: { base: number; per_participant: number; per_minute: number };
  email: { draft: number };
  suggestions: { per_minute: number; interval_seconds: number };
  /** False when the backend has no Resend credentials (email feature 503s). */
  email_enabled: boolean;
}

let pricingCache: AiPricing | null = null;

/** Per-feature user rates for cost previews. Cached for the page lifetime. */
export async function fetchAiPricing(): Promise<AiPricing | null> {
  if (pricingCache) return pricingCache;
  try {
    const res = await fetch(`${HTTP_BASE}/api/billing/ai-pricing`, { cache: 'no-store' });
    if (!res.ok) return null;
    pricingCache = (await res.json()) as AiPricing;
    return pricingCache;
  } catch {
    return null;
  }
}

// ---- AI report (REST under /api/sessions/{id}/report) -----------------------

export interface AiReport {
  /** Absent when the server delivered an unsaved report (insert failed). */
  id?: string;
  format: string;
  lang: string;
  guidelines?: string | null;
  markdown: string;
  model: string;
  cost: number;
  created_at?: string;
  /** New balance after the charge; absent on GET and on free delivery. */
  balance?: number;
}

/** Generation outcome: exactly one of the three fields is meaningful. */
export interface AiReportResult {
  report: AiReport | null;
  /** The standard 402 body when credits ran short. */
  insufficient: InsufficientCredits | null;
  /** Server error text; empty on network failure (caller shows a generic message). */
  error: string;
}

const reportUrl = (sessionId: string) =>
  `${HTTP_BASE}/api/sessions/${encodeURIComponent(sessionId)}/report`;

/** Latest stored report for the session; null when none / 403 / network error. */
export async function fetchLatestReport(sessionId: string): Promise<AiReport | null> {
  try {
    const res = await fetch(reportUrl(sessionId), { headers: authHeaders() });
    if (!res.ok) return null;
    return (await res.json()) as AiReport;
  } catch {
    return null;
  }
}

/** Generate (and charge for) a new AI report. Empty guidelines are omitted. */
export async function generateReport(
  sessionId: string,
  opts: { format: string; lang: string; guidelines: string },
): Promise<AiReportResult> {
  try {
    const res = await fetch(reportUrl(sessionId), {
      method: 'POST',
      headers: { ...authHeaders(), 'Content-Type': 'application/json' },
      body: JSON.stringify({
        format: opts.format,
        lang: opts.lang,
        guidelines: opts.guidelines.trim() || null,
      }),
    });
    if (res.status === 402) {
      return { report: null, insufficient: await parseInsufficient(res), error: '' };
    }
    if (!res.ok) return { report: null, insufficient: null, error: await res.text() };
    return { report: (await res.json()) as AiReport, insufficient: null, error: '' };
  } catch {
    return { report: null, insufficient: null, error: '' };
  }
}

// ---- Sentiment analysis (REST under /api/sessions/{id}/sentiment) -----------

/** The aggregated analysis the server stores per session. */
export interface SentimentResult {
  overall: { score: number; mood: string };
  timeline: { t: number; score: number }[];
  speakers: { name: string; talk_pct: number; score: number | null; mood: string | null }[];
  key_moments: { t: number; label: string; score: number }[];
  window_secs: number;
}

export interface AiSentiment {
  /** Absent when the server delivered an unsaved analysis (insert race). */
  id?: string;
  result: SentimentResult;
  model: string;
  cost: number;
  created_at?: string;
  /** True when this came from the per-session cache (nobody was charged). */
  cached: boolean;
  /** New balance after the charge; absent on GET and cache hits. */
  balance?: number;
}

/** Generation outcome: exactly one of the three fields is meaningful. */
export interface AiSentimentResult {
  sentiment: AiSentiment | null;
  insufficient: InsufficientCredits | null;
  error: string;
}

const sentimentUrl = (sessionId: string) =>
  `${HTTP_BASE}/api/sessions/${encodeURIComponent(sessionId)}/sentiment`;

/** Cached analysis for the session; null when none / 403 / network error. */
export async function fetchSentiment(sessionId: string): Promise<AiSentiment | null> {
  try {
    const res = await fetch(sentimentUrl(sessionId), { headers: authHeaders() });
    if (!res.ok) return null;
    return (await res.json()) as AiSentiment;
  } catch {
    return null;
  }
}

/** Run (and pay for) the analysis — or get the cached one back for free. */
export async function generateSentiment(sessionId: string): Promise<AiSentimentResult> {
  try {
    const res = await fetch(sentimentUrl(sessionId), {
      method: 'POST',
      headers: authHeaders(),
    });
    if (res.status === 402) {
      return { sentiment: null, insufficient: await parseInsufficient(res), error: '' };
    }
    if (!res.ok) return { sentiment: null, insufficient: null, error: await res.text() };
    return { sentiment: (await res.json()) as AiSentiment, insufficient: null, error: '' };
  } catch {
    return { sentiment: null, insufficient: null, error: '' };
  }
}

// ---- Follow-up email (REST under /api/sessions/{id}/email*) -----------------

/** A recipient as the composer sends it (mirrors the server enum). */
export type RecipientRef =
  | { kind: 'participant'; peer_id: string; cc?: boolean }
  | { kind: 'email'; email: string; cc?: boolean };

/** A recipient as the server echoes it back — never a user id or another
 *  participant's address (only raw addresses the requester typed echo). */
export type EmailRecipient =
  | { kind: 'participant'; name: string; cc: boolean }
  | { kind: 'email'; email: string; cc: boolean };

export interface AiEmail {
  /** Absent when the server delivered an unsaved draft (insert failed) — it
   *  can be read but not sent. */
  id?: string;
  status: 'draft' | 'sent' | 'failed' | string;
  subject: string;
  body_text: string;
  recipients: EmailRecipient[];
  tone?: string | null;
  guidelines?: string | null;
  lang?: string | null;
  resend_id?: string | null;
  sent_at?: string | null;
  created_at?: string;
  cost?: number;
  /** New balance after the charge; absent on GET and on free delivery. */
  balance?: number;
}

/** Generation outcome: exactly one of the three fields is meaningful. */
export interface AiEmailResult {
  email: AiEmail | null;
  insufficient: InsufficientCredits | null;
  error: string;
}

const emailUrl = (sessionId: string, tail: string) =>
  `${HTTP_BASE}/api/sessions/${encodeURIComponent(sessionId)}/${tail}`;

/** The requester's own latest draft/sent email; null when none / 403 / network. */
export async function fetchLatestEmail(sessionId: string): Promise<AiEmail | null> {
  try {
    const res = await fetch(emailUrl(sessionId, 'email'), { headers: authHeaders() });
    if (!res.ok) return null;
    return (await res.json()) as AiEmail;
  } catch {
    return null;
  }
}

/** Generate (and charge for) a follow-up email draft. */
export async function generateEmailDraft(
  sessionId: string,
  opts: {
    recipients: RecipientRef[];
    tone: string;
    guidelines: string;
    lang: string;
    includeSummary: boolean;
  },
): Promise<AiEmailResult> {
  try {
    const res = await fetch(emailUrl(sessionId, 'email-draft'), {
      method: 'POST',
      headers: { ...authHeaders(), 'Content-Type': 'application/json' },
      body: JSON.stringify({
        recipients: opts.recipients,
        tone: opts.tone,
        guidelines: opts.guidelines.trim() || null,
        lang: opts.lang,
        include_summary: opts.includeSummary,
      }),
    });
    if (res.status === 402) {
      return { email: null, insufficient: await parseInsufficient(res), error: '' };
    }
    if (!res.ok) return { email: null, insufficient: null, error: await res.text() };
    return { email: (await res.json()) as AiEmail, insufficient: null, error: '' };
  } catch {
    return { email: null, insufficient: null, error: '' };
  }
}

/** What a successful send returns. */
export interface EmailSent {
  id: string;
  status: 'sent';
  resend_id: string;
  sent_at: string;
}

/** Send outcome: `sent` on success, else the server's error text ('' = network). */
export interface EmailSendResult {
  sent: EmailSent | null;
  error: string;
}

/** Send a draft (free). Edited subject/body travel with the request. */
export async function sendEmail(
  sessionId: string,
  emailId: string,
  edits: { subject?: string; body_text?: string } = {},
): Promise<EmailSendResult> {
  try {
    const res = await fetch(emailUrl(sessionId, 'email-send'), {
      method: 'POST',
      headers: { ...authHeaders(), 'Content-Type': 'application/json' },
      body: JSON.stringify({ email_id: emailId, ...edits }),
    });
    if (!res.ok) return { sent: null, error: await res.text() };
    return { sent: (await res.json()) as EmailSent, error: '' };
  } catch {
    return { sent: null, error: '' };
  }
}

// ---- Shared error shape ------------------------------------------------------

/** The 402 body every credit-charged AI endpoint returns on insufficient funds. */
export interface InsufficientCredits {
  error: 'insufficient_credits';
  required: number;
  available: number;
  feature: string;
}

/** Parse a 402 response body, or null when it isn't the standard shape. */
export async function parseInsufficient(res: Response): Promise<InsufficientCredits | null> {
  if (res.status !== 402) return null;
  try {
    const body = (await res.json()) as InsufficientCredits;
    return body.error === 'insufficient_credits' ? body : null;
  } catch {
    return null;
  }
}

// Fetch wrappers for the advanced-feature REST endpoints (specs 0011+):
// transcript documents, AI pricing, and — in later phases — bookmarks,
// subtitles, glossaries, reports, sentiment and email. Auth/session plumbing
// stays in auth.ts; this module only talks JSON.

import { authHeaders, HTTP_BASE } from './auth';

// ---- Cartesia "Enhanced" client-direct session + voice cloning (spec 0108) --
// Mint a scoped, short-lived Cartesia access token so the browser can connect DIRECTLY to
// Cartesia STT (Ink-2) + TTS (Sonic-3.5). The raw `CARTESIA_API_KEY` never reaches the
// client — only this token + the public endpoints. Auth-gated server-side (guests get 401).

export interface EnhancedSessionResponse {
  /** Short-lived Cartesia access token (STT + TTS grants), passed as the WS query param. */
  token: string;
  /** Unix seconds at which the token expires. */
  expires_at: number;
  cartesia_version: string;
  stt: { endpoint: string; model: string };
  tts: { endpoint: string; model: string };
  voice_cloning_enabled: boolean;
  /** Optional env-configured fallback voice for speakers without a clone. */
  default_voice_id?: string | null;
}

/** Mint a fresh Enhanced (Cartesia) session. Returns null on any failure so the caller can
 *  degrade gracefully (fall back to Standard). */
export async function fetchEnhancedSession(): Promise<EnhancedSessionResponse | null> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/sessions/enhanced/session`, {
      method: 'POST',
      headers: { ...authHeaders() },
    });
    if (!res.ok) return null;
    return (await res.json()) as EnhancedSessionResponse;
  } catch {
    return null;
  }
}

/** Instant Voice Cloning result (spec 0108): `voice_id` is null when the clone failed and
 *  the call should proceed with a default voice. */
export interface CloneVoiceResponse {
  voice_id: string | null;
  fallback?: boolean;
}

/** Upload a recorded voice clip for Instant Voice Cloning. Never throws — voice prep must
 *  not block the call; transport failures resolve to a fallback shape. */
export async function cloneVoice(clip: Blob, language?: string): Promise<CloneVoiceResponse> {
  try {
    const form = new FormData();
    form.append('clip', clip, 'voice.webm');
    if (language) form.append('language', language);
    // Do NOT set Content-Type: the browser adds the multipart boundary itself.
    const res = await fetch(`${HTTP_BASE}/api/sessions/enhanced/clone-voice`, {
      method: 'POST',
      headers: { ...authHeaders() },
      body: form,
    });
    if (!res.ok) return { voice_id: null, fallback: true };
    return (await res.json()) as CloneVoiceResponse;
  } catch {
    return { voice_id: null, fallback: true };
  }
}

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
 * Pin a moment. The in-call flow (spec 0039) requires a label, so it passes the
 * typed `label` plus the client `ts` captured when 🔖 was pressed — the pin then
 * reflects the actual moment, not when the label was finished. With `ts` omitted
 * the server stamps "now".
 */
export async function addBookmark(
  sessionId: string,
  opts: { label?: string; ts?: string } = {},
): Promise<Bookmark | null> {
  try {
    const res = await fetch(
      `${HTTP_BASE}/api/sessions/${encodeURIComponent(sessionId)}/bookmarks`,
      {
        method: 'POST',
        headers: { ...authHeaders(), 'Content-Type': 'application/json' },
        body: JSON.stringify({ label: opts.label, ts: opts.ts }),
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

/** Supported upload extensions — documents only (mirrors the server's `classify_ext`).
 *  txt/md/csv/pdf/docx are text-extracted + translated; the rest are stored-only. */
export const UPLOAD_EXTS = [
  'txt', 'md', 'csv', 'log', 'pdf', 'docx', 'doc', 'odt', 'rtf', 'xlsx', 'pptx',
] as const;

/** The `accept` attribute value for the file picker (documents only). */
export const UPLOAD_ACCEPT = UPLOAD_EXTS.map((e) => `.${e}`).join(',');

/** Client-side size cap, 5 MB (must stay ≤ the server's `SUPABASE_MAX_UPLOAD_BYTES`). */
export const UPLOAD_MAX_BYTES = 5 * 1024 * 1024;

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
  /** Why the document text wasn't translated (pay-to-translate), if applicable:
   *  'signin' → not signed in, 'credits' → out of credits. Undefined otherwise. */
  translateBlocked?: 'signin' | 'credits';
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
    xhr.onload = () => {
      const ok = xhr.status >= 200 && xhr.status < 300;
      let translateBlocked: 'signin' | 'credits' | undefined;
      try {
        const b = JSON.parse(xhr.responseText)?.translate_blocked;
        if (b === 'signin' || b === 'credits') translateBlocked = b;
      } catch {
        /* non-JSON body — ignore */
      }
      resolve({ ok, status: xhr.status, translateBlocked });
    };
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
  /** Quiz generation: base + per_question (charged per language the quiz covers). */
  quiz?: { base: number; per_question: number };
  transcript_correction?: { base: number; per_event: number };
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

// ---- AI transcript correction (REST under /api/sessions/{id}/correction, 0068) ----

/** Which text the corrected export polishes — mirrors the server cache key. */
export type CorrectionMode = 'original' | 'translated' | 'both';

/** Outcome of ensuring a correction exists. `ok` means the corrected download
 *  can proceed (cache hit or a fresh, paid generation). Never throws. */
export interface CorrectionResult {
  ok: boolean;
  /** True when a cached correction was reused (no charge). */
  cached: boolean;
  /** True when this request actually charged credits. */
  charged?: boolean;
  cost?: number;
  /** New balance after a charge; absent on cache hits / free delivery. */
  balance?: number;
  /** Set when credits ran short (the standard 402 body). */
  insufficient?: { required?: number; available?: number };
  /** Set when the server rate-limited the request (429) so the UI can say so (#222). */
  rateLimited?: boolean;
}

const correctionUrl = (sessionId: string, mode: CorrectionMode, lang: string): string => {
  const qs = new URLSearchParams({ mode });
  if (mode !== 'original') qs.set('lang', lang);
  return `${HTTP_BASE}/api/sessions/${encodeURIComponent(sessionId)}/correction?${qs.toString()}`;
};

/** Whether a correction is already cached for this `(mode, lang)` — lets the UI
 *  label a repeat export as free. Null on 403 / network error. */
export async function fetchCorrectionStatus(
  sessionId: string,
  mode: CorrectionMode,
  lang: string,
): Promise<{ cached: boolean; cost?: number } | null> {
  try {
    const res = await fetch(correctionUrl(sessionId, mode, lang), { headers: authHeaders() });
    if (!res.ok) return null;
    return (await res.json()) as { cached: boolean; cost?: number };
  } catch {
    return null;
  }
}

/** Ensure a cached correction exists for this export shape, charging once. The
 *  corrected text itself is delivered by the corrected download, not here. */
export async function ensureCorrection(
  sessionId: string,
  mode: CorrectionMode,
  lang: string,
): Promise<CorrectionResult> {
  try {
    const res = await fetch(correctionUrl(sessionId, mode, lang), {
      method: 'POST',
      headers: authHeaders(),
    });
    if (res.status === 402) {
      const b = (await res.json().catch(() => ({}))) as { required?: number; available?: number };
      return { ok: false, cached: false, insufficient: { required: b.required, available: b.available } };
    }
    if (res.status === 429) return { ok: false, cached: false, rateLimited: true };
    // 202 → generation runs in the background; poll the job for the outcome.
    if (res.status === 202) {
      const { job_id: jobId } = (await res.json()) as { job_id: string };
      const job = await pollAiJob(sessionId, jobId);
      if (job.status === 'done') {
        return { ok: true, ...(job.result as Omit<CorrectionResult, 'ok'>) };
      }
      if (job.error === 'insufficient_credits') {
        const b = (job.result ?? {}) as { required?: number; available?: number };
        return { ok: false, cached: false, insufficient: { required: b.required, available: b.available } };
      }
      return { ok: false, cached: false };
    }
    if (!res.ok) return { ok: false, cached: false };
    // Cache hit (200) or backward-compatible synchronous result.
    const body = (await res.json()) as Omit<CorrectionResult, 'ok'>;
    return { ok: true, ...body };
  } catch {
    return { ok: false, cached: false };
  }
}

// ---- Quiz history (spec 0098 / #221) ----------------------------------------

/** One persisted quiz with per-participant scores, for the session-detail page. */
export interface SessionQuiz {
  id: string;
  title: string | null;
  questions: Array<{ prompt: string; options: string[]; correct_index: number }>;
  created_at: string;
  results: Array<{ peer_id: string; display_name: string; score: number; total: number }>;
}

/** Persist a finished quiz + scores (host only). Best-effort: failures (incl. a
 *  guest host with no auth) are swallowed — quiz history never disrupts the call. */
export async function saveQuizHistory(
  sessionId: string,
  summary: {
    title: string | null;
    questions: Array<{ prompt: string; options: string[]; correct_index: number }>;
    results: Array<{ peer_id: string; display_name: string; score: number; total: number }>;
  },
): Promise<boolean> {
  try {
    const res = await fetch(
      `${HTTP_BASE}/api/sessions/${encodeURIComponent(sessionId)}/quizzes`,
      {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', ...authHeaders() },
        body: JSON.stringify(summary),
      },
    );
    return res.ok;
  } catch {
    return false;
  }
}

/** The quizzes + scores stored for a session (empty on any error). */
export async function fetchSessionQuizzes(sessionId: string): Promise<SessionQuiz[]> {
  try {
    const res = await fetch(
      `${HTTP_BASE}/api/sessions/${encodeURIComponent(sessionId)}/quizzes`,
      { headers: authHeaders() },
    );
    if (!res.ok) return [];
    return (await res.json()) as SessionQuiz[];
  } catch {
    return [];
  }
}

// ---- AI quiz on demand (POST /api/quiz/generate, spec 0067 / #124) -----------

export interface GeneratedQuiz {
  // Questions localized server-side: stem + options keyed by language.
  questions: { q: Record<string, string>; options: Record<string, string[]>; answer: number }[];
  cost: number;
  balance?: number;
}

/** Result of an AI-quiz generation: `quiz` on success, else a `reason` (a typed
 *  402 so the UI can prompt a top-up, or a generic failure). Never throws. */
export interface QuizResult {
  ok: boolean;
  quiz?: GeneratedQuiz;
  reason?: 'insufficient_credits' | 'failed';
  required?: number;
  available?: number;
}

/** Generate a custom quiz from a prompt. `count` is clamped server-side. `langs`
 *  are the distinct languages in the room, so the quiz is localized for everyone. */
export async function generateAiQuiz(
  prompt: string,
  count: number,
  lang: string,
  langs: string[],
): Promise<QuizResult> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/quiz/generate`, {
      method: 'POST',
      headers: { ...authHeaders(), 'Content-Type': 'application/json' },
      body: JSON.stringify({ prompt, count, lang, langs }),
    });
    if (res.status === 402) {
      const b = (await res.json().catch(() => ({}))) as { required?: number; available?: number };
      return { ok: false, reason: 'insufficient_credits', required: b.required, available: b.available };
    }
    if (!res.ok) return { ok: false, reason: 'failed' };
    return { ok: true, quiz: (await res.json()) as GeneratedQuiz };
  } catch {
    return { ok: false, reason: 'failed' };
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
    // 202 → generation runs in the background; poll the job for the result.
    if (res.status === 202) {
      const { job_id: jobId } = (await res.json()) as { job_id: string };
      const job = await pollAiJob(sessionId, jobId);
      if (job.status === 'done') {
        return { report: job.result as AiReport, insufficient: null, error: '' };
      }
      if (job.error === 'insufficient_credits') {
        return { report: null, insufficient: asInsufficient(job.result), error: '' };
      }
      // Generic failure / timeout → '' lets the UI show its localized message.
      return { report: null, insufficient: null, error: '' };
    }
    if (!res.ok) return { report: null, insufficient: null, error: await res.text() };
    // Backward-compatible synchronous result (older server).
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
    // 202 → analysis runs in the background; poll the job for the result.
    if (res.status === 202) {
      const { job_id: jobId } = (await res.json()) as { job_id: string };
      const job = await pollAiJob(sessionId, jobId);
      if (job.status === 'done') {
        return { sentiment: job.result as AiSentiment, insufficient: null, error: '' };
      }
      if (job.error === 'insufficient_credits') {
        return { sentiment: null, insufficient: asInsufficient(job.result), error: '' };
      }
      // Generic failure / timeout → '' lets the UI show its localized message.
      return { sentiment: null, insufficient: null, error: '' };
    }
    if (!res.ok) return { sentiment: null, insufficient: null, error: await res.text() };
    // Cache hit (200) or backward-compatible synchronous result.
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
    // 202 → generation runs in the background; poll the job for the draft.
    if (res.status === 202) {
      const { job_id: jobId } = (await res.json()) as { job_id: string };
      const job = await pollAiJob(sessionId, jobId);
      if (job.status === 'done') {
        return { email: job.result as AiEmail, insufficient: null, error: '' };
      }
      if (job.error === 'insufficient_credits') {
        return { email: null, insufficient: asInsufficient(job.result), error: '' };
      }
      // Generic failure / timeout → '' lets the UI show its localized message.
      return { email: null, insufficient: null, error: '' };
    }
    if (!res.ok) return { email: null, insufficient: null, error: await res.text() };
    // Backward-compatible synchronous result (older server).
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

/** Narrow an arbitrary value to the standard insufficient-credits body. Used to
 *  read the 402 payload an async AI job carries when its charge fails. */
function asInsufficient(v: unknown): InsufficientCredits | null {
  const b = v as Partial<InsufficientCredits> | null;
  return b != null && b.error === 'insufficient_credits' ? (b as InsufficientCredits) : null;
}

// ---- Async AI jobs (report / correction / email draft) ----------------------
// These features fan out into many model calls; on a multi-hour transcript the
// work runs longer than the upstream proxy's request ceiling, so the POST
// returns `202 { job_id }` and generation runs server-side. The client polls
// the job below until it is `done` (result attached) or `failed`. Each poll is a
// quick request, so no single connection is held open long enough to time out.

/** Job status as `GET /api/sessions/{id}/ai-job/{job_id}` serves it. */
interface AiJobState {
  status: 'pending' | 'done' | 'failed' | string;
  error?: string | null;
  /** The body the synchronous endpoint used to return (present on `done`; the
   *  standard 402 body on an `insufficient_credits` failure). */
  result?: unknown;
}

const sleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * Poll a background AI job until it is `done` or `failed`. Resolves to the
 * terminal job, or `{ status: 'failed', error: 'timeout' }` once a generous
 * deadline passes. Transient poll errors (a not-yet-visible row → 404, a 5xx
 * hiccup, a network blip) are ignored — only a terminal status or the deadline
 * stops the loop.
 */
async function pollAiJob(sessionId: string, jobId: string): Promise<AiJobState> {
  const url = `${HTTP_BASE}/api/sessions/${encodeURIComponent(sessionId)}/ai-job/${encodeURIComponent(jobId)}`;
  const deadline = Date.now() + 10 * 60 * 1000; // 10 min — covers multi-hour calls
  let delay = 1500;
  while (Date.now() < deadline) {
    await sleep(delay);
    try {
      const res = await fetch(url, { headers: authHeaders(), cache: 'no-store' });
      if (res.ok) {
        const job = (await res.json()) as AiJobState;
        if (job.status === 'done' || job.status === 'failed') return job;
      }
    } catch {
      // network blip — keep polling until the deadline
    }
    delay = Math.min(delay + 500, 4000); // gentle backoff, cap 4s
  }
  return { status: 'failed', error: 'timeout' };
}

// ---- User bug report (spec 0071) -------------------------------------------
/** Submit a user bug report. Works for guests + signed-in users (the auth header,
 *  when present, attributes it). Returns true on success. */
export async function postBugReport(message: string, pageUrl: string): Promise<boolean> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/bug-report`, {
      method: 'POST',
      headers: { ...authHeaders(), 'Content-Type': 'application/json' },
      body: JSON.stringify({ message, page_url: pageUrl }),
    });
    return res.ok;
  } catch {
    return false;
  }
}

// ---- In-call invite emails (spec 0082) -------------------------------------

export interface InviteResult {
  /** Number of invites Resend accepted. */
  sent: number;
  /** Number that failed to send (partial success is possible). */
  failed: number;
  /** A user-facing error message when the whole request was rejected. */
  error?: string;
}

/** Email a join link to people the sender knows. Auth-only (the server 401s
 *  guests); the server builds the link from its own origin, so we never send a
 *  URL. Returns counts, or `{ error }` with the server's message on failure. */
export async function sendInvites(
  room: string,
  emails: string[],
  lang: string,
): Promise<InviteResult> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/rooms/${encodeURIComponent(room)}/invite`, {
      method: 'POST',
      headers: { ...authHeaders(), 'Content-Type': 'application/json' },
      body: JSON.stringify({ emails, lang }),
    });
    if (!res.ok) {
      const msg = (await res.text().catch(() => '')) || `error ${res.status}`;
      return { sent: 0, failed: emails.length, error: msg };
    }
    const body = (await res.json()) as { sent?: number; failed?: number };
    return { sent: body.sent ?? 0, failed: body.failed ?? 0 };
  } catch {
    return { sent: 0, failed: emails.length, error: 'network' };
  }
}

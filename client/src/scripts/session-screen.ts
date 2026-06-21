// Session detail screen (specs 0011+): full-screen view opened after a call
// ends and from the buy-modal Transcripts tab. Hosts the download buttons, the
// AI tool sections (report/sentiment/email — filled in by later phases), and a
// read-only transcript viewer. Auth-only: guests never reach this screen
// (transcript APIs are auth-gated server-side).

import * as auth from './auth';
import {
  ensureCorrection,
  fetchAiPricing,
  fetchCorrectionStatus,
  fetchSessionQuizzes,
  fetchTranscript,
  type CorrectionMode,
  type SessionQuiz,
  type TranscriptDoc,
} from './api';
import { getUiLang, t } from './i18n';
import { initEmailSlot, updateEmailContext } from './email';
import { initReportSlot } from './report';
import { initSentimentSlot, updateSentimentContext } from './sentiment';

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;

/** What the screen needs to paint its header before the transcript loads. */
export interface SessionRef {
  id: string;
  room: string;
  started_at: string;
  ended_at?: string | null;
  event_count: number;
}

let current: SessionRef | null = null;
let onCloseCb: (() => void) | null = null;

/** The session currently shown (null when the screen is closed). */
export function currentSession(): SessionRef | null {
  return current;
}

export function openSessionScreen(ref: SessionRef, opts: { onClose?: () => void } = {}): void {
  current = ref;
  onCloseCb = opts.onClose ?? null;
  renderHeader(ref);
  $('home').classList.add('hidden');
  $('session').classList.remove('hidden');
  initReportSlot(ref);
  initSentimentSlot(ref);
  initEmailSlot(ref);
  void renderTranscript(ref);
  $('session-back').focus();
}

export function closeSessionScreen(): void {
  current = null;
  $('session').classList.add('hidden');
  $('home').classList.remove('hidden');
  const cb = onCloseCb;
  onCloseCb = null;
  cb?.();
}

function renderHeader(ref: SessionRef): void {
  $('session-room').textContent = ref.room;
  $('session-date').textContent = new Date(ref.started_at).toLocaleString();
  const ms = ref.ended_at
    ? new Date(ref.ended_at).getTime() - new Date(ref.started_at).getTime()
    : 0;
  $('session-duration').textContent = formatDuration(ms);
  $('session-events').textContent = String(ref.event_count);
  $('session-participants').textContent = '';
  for (const id of ['session-dl-pdf', 'session-dl-json', 'session-dl-srt', 'session-dl-vtt']) {
    const btn = $<HTMLButtonElement>(id);
    btn.disabled = ref.event_count === 0;
    btn.title = ref.event_count === 0 ? t('noTranscriptEvents') : '';
  }
  // AI text correction checkbox + cost estimate
  initAiCorrect(ref);
}

// Client fallback until the server pricing arrives (mirrors the 0068 defaults).
const FALLBACK_CORRECT_COST = { base: 0.05, per_event: 0.001 };
let correctPricing = FALLBACK_CORRECT_COST;

/** Correction mode + cache lang for the *currently selected* sub-mode — the
 *  visible control next to the checkbox. PDF/JSON downloads override this at
 *  click time (see `correctionTargetFor`); the server cost is authoritative. */
function correctTargetFromDropdown(): { mode: CorrectionMode; lang: string } {
  const mode = $<HTMLSelectElement>('session-sub-mode').value as CorrectionMode;
  return { mode, lang: mode === 'original' ? '' : getUiLang() };
}

/** Repaint the cost label: an estimate by size × mode, or "free" when this
 *  exact (mode, lang) is already cached. */
function refreshCorrectCost(): void {
  const ref = current;
  if (!ref || ref.event_count === 0) return;
  const costEl = $('ai-correct-cost');
  const { mode, lang } = correctTargetFromDropdown();
  const passes = mode === 'both' ? 2 : 1;
  const estimated =
    correctPricing.base + correctPricing.per_event * Math.max(1, ref.event_count) * passes;
  costEl.textContent = `~${auth.formatCredits(estimated)}`;
  // Already paid for this shape → say so (don't double-charge expectations).
  void fetchCorrectionStatus(ref.id, mode, lang).then((s) => {
    const now = correctTargetFromDropdown();
    if (current?.id !== ref.id || now.mode !== mode || now.lang !== lang) return;
    if (s?.cached) costEl.textContent = t('aiCorrectFree');
  });
}

function initAiCorrect(ref: SessionRef): void {
  const chk = $<HTMLInputElement>('ai-correct-chk');
  const costEl = $('ai-correct-cost');
  if (ref.event_count === 0) {
    chk.disabled = true;
    chk.checked = false;
    costEl.textContent = '';
    return;
  }
  chk.disabled = false;
  void fetchAiPricing().then((p) => {
    if (current?.id !== ref.id) return;
    if (p?.transcript_correction) correctPricing = p.transcript_correction;
    refreshCorrectCost();
  });
  refreshCorrectCost(); // immediate, with the fallback price
}

function formatDuration(ms: number): string {
  const total = Math.max(0, Math.round(ms / 1000));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  return h > 0
    ? `${h}h ${String(m).padStart(2, '0')}m`
    : `${m}m ${String(s).padStart(2, '0')}s`;
}

async function renderTranscript(ref: SessionRef): Promise<void> {
  const list = $('session-transcript');
  const status = $('session-transcript-status');
  list.innerHTML = '';
  syncTranscriptPreview(0); // reset the toggle/collapse state for the new session (#224)
  void renderQuizzes(ref.id); // quiz history loads independently of the transcript (#221)
  $('session-bookmarks').hidden = true;
  if (ref.event_count === 0) {
    status.textContent = t('noTranscriptEvents');
    return;
  }
  status.textContent = t('processing');
  const doc = await fetchTranscript(ref.id);
  if (current?.id !== ref.id) return; // navigated away while loading
  if (!doc) {
    status.textContent = t('loadFailed');
    return;
  }
  status.textContent = '';
  // The export is authoritative — refresh duration + participants from it. Collapse
  // duplicates first: rejoining/refreshing mints a new peer_id, so the same person
  // can appear several times in the roster (and inflate the count the sentiment
  // estimate is billed on). Dedupe by display name, keeping the first occurrence.
  const seen = new Set<string>();
  const participants = doc.session.participants.filter((p) => {
    const key = (p.name || '').trim().toLowerCase();
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
  $('session-duration').textContent = formatDuration(doc.session.duration_seconds * 1000);
  $('session-participants').textContent = participants.map((p) => p.name).join(', ');
  // Sentiment cost preview needs the participant count; the chart wants
  // bookmark offsets (seconds from session start).
  const startMs = new Date(doc.session.started_at).getTime();
  updateSentimentContext(
    ref.id,
    participants.length,
    doc.session.duration_seconds,
    (doc.bookmarks ?? []).map((bm) => Math.max(0, (new Date(bm.ts).getTime() - startMs) / 1000)),
  );
  // The email composer's To-chips come from the same (deduped) roster.
  updateEmailContext(
    ref.id,
    participants.map((p) => ({ id: p.id, name: p.name })),
  );
  renderBookmarks(doc);
  renderEvents(list, doc);
}

/** Quiz history (#221): render the quizzes run in the call + per-participant
 *  scores. Hidden when the session has none. */
async function renderQuizzes(sessionId: string): Promise<void> {
  const box = $('session-quizzes');
  const list = $('session-quizzes-list');
  box.hidden = true;
  list.innerHTML = '';
  const quizzes = await fetchSessionQuizzes(sessionId);
  if (current?.id !== sessionId || quizzes.length === 0) return; // navigated away / none
  for (const q of quizzes) list.appendChild(renderQuizCard(q));
  box.hidden = false;
}

function renderQuizCard(q: SessionQuiz): HTMLElement {
  const card = document.createElement('div');
  card.className = 'quiz-card';

  const head = document.createElement('div');
  head.className = 'quiz-card-head';
  const title = document.createElement('span');
  title.className = 'quiz-card-title';
  title.textContent = q.title || t('quizzesLabel');
  const meta = document.createElement('span');
  meta.className = 'quiz-card-meta';
  meta.textContent = `${q.questions.length} · ${new Date(q.created_at).toLocaleDateString()}`;
  head.append(title, meta);
  card.appendChild(head);

  // Server returns results best-score-first.
  const scores = document.createElement('div');
  scores.className = 'quiz-scores';
  for (const r of q.results) {
    const row = document.createElement('div');
    row.className = 'quiz-score-row';
    const name = document.createElement('span');
    name.textContent = r.display_name; // textContent: participant name is attacker-controlled
    const val = document.createElement('span');
    val.className = 'quiz-score-val mono';
    val.textContent = `${r.score}/${r.total}`;
    row.append(name, val);
    scores.appendChild(row);
  }
  card.appendChild(scores);

  if (q.results.length > 0) {
    const summary = document.createElement('div');
    summary.className = 'quiz-card-summary';
    const best = q.results[0];
    const avg = q.results.reduce((a, r) => a + r.score, 0) / q.results.length;
    summary.textContent = `🏆 ${best.display_name} · ${t('quizAverage')}: ${avg.toFixed(1)}`;
    card.appendChild(summary);
  }
  return card;
}

/** Pinned moments (spec 0013) above the transcript; hidden when none exist. */
function renderBookmarks(doc: TranscriptDoc): void {
  const box = $('session-bookmarks');
  box.innerHTML = '';
  const pins = doc.bookmarks ?? [];
  box.hidden = pins.length === 0;
  if (!pins.length) return;
  const head = document.createElement('div');
  head.className = 'bm-head';
  head.textContent = `🔖 ${t('bookmarksTitle')}`;
  box.appendChild(head);
  for (const bm of pins) {
    const row = document.createElement('div');
    row.className = 'bm-item';
    const time = document.createElement('span');
    time.className = 'bm-time mono';
    time.textContent = new Date(bm.ts).toLocaleTimeString([], {
      hour: '2-digit',
      minute: '2-digit',
      second: '2-digit',
    });
    const by = document.createElement('span');
    by.className = 'bm-by';
    by.textContent = bm.by;
    row.append(time, by);
    if (bm.label) {
      const label = document.createElement('span');
      label.className = 'bm-label';
      label.textContent = bm.label;
      row.appendChild(label);
    }
    box.appendChild(row);
  }
}

function renderEvents(list: HTMLElement, doc: TranscriptDoc): void {
  const ui = getUiLang();
  for (const ev of doc.events) {
    const row = document.createElement('div');
    row.className = ev.type === 'chat' ? 'tr-event tr-chat' : 'tr-event';
    // Epoch ms, so sentiment key moments can jump to the closest row.
    row.dataset.ts = String(new Date(ev.ts).getTime());
    const time = document.createElement('span');
    time.className = 'tr-time mono';
    time.textContent = new Date(ev.ts).toLocaleTimeString([], {
      hour: '2-digit',
      minute: '2-digit',
      second: '2-digit',
    });
    const body = document.createElement('div');
    body.className = 'tr-body';
    const name = document.createElement('span');
    name.className = 'tr-speaker';
    name.textContent = ev.type === 'chat' ? `${ev.speaker_name} 💬` : ev.speaker_name;
    const text = document.createElement('span');
    text.className = 'tr-text';
    // Show the viewer's language when a translation exists; original below it.
    const translated = ev.lang !== ui ? ev.translations?.[ui] : undefined;
    text.textContent = translated || ev.original;
    body.append(name, text);
    if (translated) {
      const orig = document.createElement('span');
      orig.className = 'tr-orig';
      orig.textContent = ev.original;
      body.appendChild(orig);
    }
    row.append(time, body);
    list.appendChild(row);
  }
  syncTranscriptPreview(doc.events.length);
}

// Transcript preview (#224): collapse to the first few rows by default, with a
// toggle to expand/collapse. Keeps the unified outputs card compact.
const PREVIEW_ROWS = 6;

/** Show the toggle only when there's more than the preview shows, and reset to the
 *  collapsed state on each (re)load. */
function syncTranscriptPreview(count: number): void {
  const list = $('session-transcript');
  const toggle = $('session-transcript-toggle');
  const collapsible = count > PREVIEW_ROWS;
  list.classList.toggle('collapsed', collapsible);
  toggle.classList.toggle('hidden', !collapsible);
  toggle.textContent = t('viewFullTranscript');
  toggle.setAttribute('aria-expanded', 'false');
}

// ---- One-time wiring (DOM is ready when modules execute) --------------------

$('session-back').addEventListener('click', closeSessionScreen);

// Expand/collapse the transcript preview (#224).
$('session-transcript-toggle').addEventListener('click', () => {
  const list = $('session-transcript');
  const toggle = $('session-transcript-toggle');
  const collapsed = list.classList.toggle('collapsed');
  toggle.textContent = collapsed ? t('viewFullTranscript') : t('hideTranscript');
  toggle.setAttribute('aria-expanded', String(!collapsed));
});

/** Show a transient status line that auto-clears (only if unchanged). */
function flashStatus(msg: string): void {
  const status = $('session-transcript-status');
  status.textContent = msg;
  setTimeout(() => {
    if (status.textContent === msg) status.textContent = '';
  }, 4000);
}

/** What the corrected export polishes, per format (spec 0068): JSON → source
 *  text only; PDF → both (original + shown translation); SRT/VTT → the dropdown. */
function correctionTargetFor(format: 'pdf' | 'json' | 'srt' | 'vtt'): {
  mode: CorrectionMode;
  lang: string;
} {
  if (format === 'json') return { mode: 'original', lang: '' };
  if (format === 'pdf') return { mode: 'both', lang: getUiLang() };
  return correctTargetFromDropdown(); // srt/vtt follow the sub-mode dropdown
}

// Serialize downloads (#222): only one runs at a time, so rapid clicks across the
// four format buttons can't fan out into a burst the server rate-limits (429), and
// a stuck-disabled "frozen" button can't happen (the finally always restores).
const DL_FORMATS = ['pdf', 'json', 'srt', 'vtt'] as const;
let downloading = false;

/** Disable every download button while one is in flight; when freeing them, a
 *  session with no events stays disabled. */
function setDownloadsBusy(busy: boolean): void {
  const noEvents = (current?.event_count ?? 0) === 0;
  for (const f of DL_FORMATS) $<HTMLButtonElement>(`session-dl-${f}`).disabled = busy || noEvents;
}

for (const format of DL_FORMATS) {
  const btn = $<HTMLButtonElement>(`session-dl-${format}`);
  btn.addEventListener('click', async () => {
    if (!current || btn.disabled || downloading) return;
    const sessionId = current.id;
    const prev = btn.textContent;
    downloading = true;
    setDownloadsBusy(true);
    btn.classList.add('btn-loading');
    try {
      // SRT/VTT also carry the lang-mode dropdown (original/translated/both).
      const mode = $<HTMLSelectElement>('session-sub-mode').value as auth.SubtitleMode;
      const correct = $<HTMLInputElement>('ai-correct-chk').checked;

      // Paid step first: ensure the corrected text is cached. It's free on a
      // repeat (server-authoritative), so re-downloading never re-charges or
      // re-runs the AI correction (#222).
      if (correct) {
        const target = correctionTargetFor(format);
        btn.textContent = t('aiCorrecting');
        const res = await ensureCorrection(sessionId, target.mode, target.lang);
        if (current?.id !== sessionId) return; // navigated away while correcting
        if (!res.ok) {
          flashStatus(
            res.rateLimited
              ? t('downloadRateLimited')
              : res.insufficient
                ? t('aiCorrectNoCredits')
                : t('aiCorrectFailed'),
          );
          return;
        }
        if (typeof res.balance === 'number') auth.setBalance(res.balance);
        refreshCorrectCost(); // now reads as free for this shape
      }

      btn.textContent = t('processing');
      const r = await auth.downloadTranscript(sessionId, format, getUiLang(), mode, correct);
      if (!r.ok) flashStatus(r.status === 429 ? t('downloadRateLimited') : t('downloadFailed'));
    } finally {
      btn.textContent = prev;
      btn.classList.remove('btn-loading');
      downloading = false;
      setDownloadsBusy(false); // re-enable (a no-event session stays disabled)
    }
  });
}

// Re-price the estimate (and re-check "free") when the lang-mode changes.
$('session-sub-mode').addEventListener('change', refreshCorrectCost);
